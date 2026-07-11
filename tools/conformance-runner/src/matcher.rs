//! Executing a conformance test against doodle-core and matching its declared
//! expectations against real output.
//!
//! At M1.3 only `stage: lex` is executable: the lexer runs and its diagnostics
//! are matched against `expect-static-error` / `expect-warning`. Higher stages
//! (parse/full/run) are still SKIPped by the caller, so their expectation kinds
//! (`expect-out` / `expect-raise`) never reach here yet — a lex test carrying
//! one is a mis-authored test and fails loudly.

use crate::model::{Expectation, Test};
use doodle_core::diag::{Diagnostic, Severity};
use doodle_core::lex_to_diagnostics;
use doodle_core::source::{LineIndex, Position, normalize};
use doodle_core::stage::Stage;

/// Executes `test` (whose required stage doodle-core implements) against
/// `source`, returning `Ok(())` on a full match or `Err(reasons)` listing every
/// mismatch found.
pub(crate) fn execute(test: &Test, source: &str) -> Result<(), Vec<String>> {
    match test.required {
        Stage::Lex => run_lex(test, source),
        // The caller dispatches here only when implemented_through() >= required,
        // and Stage::Lex is the highest implemented stage, so no higher stage
        // reaches this arm. It exists so a future bump that forgets its executor
        // fails loudly instead of silently passing.
        other => Err(vec![format!(
            "no executor for stage {other:?} (runner/coordination bug)"
        )]),
    }
}

/// Runs the lexer over `source` and matches its diagnostics against the test's
/// static-error / warning expectations (conformance/README.md § `mode: static`).
fn run_lex(test: &Test, source: &str) -> Result<(), Vec<String>> {
    let mut reasons = Vec::new();

    // A lex test declares only load-time expectations; run-mode kinds are
    // meaningless at this stage and indicate a mis-authored test. Echo the
    // offending directive so the author sees exactly what to remove.
    for exp in &test.expectations {
        match exp {
            Expectation::Out { text } => {
                reasons.push(format!("`expect-out: {text}` is not valid at `stage: lex`"));
            }
            Expectation::Raise { substring, pos } => reasons.push(format!(
                "`expect-raise: {substring} @ {}:{}` is not valid at `stage: lex`",
                pos.line, pos.column
            )),
            Expectation::StaticError { .. } | Expectation::Warning { .. } => {}
        }
    }

    let nfc = normalize(source);
    let index = LineIndex::new(nfc.as_ref());
    let diagnostics = lex_to_diagnostics(nfc.as_ref());
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
}
