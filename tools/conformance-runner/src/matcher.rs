//! Executing a conformance test against doodle-core and matching its declared
//! expectations against real output.
//!
//! At M1.7 the `stage: lex` and `stage: parse` static stages are executable:
//! the lexer or parser runs and its diagnostics are matched against
//! `expect-static-error` / `expect-warning`. Higher stages (full/run) are still
//! SKIPped by the caller, so their expectation kinds (`expect-out` /
//! `expect-raise`) never reach here yet — a static test carrying one is
//! mis-authored and fails loudly.

use crate::model::{Expectation, Test};
use doodle_core::diag::{Diagnostic, Severity};
use doodle_core::source::{LineIndex, Position, normalize};
use doodle_core::stage::Stage;
use doodle_core::{lex_to_diagnostics, parse_to_diagnostics};

/// Executes `test` (whose required stage doodle-core implements) against
/// `source`, returning `Ok(())` on a full match or `Err(reasons)` listing every
/// mismatch found.
pub(crate) fn execute(test: &Test, source: &str) -> Result<(), Vec<String>> {
    match test.required {
        Stage::Lex | Stage::Parse => run_static(test, source, test.required),
        // The caller dispatches here only when implemented_through() >= required,
        // and Stage::Parse is the highest implemented stage, so no higher stage
        // reaches this arm. It exists so a future bump that forgets its executor
        // fails loudly instead of silently passing.
        other => Err(vec![format!(
            "no executor for stage {other:?} (runner/coordination bug)"
        )]),
    }
}

/// The `stage:` directive spelling of a static stage, for diagnostic text.
fn stage_label(stage: Stage) -> &'static str {
    match stage {
        Stage::Lex => "lex",
        Stage::Parse => "parse",
        Stage::Full => "full",
        Stage::Run => "run",
    }
}

/// Runs `source` to the given static `stage` (lexing, or lexing+parsing) and
/// matches the resulting diagnostics against the test's static-error / warning
/// expectations (conformance/README.md § `mode: static`).
fn run_static(test: &Test, source: &str, stage: Stage) -> Result<(), Vec<String>> {
    let mut reasons = Vec::new();
    let label = stage_label(stage);

    // A static test declares only load-time expectations; run-mode kinds are
    // meaningless here and indicate a mis-authored test. Echo the offending
    // directive so the author sees exactly what to remove.
    for exp in &test.expectations {
        match exp {
            Expectation::Out { text } => {
                reasons.push(format!(
                    "`expect-out: {text}` is not valid at `stage: {label}`"
                ));
            }
            Expectation::Raise { substring, pos } => reasons.push(format!(
                "`expect-raise: {substring} @ {}:{}` is not valid at `stage: {label}`",
                pos.line, pos.column
            )),
            Expectation::StaticError { .. } | Expectation::Warning { .. } => {}
        }
    }

    let nfc = normalize(source);
    let index = LineIndex::new(nfc.as_ref());
    let diagnostics = match stage {
        Stage::Parse => parse_to_diagnostics(nfc.as_ref()),
        _ => lex_to_diagnostics(nfc.as_ref()),
    };
    // Each diagnostic paired with its source position (None if it has no span,
    // which cannot match a positioned expectation).
    let located: Vec<(&Diagnostic, Option<Position>)> = diagnostics
        .iter()
        .map(|d| (d, d.span.map(|s| index.position_at(nfc.as_ref(), s.start))))
        .collect();

    // Errors: order-insensitive set match on (substring, position). Every
    // expected error must claim a distinct diagnostic, and no error diagnostic
    // may go unclaimed.
    let mut claimed = vec![false; located.len()];
    for exp in &test.expectations {
        let Expectation::StaticError { substring, pos } = exp else {
            continue;
        };
        match (0..located.len()).find(|&i| {
            let (d, dpos) = located[i];
            !claimed[i]
                && d.severity == Severity::Error
                && dpos == Some(*pos)
                && d.message.contains(substring.as_str())
        }) {
            Some(i) => claimed[i] = true,
            None => reasons.push(format!(
                "no error matching {substring:?} @ {}:{}",
                pos.line, pos.column
            )),
        }
    }
    for i in 0..located.len() {
        let (d, dpos) = located[i];
        if d.severity == Severity::Error && !claimed[i] {
            reasons.push(unexpected(d, dpos));
        }
    }

    // Warnings: every expected warning must occur; unlisted warnings never fail
    // a test (so success-expecting tests survive new lints).
    for exp in &test.expectations {
        let Expectation::Warning { substring, pos } = exp else {
            continue;
        };
        let matched = (0..located.len()).any(|i| {
            let (d, dpos) = located[i];
            d.severity == Severity::Warning
                && dpos == Some(*pos)
                && d.message.contains(substring.as_str())
        });
        if !matched {
            reasons.push(format!(
                "no warning matching {substring:?} @ {}:{}",
                pos.line, pos.column
            ));
        }
    }

    if reasons.is_empty() {
        Ok(())
    } else {
        Err(reasons)
    }
}

