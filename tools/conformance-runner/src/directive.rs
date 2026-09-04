//! Parsing of the `#!` directive block at the top of a `.doodle` test file.
//!
//! Directives are `#! `-prefixed comment lines (hash-bang-**space**) that
//! appear before the first non-comment line; `#!/…` is a shebang, not a
//! directive (conformance/README.md). At M0 the block is parsed and
//! syntax-validated; expectation *matching* begins at M1.

use crate::model::{Expectation, Mode, ScriptInput, ScriptResponse, Test};
use doodle_core::source::Position;
use doodle_core::stage::Stage;
use std::path::Path;

/// Parses the leading directive block of `source` into a [`Test`].
///
/// Returns a human-readable error for a malformed or self-inconsistent header:
/// an unknown directive, a non-`key: value` body, a `stage:` in run mode, a
/// malformed `@ <line>:<col>` position, or a missing `clause:`.
pub(crate) fn parse_test(rel_path: &str, source: &str) -> Result<Test, String> {
    // A leading UTF-8 BOM is not whitespace, so it would otherwise hide the
    // first directive; strip it before scanning.
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);

    let mut clauses: Vec<String> = Vec::new();
    let mut mode = Mode::Run;
    let mut stage_directive: Option<Stage> = None;
    let mut expectations: Vec<Expectation> = Vec::new();
    // Scripted capability responses (`#! input:`, run mode), in header order.
    let mut inputs: Vec<ScriptInput> = Vec::new();
    // Drive-script directives (`mode: drive`), retained raw and in order, then parsed into a
    // `DriveScript` below (crate::drivescript) once the whole header is read.
    let mut drive_raw: Vec<(String, String)> = Vec::new();

    for raw in source.lines() {
        let line = raw.trim_start();
        if let Some(body) = directive_body(line) {
            let (key, value) = split_directive(body)?;
            match key {
                "clause" => {
                    if value.is_empty() {
                        return Err("empty `#! clause:` value".to_string());
                    }
                    clauses.push(value.to_string());
                }
                "mode" => mode = parse_mode(value)?,
                "stage" => stage_directive = Some(parse_stage(value)?),
                "expect-static-error" => {
                    let (substring, pos) = parse_positioned(value)?;
                    expectations.push(Expectation::StaticError { substring, pos });
                }
                "expect-warning" => {
                    let (substring, pos) = parse_positioned(value)?;
                    expectations.push(Expectation::Warning { substring, pos });
                }
                "expect-raise" => {
                    let (substring, pos) = parse_positioned(value)?;
                    expectations.push(Expectation::Raise { substring, pos });
                }
                "expect-out" => expectations.push(Expectation::Out {
                    text: value.to_string(),
                }),
                "input" => inputs.push(parse_input(value)?),
                // Drive-script directives — retained raw, parsed as a unit below.
                "break" | "raise-trap" | "obs" | "do" | "resolve" | "resolve-raise" | "expect"
                | "stack" => {
                    drive_raw.push((key.to_string(), value.to_string()));
                }
                // Reserved for the inspection follow-on (value/render assertions): a named slot
                // that fails loudly, so a fixture using it is not silently skipped or mis-spelled.
                "local" | "render" => {
                    return Err(format!(
                        "`#! {key}:` is reserved for a future drive-script extension, not yet supported"
                    ));
                }
                other => return Err(format!("unknown directive `#! {other}:`")),
            }
        } else if is_comment_or_blank(line) {
            continue; // shebang (`#!/…`), ordinary comments, and blank lines
        } else {
            break; // the first non-comment line: the directive block ends here
        }
    }

    let required = resolve_stage(mode, stage_directive)?;
    let primary = clauses
        .first()
        .ok_or_else(|| "missing required `#! clause:` directive".to_string())?;
    let id = test_id(primary, rel_path);

    // Drive directives belong only to `mode: drive`, and a drive fixture uses them instead of the
    // `expect-…` directives — cross-checked so a mis-declared fixture fails at parse, not silently.
    let drive = if mode == Mode::Drive {
        if !expectations.is_empty() {
            return Err(
                "`mode: drive` uses `do:`/`expect:`, not `expect-…` directives".to_string(),
            );
        }
        Some(crate::drivescript::parse(&drive_raw)?)
    } else {
        if !drive_raw.is_empty() {
            return Err(
                "`break:`/`do:`/`resolve:`/`expect:`/`stack:` require `mode: drive`".to_string(),
            );
        }
        None
    };
    // `input:` scripts a capability response before the run drives it — a `run`-mode notion (a drive
    // fixture uses `resolve:` steps instead).
    if !inputs.is_empty() && mode != Mode::Run {
        return Err("`#! input:` requires `mode: run` (drive fixtures use `resolve:`)".to_string());
    }

    Ok(Test {
        id,
        clauses,
        mode,
        required,
        expectations,
        inputs,
        drive,
    })
}

