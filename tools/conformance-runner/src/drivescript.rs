//! Parsing of the `mode: drive` header directives into a [`DriveScript`] (implementation-plan
//! §4.3). The micro-grammar (normative in `conformance/README.md`; this is its reference parser):
//!
//! ```text
//! #! break: [<canonical>] <line>     # a breakpoint (E§8.6); canonical defaults to `main`
//! #! raise-trap: on                  # enable raise-trapping (E§8.7)
//! #! obs: subexpr | statement        # observation mode (E§8.8, S-62); default statement
//! #! do: run | continue | step | into | over | out    # a driving directive (E§7.3)
//! #! resolve: <value>                # resolve the suspended capability with a value (E§7.5)
//! #! resolve-raise: "<msg>"          # raise <msg> at the suspended capability's call site
//! #! expect: <stop>                  # the stop the step must produce
//! #! stack: <elem>, <elem>, …        # optional stack shape at the stop, innermost first
//! ```
//!
//! where a `<stop>` is `completed`, `paused <reason> @ L:C`, `raised <substring> @ L:C`,
//! `suspended <capability> @ L:C` (a capability request, `Outcome::Suspended`), `import <path> @
//! L:C` (an import, `Outcome::SuspendedImport`), or `faulted <kind>`; a `<reason>` is one of
//! `step`/`breakpoint`/`host-pause`/`raise-trap`/`slice-end`; a `<value>` is a string (`"…"`),
//! integer, float, `true`/`false`, or `nil`; and a stack `<elem>` is `L`, `name@L`, or `name@L×N`
//! (tail-iteration count `N`; `x` accepted for `×`). `do:`/`resolve:`/`resolve-raise:` each start a
//! step; setup directives (`break`/`raise-trap`/`obs`) must precede the first one.

use crate::directive::parse_positioned;
use crate::model::{DriveAction, DriveScript, DriveStep, ScriptValue, StackElem, StopAssertion};

/// Parses the raw drive directives (key, value), in header order, into a [`DriveScript`].
pub(crate) fn parse(raw: &[(String, String)]) -> Result<DriveScript, String> {
    let mut breakpoints = Vec::new();
    let mut raise_trap = false;
    let mut subexpr = false;
    let mut steps: Vec<DriveStep> = Vec::new();
    let mut seen_do = false;
    let mut building: Option<Building> = None;

    for (key, value) in raw {
        match key.as_str() {
            "break" | "raise-trap" | "obs" if seen_do => {
                return Err(format!(
                    "`#! {key}:` (setup) must come before the first `#! do:`"
                ));
            }
            "break" => breakpoints.push(parse_break(value)?),
            "raise-trap" => raise_trap = parse_on(value)?,
            "obs" => subexpr = parse_obs(value)?,
            "do" | "resolve" | "resolve-raise" => {
                if let Some(prev) = building.take() {
                    steps.push(prev.finish()?);
                }
                seen_do = true;
                let action = match key.as_str() {
                    "resolve" => DriveAction::Resolve(parse_script_value(value)?),
                    "resolve-raise" => DriveAction::ResolveRaise(parse_string_literal(value)?),
                    _ => parse_action(value)?,
                };
                building = Some(Building::new(action));
            }
            "expect" => {
                let step = building
                    .as_mut()
                    .ok_or("`#! expect:` with no preceding `#! do:`")?;
                if step.expect.is_some() {
                    return Err("two `#! expect:` for one `#! do:`".to_string());
                }
                step.expect = Some(parse_stop(value)?);
            }
            "stack" => {
                let step = building
                    .as_mut()
                    .ok_or("`#! stack:` with no preceding `#! do:`")?;
                if step.expect.is_none() {
                    return Err("`#! stack:` must follow this step's `#! expect:`".to_string());
                }
                if step.stack.is_some() {
                    return Err("two `#! stack:` for one `#! do:`".to_string());
                }
                step.stack = Some(parse_stack(value)?);
            }
            other => return Err(format!("unknown drive directive `#! {other}:`")),
        }
    }
    if let Some(last) = building.take() {
        steps.push(last.finish()?);
    }
    if steps.is_empty() {
        return Err("a `mode: drive` fixture needs at least one `#! do:` step".to_string());
    }
    Ok(DriveScript {
        breakpoints,
        raise_trap,
        subexpr,
        steps,
    })
}

/// A drive step under construction — its `do:` action plus the `expect:`/`stack:` it accrues.
struct Building {
    action: DriveAction,
    expect: Option<StopAssertion>,
    stack: Option<Vec<StackElem>>,
}

impl Building {
    fn new(action: DriveAction) -> Self {
        Building {
            action,
            expect: None,
            stack: None,
        }
    }

    /// A finished [`DriveStep`], or an error if the `do:` never got its `expect:`.
    fn finish(self) -> Result<DriveStep, String> {
        let expect = self
            .expect
            .ok_or("`#! do:` with no `#! expect:` for its stop")?;
        Ok(DriveStep {
            action: self.action,
            expect,
            stack: self.stack,
        })
    }
}

/// `[<canonical>] <line>` → `(canonical, line)`; the canonical id defaults to `main` (E§3.2).
fn parse_break(value: &str) -> Result<(String, u32), String> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    let (canonical, line) = match parts.as_slice() {
        [line] => ("main", *line),
        [canonical, line] => (*canonical, *line),
        _ => {
            return Err(format!(
                "`break:` expects `[<canonical>] <line>`, got `{value}`"
            ));
        }
    };
    let line: u32 = line
        .parse()
        .map_err(|_| format!("bad breakpoint line in `{value}`"))?;
    if line == 0 {
        return Err(format!("breakpoint lines are 1-based; got 0 in `{value}`"));
    }
    Ok((canonical.to_string(), line))
}