/// Renders an unclaimed error diagnostic for a FAIL report.
fn unexpected(d: &Diagnostic, pos: Option<Position>) -> String {
    match pos {
        Some(p) => format!(
            "unexpected error {} @ {}:{}: {}",
            d.code.slug(),
            p.line,
            p.column,
            d.message
        ),
        None => format!("unexpected error {}: {}", d.code.slug(), d.message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Mode;

    fn lex_test(expectations: Vec<Expectation>) -> Test {
        Test {
            id: "L3.6.1-num-001".to_string(),
            clauses: vec!["L3.6.1".to_string()],
            mode: Mode::Static,
            required: Stage::Lex,
            expectations,
        }
    }

    fn expect_error(substring: &str, line: u32, column: u32) -> Expectation {
        Expectation::StaticError {
            substring: substring.to_string(),
            pos: Position { line, column },
        }
    }

    fn reasons_of(test: &Test, source: &str) -> Vec<String> {
        execute(test, source).unwrap_err()
    }

    #[test]
    fn clean_source_with_no_expectations_passes() {
        assert!(execute(&lex_test(vec![]), "let x = 1 + 2\n").is_ok());
    }

    #[test]
    fn matches_a_static_error_by_substring_and_position() {
        let t = lex_test(vec![expect_error("between digits", 1, 1)]);
        assert!(execute(&t, "1__0\n").is_ok());
    }

    #[test]
    fn a_wrong_position_does_not_match() {
        let t = lex_test(vec![expect_error("between digits", 1, 2)]);
        assert!(
            reasons_of(&t, "1__0\n")
                .iter()
                .any(|r| r.contains("no error matching"))
        );
    }

    #[test]
    fn an_unlisted_error_fails() {
        // The source has a malformed number, but the test declares no error.
        assert!(
            reasons_of(&lex_test(vec![]), "1__0\n")
                .iter()
                .any(|r| r.contains("unexpected error"))
        );
    }

    #[test]
    fn an_expected_error_that_never_occurs_fails() {
        let t = lex_test(vec![expect_error("between digits", 1, 1)]);
        assert!(execute(&t, "42\n").is_err());
    }

    #[test]
    fn a_run_mode_expectation_is_rejected_at_lex_stage() {
        let t = lex_test(vec![Expectation::Out {
            text: "3".to_string(),
        }]);
        assert!(
            reasons_of(&t, "42\n")
                .iter()
                .any(|r| r.contains("not valid at `stage: lex`"))
        );
    }

    #[test]
    fn an_expected_warning_with_no_warning_fails() {
        // The lexer emits no warnings, so an expected warning cannot match;
        // unlisted warnings, in contrast, never fail a test.
        let t = lex_test(vec![Expectation::Warning {
            substring: "anything".to_string(),
            pos: Position { line: 1, column: 1 },
        }]);
        assert!(
            reasons_of(&t, "42\n")
                .iter()
                .any(|r| r.contains("no warning matching"))
        );
    }

    fn parse_test(expectations: Vec<Expectation>) -> Test {
        Test {
            id: "L5.3-assign-001".to_string(),
            clauses: vec!["L5.3".to_string()],
            mode: Mode::Static,
            required: Stage::Parse,
            expectations,
        }
    }

    #[test]
    fn parse_stage_matches_a_syntax_error() {
        // `1 = 2` is a parse-stage static error (a non-lvalue assignment target)
        // the lexer alone would never surface — so this exercises the parser arm.
        let t = parse_test(vec![expect_error("the left side of", 1, 1)]);
        assert!(execute(&t, "1 = 2\n").is_ok());
    }

    #[test]
    fn parse_stage_clean_source_passes() {
        assert!(execute(&parse_test(vec![]), "let x = 1\nx = x + 1\n").is_ok());
    }

    #[test]
    fn a_run_mode_expectation_is_rejected_at_parse_stage() {
        let t = parse_test(vec![Expectation::Out {
            text: "3".to_string(),
        }]);
        assert!(
            reasons_of(&t, "1 + 2\n")
                .iter()
                .any(|r| r.contains("not valid at `stage: parse`"))
        );
    }
}
