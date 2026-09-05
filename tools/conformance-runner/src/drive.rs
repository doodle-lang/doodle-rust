//! The `mode: drive` executor (implementation-plan §4.3): load the program, apply the script's
//! debug setup, then drive each step and compare the resulting outcome/position/stack **transcript**
//! against the declared one. The whole ordered sequence is checked — every `do:` has an `expect:`,
//! none is spot-checked — so a drive fixture is cross-surface determinism evidence for E§8.

use crate::capability::{capability_name, registry};
use crate::matcher::{build_resolution, error_messages};
use crate::model::{
    DriveAction, DriveScript, DriveStep, ScriptResponse, StackElem, StopAssertion, Test,
};
use doodle_core::drive::ObservationMode;
use doodle_core::drive::{
    Directive, EngineFault, ImportResolution, LimitKind, Limits, Outcome, PauseReason,
    resolve as resolve_capability, resolve_import, run,
};
use doodle_core::machine::{Instance, InstanceState};
use doodle_core::parse::parse_program;
use doodle_core::resolve::resolve;
use doodle_core::source::{LineIndex, Position, normalize};
use doodle_core::span::ModuleId;
use std::path::Path;

/// Runs a `mode: drive` fixture. Loads the program (a load error is a FAIL, not part of the
/// transcript), applies the setup, then checks every step's stop; every mismatch is reported.
pub(crate) fn run_drive(
    test: &Test,
    source: &str,
    modules_dir: Option<&Path>,
) -> Result<(), Vec<String>> {
    let script = test
        .drive
        .as_ref()
        .expect("a `mode: drive` test carries a parsed drive script");
    let nfc = normalize(source);
    let index = LineIndex::new(nfc.as_ref());

    let parsed = parse_program(nfc.as_ref(), ModuleId(0));
    let parse_errors = error_messages(&parsed.diagnostics);
    if !parse_errors.is_empty() {
        return Err(parse_errors);
    }
    let resolved = resolve(parsed.ast, parsed.root, ModuleId(0));
    let resolve_errors = error_messages(&resolved.diagnostics);
    if !resolve_errors.is_empty() {
        return Err(resolve_errors);
    }

    // The entry id is `main` — the id a drive fixture's `break:` defaults to (drivescript.rs), so
    // its breakpoints bind to this module (E§3.2).
    let mut instance = Instance::load(resolved.module, Limits::default(), registry(), "main");
    apply_setup(&mut instance, script);

    let mut reasons = Vec::new();
    for (i, step) in script.steps.iter().enumerate() {
        let outcome = match drive_action(&mut instance, &step.action, &step.expect, modules_dir) {
            Ok(outcome) => outcome,
            Err(reason) => {
                reasons.push(format!("step {}: {reason}", i + 1));
                break;
            }
        };
        if let Err(reason) = check_step(&mut instance, &outcome, step, nfc.as_ref(), &index) {
            reasons.push(format!("step {}: {reason}", i + 1));
        }
    }
    if reasons.is_empty() {
        Ok(())
    } else {
        Err(reasons)
    }
}

/// Applies the script's debug setup to a freshly loaded instance (E§8.6/§8.7/§8.8).
fn apply_setup(instance: &mut Instance, script: &DriveScript) {
    for (canonical, line) in &script.breakpoints {
        instance.set_breakpoint(canonical, *line);
    }
    if script.raise_trap {
        instance.set_raise_trapping(true);
    }
    if script.subexpr {
        instance.set_observation_mode(ObservationMode::Subexpression);
    }
}

