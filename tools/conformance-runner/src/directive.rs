//! Parsing of the `#!` directive block at the top of a `.doodle` test file.
//!
//! Directives are `#! `-prefixed comment lines (hash-bang-**space**) that
//! appear before the first non-comment line; `#!/…` is a shebang, not a
//! directive (conformance/README.md). At M0 the block is parsed and
//! syntax-validated; expectation *matching* begins at M1.

use crate::model::{Mode, Test};
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
    let mut expectation_count = 0usize;

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
                "expect-static-error" | "expect-warning" | "expect-raise" => {
                    parse_positioned(value)?;
                    expectation_count += 1;
                }
                "expect-out" => expectation_count += 1,
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

    Ok(Test {
        id,
        clauses,
        mode,
        required,
        expectation_count,
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
        other => Err(format!(
            "unknown mode `{other}` (expected `run` or `static`)"
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
        Mode::Run if stage_directive.is_some() => {
            Err("`stage:` is only valid in `mode: static`".to_string())
        }
        Mode::Run => Ok(Stage::Run),
        Mode::Static => Ok(stage_directive.unwrap_or(Stage::Full)),
    }
}

/// Validates a `<substring> @ <line>:<col>` position (the value itself is not
/// retained at M0 — only its well-formedness is checked).
fn parse_positioned(value: &str) -> Result<(), String> {
    let (_substring, pos) = value
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
    // Positions are 1-based in the NFC'd source (S-1).
    if line_no == 0 || col_no == 0 {
        return Err(format!(
            "positions are 1-based; got {line_no}:{col_no} in `{value}`"
        ));
    }
    Ok(())
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
        assert_eq!(t.expectation_count, 1);
        assert_eq!(t.id, "L6.5-arith-001");
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
    fn leading_bom_is_stripped() {
        let src = "\u{feff}#! clause: L1\n#! mode: static\nx\n";
        let t = parse_test("f.doodle", src).unwrap();
        assert_eq!(t.clauses, ["L1"]);
    }
}
