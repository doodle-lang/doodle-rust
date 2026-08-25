//! Executing a conformance test against doodle-core and matching its declared
//! expectations against real output.
//!
//! Static stages (`lex`/`parse`/`full`) run the front end and match diagnostics
//! against `expect-static-error` / `expect-warning` (see [`run_static`]). The `run`
//! stage drives the machine (see [`run_dynamic`]) and matches both `expect-raise`
//! (the uncaught exception) and `expect-out` (the captured output — the `print`
//! intrinsic is registered before load, S-43/M2b.2). A run expectation at a static
//! stage (or vice versa) is a mis-authored test and fails loudly.

use crate::model::{Expectation, Test};
use doodle_core::diag::{Diagnostic, Severity};
use doodle_core::drive::{Directive, Outcome, run};
use doodle_core::machine::{Instance, Registry, print_intrinsic};
use doodle_core::parse::parse_program;
use doodle_core::resolve::resolve;
use doodle_core::source::{LineIndex, Position, normalize};
use doodle_core::span::ModuleId;
use doodle_core::stage::Stage;
use doodle_core::{full_to_diagnostics, lex_to_diagnostics, parse_to_diagnostics};

/// Executes `test` (whose required stage doodle-core implements) against
/// `source`, returning `Ok(())` on a full match or `Err(reasons)` listing every
/// mismatch found.
pub(crate) fn execute(test: &Test, source: &str) -> Result<(), Vec<String>> {
    match test.required {
        Stage::Lex | Stage::Parse | Stage::Full => run_static(test, source, test.required),
        Stage::Run => run_dynamic(test, source),
    }
}

/// Executes a `mode: run` test: load the program, drive it to completion, and match
/// its outcome against the test's `expect-raise` expectations and its captured
/// output against `expect-out` (conformance/README.md § `mode: run`). The `print`
/// intrinsic is registered before load (S-43), so `expect-out` tests execute at M2b.
fn run_dynamic(test: &Test, source: &str) -> Result<(), Vec<String>> {
    let nfc = normalize(source);
    let index = LineIndex::new(nfc.as_ref());

    // A run fixture must load clean — a load-time (parse/resolve) error is a FAIL,
    // not a raise. Report every such error so a mis-authored fixture is obvious.
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

    let mut instance = Instance::load_with_intrinsics(resolved.module, demo_registry());
    let outcome = run(&mut instance, Directive::RunToCompletion);
    // Check the outcome (raise/completion) and the captured output independently, so
    // a fixture asserting both, or either alone, gets every mismatch reported.
    let mut reasons = Vec::new();
    if let Err(mut e) = match_run_outcome(test, &instance, &outcome, nfc.as_ref(), &index) {
        reasons.append(&mut e);
    }
    if let Err(mut e) = match_output(test, instance.output()) {
        reasons.append(&mut e);
    }
    if reasons.is_empty() {
        Ok(())
    } else {
        Err(reasons)
    }
}

/// The registry the runner drives with: the demo intrinsics a `mode: run` fixture may
/// use. At M2b that is `print` (S-43); more join as they land.
fn demo_registry() -> Registry {
    let mut registry = Registry::new();
    registry
        .register(print_intrinsic())
        .expect("print registers cleanly into a fresh registry");
    registry
}

/// Matches captured output against the test's `expect-out` lines. Each `expect-out`
/// directive is one printed line; `print` emits its argument followed by a newline,
/// so the expected transcript is the lines each newline-terminated, in order. A run
/// fixture with **no** `expect-out` expects an **empty** transcript — a program that
/// prints anything then FAILs (spurious output is a determinism-visible bug,
/// conformance/README.md), so the empty case is not special-cased away.
fn match_output(test: &Test, actual: &[u8]) -> Result<(), Vec<String>> {
    let expected: String = test
        .expectations
        .iter()
        .filter_map(|e| match e {
            Expectation::Out { text } => Some(format!("{text}\n")),
            _ => None,
        })
        .collect();
    let actual = String::from_utf8_lossy(actual);
    if actual == expected {
        Ok(())
    } else {
        Err(vec![format!(
            "expected output {expected:?}, got {actual:?}"
        )])
    }
}