/// Drives one action to its stop (E§7.3). A `do:` directive drives the machine; a `resolve:`/
/// `resolve-raise:` step fulfils the capability the previous step suspended on (E§7.5). Any `import`
/// is transparently resolved the same way a `run` fixture does (sibling module file, else
/// `NotFound`) so imports need no explicit step — **unless** this step asserts an `import` stop, in
/// which case the suspension is left for [`check_step`] to inspect. A **capability** suspension is
/// never auto-resolved: it is a stop the fixture asserts and then a `resolve:` step resumes.
fn drive_action(
    instance: &mut Instance,
    action: &DriveAction,
    expect: &StopAssertion,
    modules_dir: Option<&Path>,
) -> Result<Outcome, String> {
    // A terminal instance is not re-drivable (E§3.3): a step after the program has finished is a
    // mis-authored fixture — report it, rather than tripping the engine's debug assertion.
    if matches!(
        instance.state(),
        InstanceState::Completed | InstanceState::Raised | InstanceState::Faulted
    ) {
        return Err(format!(
            "cannot drive a {:?} (terminal) instance — a step runs past the program's end",
            instance.state()
        ));
    }
    let mut outcome = match action {
        DriveAction::Run => run(instance, Directive::RunToCompletion),
        DriveAction::Continue => run(instance, Directive::Continue),
        DriveAction::Step => run(instance, Directive::Step),
        DriveAction::Into => run(instance, Directive::StepInto),
        DriveAction::Over => run(instance, Directive::StepOver),
        DriveAction::Out => run(instance, Directive::StepOut),
        DriveAction::Resolve(value) => {
            let resolution = build_resolution(instance, &ScriptResponse::Value(value.clone()))?;
            resolve_capability(instance, resolution)
        }
        DriveAction::ResolveRaise(message) => {
            let resolution = build_resolution(instance, &ScriptResponse::Raise(message.clone()))?;
            resolve_capability(instance, resolution)
        }
    };
    // Leave an import suspension in place when the step asserts an `import` stop; otherwise resolve
    // imports transparently. (A capability suspension is never auto-resolved here.)
    let assert_import = matches!(expect, StopAssertion::Import { .. });
    if !assert_import {
        while let Outcome::SuspendedImport(req) = &outcome {
            let joined = req
                .path
                .iter()
                .map(String::as_str)
                .collect::<Vec<&str>>()
                .join("/");
            let file = modules_dir.map(|d| d.join(&joined).with_extension("doodle"));
            let resolution = match file {
                Some(path) if path.is_file() => match std::fs::read_to_string(&path) {
                    Ok(text) => ImportResolution::Source {
                        text,
                        canonical_id: joined.clone(),
                    },
                    Err(e) => return Err(format!("reading module {}: {e}", path.display())),
                },
                _ => ImportResolution::NotFound,
            };
            outcome = resolve_import(instance, resolution);
        }
    }
    Ok(outcome)
}

/// Checks one step's stop — the outcome kind/reason/position, then the optional stack shape.
fn check_step(
    instance: &mut Instance,
    outcome: &Outcome,
    step: &DriveStep,
    nfc: &str,
    index: &LineIndex,
) -> Result<(), String> {
    match (&step.expect, outcome) {
        (StopAssertion::Completed, Outcome::Completed(_)) => {}
        (StopAssertion::Paused { reason, pos }, Outcome::Paused(actual)) => {
            let actual_reason = reason_name(*actual);
            if actual_reason != reason {
                return Err(format!(
                    "expected paused {reason}, got paused {actual_reason}"
                ));
            }
            let got = paused_position(instance, *actual).map(|p| index.position_at(nfc, p));
            if got.as_ref() != Some(pos) {
                return Err(format!(
                    "expected {reason} @ {}, got {}",
                    show(pos),
                    show_opt(&got)
                ));
            }
        }
        (StopAssertion::Raised { substring, pos }, Outcome::Raised(value, trace)) => {
            let (_kind, message) = instance.describe_raised(*value);
            if !message.contains(substring.as_str()) {
                return Err(format!(
                    "expected raise containing {substring:?}, got {message:?}"
                ));
            }
            let got = trace.raised_at.map(|s| index.position_at(nfc, s.start));
            if got.as_ref() != Some(pos) {
                return Err(format!(
                    "expected raise @ {}, got {}",
                    show(pos),
                    show_opt(&got)
                ));
            }
        }
        (StopAssertion::Suspended { capability, pos }, Outcome::Suspended(req)) => {
            let actual = capability_name(req.capability.0)
                .ok_or_else(|| format!("unknown capability id {}", req.capability.0))?;
            if actual != capability {
                return Err(format!(
                    "expected capability {capability}, got capability {actual}"
                ));
            }
            let got = instance
                .current_position()
                .map(|p| index.position_at(nfc, p.span.start));
            check_position("suspend", pos, &got)?;
        }
        (StopAssertion::Import { path, pos }, Outcome::SuspendedImport(req)) => {
            let actual = req.path.join(".");
            if &actual != path {
                return Err(format!("expected import {path}, got import {actual}"));
            }
            let got = instance
                .current_position()
                .map(|p| index.position_at(nfc, p.span.start));
            check_position("import", pos, &got)?;
        }
        (StopAssertion::Faulted { kind }, Outcome::Faulted(fault)) => {
            let actual = fault_kind(*fault);
            if &actual != kind {
                return Err(format!("expected faulted {kind}, got faulted {actual}"));
            }
        }
        (expected, actual) => {
            return Err(format!(
                "expected {expected:?}, got {}",
                outcome_kind(actual)
            ));
        }
    }
    if let Some(want) = &step.stack {
        check_stack(instance, want, nfc, index)?;
    }
    Ok(())
}