/// Parses `#! input: <capability> -> <response>` (run mode): the capability name, then either a
/// scripted value or `raise "<msg>"`.
fn parse_input(value: &str) -> Result<ScriptInput, String> {
    let (capability, response) = value
        .split_once("->")
        .ok_or_else(|| format!("`input:` expects `<capability> -> <response>`, got `{value}`"))?;
    let capability = capability.trim();
    if capability.is_empty() {
        return Err(format!("`input:` has no capability name in `{value}`"));
    }
    let response = response.trim();
    let response = match response.strip_prefix("raise") {
        Some(msg) => ScriptResponse::Raise(crate::drivescript::parse_string_literal(msg)?),
        None => ScriptResponse::Value(crate::drivescript::parse_script_value(response)?),
    };
    Ok(ScriptInput {
        capability: capability.to_string(),
        response,
    })
}

/// The body of a directive line (`#! <body>`), or `None` if the line is not a
/// directive. Requires the space after `#!`, so `#!/…` shebangs are excluded.
fn directive_body(line: &str) -> Option<&str> {
    line.strip_prefix("#!")
        .filter(|rest| rest.starts_with(' '))
        .map(str::trim)
}

/// Whether `line` (already left-trimmed) is a comment or blank — the lines
/// permitted between directives and before the first statement.
fn is_comment_or_blank(line: &str) -> bool {
    line.is_empty() || line.starts_with('#')
}

/// Splits a directive body into `(key, value)` on the first colon.
fn split_directive(body: &str) -> Result<(&str, &str), String> {
    body.split_once(':')
        .map(|(k, v)| (k.trim(), v.trim()))
        .ok_or_else(|| format!("directive is not `key: value`: `{body}`"))
}

fn parse_mode(value: &str) -> Result<Mode, String> {
    match value {
        "run" => Ok(Mode::Run),
        "static" => Ok(Mode::Static),
        "drive" => Ok(Mode::Drive),
        other => Err(format!(
            "unknown mode `{other}` (expected `run`, `static`, or `drive`)"
        )),
    }
}

fn parse_stage(value: &str) -> Result<Stage, String> {
    match value {
        "lex" => Ok(Stage::Lex),
        "parse" => Ok(Stage::Parse),
        "full" => Ok(Stage::Full),
        other => Err(format!(
            "unknown stage `{other}` (expected lex, parse, or full)"
        )),
    }
}

/// Resolves the stage a test requires from its mode and optional `stage:`.
fn resolve_stage(mode: Mode, stage_directive: Option<Stage>) -> Result<Stage, String> {
    match mode {
        Mode::Run | Mode::Drive if stage_directive.is_some() => {
            Err("`stage:` is only valid in `mode: static`".to_string())
        }
        // A drive script needs the full machine, exactly as a `run` fixture does.
        Mode::Run | Mode::Drive => Ok(Stage::Run),
        Mode::Static => Ok(stage_directive.unwrap_or(Stage::Full)),
    }
}