/// The messages of every error-severity diagnostic (empty when the load is clean).
fn error_messages(diagnostics: &[Diagnostic]) -> Vec<String> {
    diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| format!("load error: {}", d.message))
        .collect()
}

/// Matches a driven program's `outcome` against a run test's expectations. At M2a
/// the only expectation kind reaching here is `expect-raise` (an uncaught exception,
/// which terminates the program), plus the empty transcript of a clean completion.
fn match_run_outcome(
    test: &Test,
    instance: &Instance,
    outcome: &Outcome,
    nfc: &str,
    index: &LineIndex,
) -> Result<(), Vec<String>> {
    let expected: Vec<(&String, &Position)> = test
        .expectations
        .iter()
        .filter_map(|e| match e {
            Expectation::Raise { substring, pos } => Some((substring, pos)),
            _ => None,
        })
        .collect();

    match outcome {
        Outcome::Raised(value, trace) => {
            // The raised value is a Doodle value (an `Error` record, or any `raise`d
            // value, E§9); match `expect-raise` against its described message.
            let (_kind, message) = instance.describe_raised(*value);
            let pos = trace.raised_at.map(|s| index.position_at(nfc, s.start));
            // An uncaught raise terminates, so the transcript is exactly one raise:
            // exactly one `expect-raise` must match it (substring + position).
            if expected.len() != 1 {
                return Err(vec![format!(
                    "program raised ({}), but the test declares {} expect-raise expectation(s)",
                    describe_raise(&message, pos),
                    expected.len()
                )]);
            }
            let (substring, want) = expected[0];
            if message.contains(substring.as_str()) && pos == Some(*want) {
                Ok(())
            } else {
                Err(vec![format!(
                    "expected raise {substring:?} @ {}:{}, got {}",
                    want.line,
                    want.column,
                    describe_raise(&message, pos)
                )])
            }
        }
        Outcome::Completed(_) => {
            if expected.is_empty() {
                Ok(())
            } else {
                Err(vec![format!(
                    "expected a raise ({:?}) but the program completed",
                    expected[0].0
                )])
            }
        }
        Outcome::Faulted(fault) => Err(vec![format!(
            "program did not complete: Faulted({fault:?})"
        )]),
        Outcome::Suspended(_) => Err(vec![
            "program suspended (no capabilities at M2a)".to_string(),
        ]),
        Outcome::Paused(_) => Err(vec!["program paused (no observation at M2a)".to_string()]),
    }
}

/// Renders an actual raise (message + position) for a FAIL report.
fn describe_raise(message: &str, pos: Option<Position>) -> String {
    match pos {
        Some(p) => format!("{message:?} @ {}:{}", p.line, p.column),
        None => format!("{message:?} @ <no position>"),
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

/// Runs `source` to the given static `stage` (lexing; lexing+parsing; or
/// lexing+parsing+resolving) and matches the resulting diagnostics against the
/// test's static-error / warning expectations (conformance/README.md §
/// `mode: static`).
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
        Stage::Full => full_to_diagnostics(nfc.as_ref()),
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

    fn full_test(expectations: Vec<Expectation>) -> Test {
        Test {
            id: "L5.3-const-001".to_string(),
            clauses: vec!["L5.3".to_string()],
            mode: Mode::Static,
            required: Stage::Full,
            expectations,
        }
    }

    #[test]
    fn full_stage_matches_a_resolver_error() {
        // Reassigning a `const` is a resolver-stage static error that lex+parse
        // alone never surface — so this exercises the resolver (full) arm.
        let t = full_test(vec![expect_error("can't assign to", 2, 1)]);
        assert!(execute(&t, "const c = 1\nc = 2\n").is_ok());
    }

    #[test]
    fn full_stage_clean_source_passes() {
        assert!(execute(&full_test(vec![]), "fn f()\n  1\nend\n").is_ok());
    }
}