/// The source position a `paused` stop reports: the raise site for a raise-trap; the completed
/// subexpression for a fine (subexpression-mode) stop; otherwise the construct about to run.
fn paused_position(instance: &Instance, reason: PauseReason) -> Option<u32> {
    let engine = match reason {
        PauseReason::RaiseTrap => instance.trapped_raise_position(),
        _ => instance
            .completed_position()
            .or_else(|| instance.current_position()),
    };
    engine.map(|p| p.span.start)
}

/// Compares the actual stack shape (call frames innermost-first) against `want`, element-wise:
/// each element's line is always checked; its name and tail-iteration count only if the fixture
/// pins them (E§8.2/§8.3).
fn check_stack(
    instance: &mut Instance,
    want: &[StackElem],
    nfc: &str,
    index: &LineIndex,
) -> Result<(), String> {
    // The call frames, innermost first: those with a call site (the module top and a
    // native-invoked block have none, so they are not stack elements).
    let frames = instance.stack_walk();
    let actual: Vec<(Option<String>, u32, u64)> = frames
        .iter()
        .filter_map(|frame| {
            let line = index.position_at(nfc, frame.call_site?.start).line;
            let name = frame
                .callable
                .and_then(|h| instance.callable_name(h).ok().flatten());
            Some((name, line, frame.tail_count))
        })
        .collect();
    if actual.len() != want.len() {
        return Err(format!(
            "expected {}-frame stack, got {} frame(s): {actual:?}",
            want.len(),
            actual.len()
        ));
    }
    for (i, (elem, (got_name, got_line, got_tail))) in want.iter().zip(actual.iter()).enumerate() {
        if elem.line != *got_line {
            return Err(format!(
                "stack frame {i}: expected line {}, got {got_line}",
                elem.line
            ));
        }
        if let Some(name) = &elem.name
            && got_name.as_deref() != Some(name.as_str())
        {
            return Err(format!(
                "stack frame {i}: expected name {name:?}, got {got_name:?}"
            ));
        }
        if let Some(tail) = elem.tail
            && tail != *got_tail
        {
            return Err(format!(
                "stack frame {i}: expected tail ×{tail}, got ×{got_tail}"
            ));
        }
    }
    Ok(())
}

/// The transcript name of a pause reason (E§7.2).
fn reason_name(reason: PauseReason) -> &'static str {
    match reason {
        PauseReason::Step => "step",
        PauseReason::Breakpoint(_) => "breakpoint",
        PauseReason::RaiseTrap => "raise-trap",
        PauseReason::HostPause => "host-pause",
        PauseReason::SliceEnd => "slice-end",
    }
}

/// The transcript name of an engine fault (E§7.2/§10). Shared with `run` mode's `expect-fault`
/// (`matcher`), so a fault reads identically whether a drive script or a `run` fixture asserts it.
pub(crate) fn fault_kind(fault: EngineFault) -> String {
    match fault {
        EngineFault::LimitExceeded(kind) => match kind {
            LimitKind::StepBudget => "step-budget",
            LimitKind::Heap => "heap",
            LimitKind::StackDepth => "stack-depth",
            LimitKind::TailHistory => "tail-history",
            LimitKind::OpResult => "op-result",
        }
        .to_string(),
        EngineFault::Cancelled => "cancelled".to_string(),
        EngineFault::NestedSuspend => "nested-suspend".to_string(),
        EngineFault::Internal => "internal".to_string(),
    }
}

/// A short name for the actual outcome kind, for a shape-mismatch message.
fn outcome_kind(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Completed(_) => "completed".to_string(),
        Outcome::Paused(r) => format!("paused {}", reason_name(*r)),
        Outcome::Raised(..) => "raised".to_string(),
        Outcome::Suspended(_) => "suspended (capability)".to_string(),
        Outcome::SuspendedImport(_) => "suspended (import)".to_string(),
        Outcome::Faulted(f) => format!("faulted {}", fault_kind(*f)),
    }
}

/// Checks a stop's reported source position against the asserted one, reporting `label @ L:C` on a
/// mismatch.
fn check_position(label: &str, expected: &Position, got: &Option<Position>) -> Result<(), String> {
    if got.as_ref() == Some(expected) {
        Ok(())
    } else {
        Err(format!(
            "expected {label} @ {}, got {}",
            show(expected),
            show_opt(got)
        ))
    }
}

/// `L:C` for a position.
fn show(pos: &Position) -> String {
    format!("{}:{}", pos.line, pos.column)
}

/// `L:C` for an optional position, or `<none>`.
fn show_opt(pos: &Option<Position>) -> String {
    pos.as_ref().map_or_else(|| "<none>".to_string(), show)
}