/// Parses a `<substring> @ <line>:<col>` value into its match substring and
/// 1-based [`Position`] (in the NFC'd source, S-1). Shared with the drive-script parser.
pub(crate) fn parse_positioned(value: &str) -> Result<(String, Position), String> {
    let (substring, pos) = value
        .rsplit_once('@')
        .ok_or_else(|| format!("expected `<substring> @ <line>:<col>` in `{value}`"))?;
    let (line, col) = pos
        .trim()
        .split_once(':')
        .ok_or_else(|| format!("expected `<line>:<col>` after `@` in `{value}`"))?;
    let line_no: u32 = line
        .trim()
        .parse()
        .map_err(|_| format!("bad line number in `{value}`"))?;
    let col_no: u32 = col
        .trim()
        .parse()
        .map_err(|_| format!("bad column number in `{value}`"))?;
    if line_no == 0 || col_no == 0 {
        return Err(format!(
            "positions are 1-based; got {line_no}:{col_no} in `{value}`"
        ));
    }
    let substring = substring.trim();
    if substring.is_empty() {
        // An empty substring would degrade to a position-only match (`contains`
        // of "" is always true); the format requires expected text.
        return Err(format!("empty expectation substring in `{value}`"));
    }
    Ok((
        substring.to_string(),
        Position {
            line: line_no,
            column: col_no,
        },
    ))
}