/// `on` → true (the only accepted `raise-trap:` value — it exists to turn trapping on).
fn parse_on(value: &str) -> Result<bool, String> {
    match value {
        "on" => Ok(true),
        other => Err(format!("`raise-trap:` expects `on`, got `{other}`")),
    }
}

/// `subexpr` → fine mode; `statement` → coarse (the default).
fn parse_obs(value: &str) -> Result<bool, String> {
    match value {
        "subexpr" => Ok(true),
        "statement" => Ok(false),
        other => Err(format!(
            "`obs:` expects `subexpr` or `statement`, got `{other}`"
        )),
    }
}

/// A driving directive keyword → [`DriveAction`].
fn parse_action(value: &str) -> Result<DriveAction, String> {
    match value {
        "run" => Ok(DriveAction::Run),
        "continue" => Ok(DriveAction::Continue),
        "step" => Ok(DriveAction::Step),
        "into" => Ok(DriveAction::Into),
        "over" => Ok(DriveAction::Over),
        "out" => Ok(DriveAction::Out),
        other => Err(format!(
            "`do:` expects run/continue/step/into/over/out, got `{other}`"
        )),
    }
}

/// A scripted primitive value (`resolve:`/`input:`): a string (`"…"`), integer, float (`has a .`),
/// `true`/`false`, or `nil`. Shared by the drive `resolve:` step and the run-mode `input:` queue.
pub(crate) fn parse_script_value(value: &str) -> Result<ScriptValue, String> {
    let value = value.trim();
    match value {
        "true" => return Ok(ScriptValue::Bool(true)),
        "false" => return Ok(ScriptValue::Bool(false)),
        "nil" => return Ok(ScriptValue::Nil),
        _ => {}
    }
    if value.starts_with('"') {
        return Ok(ScriptValue::Str(parse_string_literal(value)?));
    }
    if value.contains('.') {
        if let Ok(f) = value.parse::<f64>() {
            return Ok(ScriptValue::Float(f));
        }
    } else if let Ok(n) = value.parse::<i64>() {
        return Ok(ScriptValue::Int(n));
    }
    Err(format!(
        "bad scripted value `{value}` (expected a \"string\", integer, float, true/false, or nil)"
    ))
}

/// The contents of a `"…"` string literal (no escapes beyond the delimiters — conformance scripts
/// keep values plain). Errors if `value` is not a single double-quoted string.
pub(crate) fn parse_string_literal(value: &str) -> Result<String, String> {
    let value = value.trim();
    let inner = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .filter(|inner| !inner.contains('"'));
    inner
        .map(str::to_string)
        .ok_or_else(|| format!("expected a double-quoted string, got `{value}`"))
}

/// A `<stop>` assertion.
fn parse_stop(value: &str) -> Result<StopAssertion, String> {
    let (head, rest) = value.split_once(char::is_whitespace).unwrap_or((value, ""));
    let rest = rest.trim();
    match head {
        "completed" => Ok(StopAssertion::Completed),
        "paused" => {
            let (reason, pos) = parse_positioned(rest)?;
            validate_reason(&reason)?;
            Ok(StopAssertion::Paused { reason, pos })
        }
        "raised" => {
            let (substring, pos) = parse_positioned(rest)?;
            Ok(StopAssertion::Raised { substring, pos })
        }
        "suspended" => {
            let (capability, pos) = parse_positioned(rest)?;
            Ok(StopAssertion::Suspended { capability, pos })
        }
        "import" => {
            let (path, pos) = parse_positioned(rest)?;
            Ok(StopAssertion::Import { path, pos })
        }
        "faulted" => {
            if rest.is_empty() {
                return Err("`faulted` expects a fault kind".to_string());
            }
            Ok(StopAssertion::Faulted {
                kind: rest.to_string(),
            })
        }
        other => Err(format!("unknown stop `{other}` in `expect: {value}`")),
    }
}

/// The pause reasons a `paused` stop may name (E§7.2 `PauseReason`).
fn validate_reason(reason: &str) -> Result<(), String> {
    match reason {
        "step" | "breakpoint" | "host-pause" | "raise-trap" | "slice-end" => Ok(()),
        other => Err(format!(
            "unknown pause reason `{other}` (step/breakpoint/host-pause/raise-trap/slice-end)"
        )),
    }
}

/// A comma-separated stack shape into its [`StackElem`]s, innermost frame first.
fn parse_stack(value: &str) -> Result<Vec<StackElem>, String> {
    value
        .split(',')
        .map(|e| parse_stack_elem(e.trim()))
        .collect()
}

/// One stack element: `L`, `name@L`, or `name@L×N` (`x` accepted for `×`).
fn parse_stack_elem(elem: &str) -> Result<StackElem, String> {
    if elem.is_empty() {
        return Err("empty stack element".to_string());
    }
    let (name, rest) = match elem.split_once('@') {
        Some((name, rest)) => (Some(name.trim().to_string()), rest.trim()),
        None => (None, elem),
    };
    // The tail-iteration count, if any: `L×N` (or `LxN`).
    let rest = rest.replace('×', "x");
    let (line_str, tail) = match rest.split_once('x') {
        Some((line, tail)) => {
            let tail: u64 = tail
                .trim()
                .parse()
                .map_err(|_| format!("bad tail count in stack element `{elem}`"))?;
            (line, Some(tail))
        }
        None => (rest.as_str(), None),
    };
    let line: u32 = line_str
        .trim()
        .parse()
        .map_err(|_| format!("bad line in stack element `{elem}`"))?;
    if line == 0 {
        return Err(format!("stack lines are 1-based; got 0 in `{elem}`"));
    }
    Ok(StackElem { name, line, tail })
}