/// Builds the canonical test id `<primary-clause>-<topic>-<seq>` from the
/// primary clause and the file stem (`<topic>-<seq>_<slug>`).
fn test_id(primary_clause: &str, rel_path: &str) -> String {
    let stem = Path::new(rel_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(rel_path);
    let topic_seq = stem.split_once('_').map_or(stem, |(head, _slug)| head);
    format!("{primary_clause}-{topic_seq}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_run_test() {
        let src = "#! clause: L6.5\n#! mode: run\n#! expect-out: 3\nprint(1 + 2)\n";
        let t = parse_test("v0.1/lang/L6.5/arith-001_int_add.doodle", src).unwrap();
        assert_eq!(t.mode, Mode::Run);
        assert_eq!(t.required, Stage::Run);
        assert_eq!(t.clauses, ["L6.5"]);
        assert_eq!(t.expectations.len(), 1);
        assert!(matches!(&t.expectations[0], Expectation::Out { text } if text == "3"));
        assert_eq!(t.id, "L6.5-arith-001");
    }

    #[test]
    fn retains_substring_and_position() {
        let src = "#! clause: L3.6.1\n#! mode: static\n#! stage: lex\n\
                   #! expect-static-error: between digits @ 4:5\n1__0\n";
        let t = parse_test("f.doodle", src).unwrap();
        let Expectation::StaticError { substring, pos } = &t.expectations[0] else {
            panic!("expected a StaticError expectation");
        };
        assert_eq!(substring, "between digits");
        assert_eq!((pos.line, pos.column), (4, 5));
    }

    #[test]
    fn static_defaults_to_full_stage() {
        let src = "#! clause: L6.2\n#! mode: static\nfn double(n)\n  return n * 2\nend\n";
        let t = parse_test("f.doodle", src).unwrap();
        assert_eq!(t.mode, Mode::Static);
        assert_eq!(t.required, Stage::Full);
    }

    #[test]
    fn stage_directive_selects_lex() {
        let src = "#! clause: L3.3\n#! mode: static\n#! stage: lex\nprint(\"hi\")\n";
        let t = parse_test("f.doodle", src).unwrap();
        assert_eq!(t.required, Stage::Lex);
    }

    #[test]
    fn shebang_is_not_a_directive() {
        let src = "#!/usr/bin/env doodle\n#! clause: L3.3\n#! mode: static\nprint(\"hi\")\n";
        let t = parse_test("f.doodle", src).unwrap();
        assert_eq!(t.clauses, ["L3.3"]);
        assert_eq!(t.mode, Mode::Static);
    }

    #[test]
    fn stage_in_run_mode_is_rejected() {
        let src = "#! clause: L1\n#! mode: run\n#! stage: lex\nprint(1)\n";
        assert!(parse_test("f.doodle", src).is_err());
    }

    #[test]
    fn missing_clause_is_rejected() {
        let src = "#! mode: static\nfn f()\nend\n";
        assert!(parse_test("f.doodle", src).is_err());
    }

    #[test]
    fn unknown_directive_is_rejected() {
        let src = "#! clause: L1\n#! wibble: 3\nprint(1)\n";
        assert!(parse_test("f.doodle", src).is_err());
    }

    #[test]
    fn malformed_position_is_rejected() {
        let src = "#! clause: L1\n#! mode: static\n#! expect-static-error: oops @ 4\nx\n";
        assert!(parse_test("f.doodle", src).is_err());
    }

    #[test]
    fn directives_end_at_first_statement() {
        // A `#! `-looking line after code is not parsed as a directive.
        let src = "#! clause: L1\nprint(1)\n#! mode: static\n";
        let t = parse_test("f.doodle", src).unwrap();
        assert_eq!(t.mode, Mode::Run); // the post-code `mode:` line was ignored
    }

    #[test]
    fn empty_clause_is_rejected() {
        assert!(parse_test("f.doodle", "#! clause:\nprint(1)\n").is_err());
    }

    #[test]
    fn zero_position_is_rejected() {
        let src = "#! clause: L1\n#! mode: static\n#! expect-static-error: x @ 0:3\ny\n";
        assert!(parse_test("f.doodle", src).is_err());
    }

    #[test]
    fn empty_expectation_substring_is_rejected() {
        let src = "#! clause: L1\n#! mode: static\n#! expect-static-error:  @ 1:1\ny\n";
        assert!(parse_test("f.doodle", src).is_err());
    }

    #[test]
    fn leading_bom_is_stripped() {
        let src = "\u{feff}#! clause: L1\n#! mode: static\nx\n";
        let t = parse_test("f.doodle", src).unwrap();
        assert_eq!(t.clauses, ["L1"]);
    }

    #[test]
    fn parses_a_drive_script() {
        let src = "#! clause: E8.6\n#! mode: drive\n#! break: 4\n#! raise-trap: on\n\
                   #! do: continue\n#! expect: paused breakpoint @ 4:1\n\
                   #! do: continue\n#! expect: raised boom @ 4:1\nprint(1)\nraise \"boom\"\n";
        let t = parse_test("v0.1/eng/E8.6/x.doodle", src).unwrap();
        assert_eq!(t.mode, Mode::Drive);
        assert_eq!(t.required, Stage::Run);
        let drive = t.drive.expect("a drive script");
        assert_eq!(drive.breakpoints, [("main".to_string(), 4)]);
        assert!(drive.raise_trap && !drive.subexpr);
        assert_eq!(drive.steps.len(), 2);
    }

    #[test]
    fn a_reserved_drive_directive_is_an_error() {
        // `local:`/`render:` are reserved-but-unimplemented named slots — a loud error, never
        // silently ignored, so the inspection follow-on cannot be reinvented ad hoc.
        for key in ["local", "render"] {
            let src = format!("#! clause: E8\n#! mode: drive\n#! do: step\n#! {key}: x\nx\n");
            assert!(parse_test("f.doodle", &src).is_err(), "{key} must error");
        }
    }

    #[test]
    fn an_unknown_drive_directive_is_an_error() {
        let src = "#! clause: E8\n#! mode: drive\n#! wat: x\n#! do: step\nx\n";
        assert!(parse_test("f.doodle", src).is_err());
    }

    #[test]
    fn drive_directives_require_drive_mode() {
        let src = "#! clause: E8\n#! mode: run\n#! do: step\nx\n";
        assert!(parse_test("f.doodle", src).is_err());
    }

    #[test]
    fn a_do_without_an_expect_is_an_error() {
        let src = "#! clause: E8\n#! mode: drive\n#! do: step\n#! do: step\nx\n";
        assert!(parse_test("f.doodle", src).is_err());
    }
}
