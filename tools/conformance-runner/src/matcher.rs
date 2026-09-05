//! Executing a conformance test against doodle-core and matching its declared
//! expectations against real output.
//!
//! Static stages (`lex`/`parse`/`full`) run the front end and match diagnostics
//! against `expect-static-error` / `expect-warning` (see [`run_static`]). The `run`
//! stage drives the machine (see [`run_dynamic`]) and matches both `expect-raise`
//! (the uncaught exception) and `expect-out` (the captured output — the `print`
//! intrinsic is registered before load, S-43/M2b.2). A run expectation at a static
//! stage (or vice versa) is a mis-authored test and fails loudly.

use crate::capability::{capability_name, is_registered, registry};
use crate::drive::fault_kind;
use crate::model::{Expectation, Mode, ScriptInput, ScriptResponse, ScriptValue, Test};
use doodle_core::diag::{Diagnostic, Severity};
use doodle_core::drive::{
    Directive, ImportResolution, Limits, Outcome, Resolution, resolve as resolve_capability,
    resolve_import, run,
};
use doodle_core::machine::{Handle, Instance};
use doodle_core::parse::parse_program;
use doodle_core::resolve::resolve;
use doodle_core::source::{LineIndex, Position, normalize};
use doodle_core::span::ModuleId;
use doodle_core::stage::Stage;
use doodle_core::{full_to_diagnostics, lex_to_diagnostics, parse_to_diagnostics};
use std::path::Path;

/// Executes `test` (whose required stage doodle-core implements) against `source`, returning
/// `Ok(())` on a full match or `Err(reasons)` listing every mismatch. `modules_dir` is the
/// directory a multi-module fixture's `import name` resolves within (`<dir>/name.doodle`).
pub(crate) fn execute(
    test: &Test,
    source: &str,
    modules_dir: Option<&Path>,
) -> Result<(), Vec<String>> {
    // A `#! requires:` names a manifest primitive the fixture calls (D-M7-21 rider): every one must
    // be registered, or this harness cannot run the fixture — fail at fixture start (loud), not
    // mid-run with a confusing name-not-defined. (Natively the manifest is fixed, so this catches a
    // typo'd requirement; portably it catches a harness whose manifest lags the corpus.)
    let missing: Vec<&String> = test
        .requires
        .iter()
        .filter(|name| !is_registered(name))
        .collect();
    if !missing.is_empty() {
        return Err(missing
            .iter()
            .map(|name| format!("`#! requires: {name}` names no registered manifest primitive"))
            .collect());
    }
    match test.required {
        Stage::Lex | Stage::Parse | Stage::Full => run_static(test, source, test.required),
        // Both `run` and `drive` need the full machine; the mode picks the executor.
        Stage::Run => match test.mode {
            Mode::Drive => crate::drive::run_drive(test, source, modules_dir),
            _ => run_dynamic(test, source, modules_dir),
        },
    }
}

/// Executes a `mode: run` test: load the program, drive it to completion, and match
/// its outcome against the test's `expect-raise` expectations and its captured
/// output against `expect-out` (conformance/README.md § `mode: run`). The `print`
/// intrinsic is registered before load (S-43), so `expect-out` tests execute at M2b.
fn run_dynamic(test: &Test, source: &str, modules_dir: Option<&Path>) -> Result<(), Vec<String>> {
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

    // The entry id `main` matches the drive runner (E§3.2); it does not appear in program output,
    // so a `run` fixture's transcript is unaffected by it.
    let mut instance = Instance::load(resolved.module, Limits::default(), registry(), "main");
    let outcome = match drive_to_terminal(&mut instance, modules_dir, &test.inputs) {
        Ok(outcome) => outcome,
        Err(reason) => return Err(vec![reason]),
    };
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

/// Drives `instance` to a terminal outcome, transparently resolving each `import` to a sibling
/// module file `<modules_dir>/<path>.doodle` (a multi-module fixture, directory-as-fixture; a
/// missing file or single-module fixture resolves `NotFound`, so the program meets a
/// `module-not-found` it can assert) and each capability request from the fixture's scripted
/// `input:` queue (`inputs`). A resolved-but-unreadable module, or an unscripted/unknown capability,
/// is a runner error (a mis-authored fixture), surfaced as the one failure reason.
fn drive_to_terminal(
    instance: &mut Instance,
    modules_dir: Option<&Path>,
    inputs: &[ScriptInput],
) -> Result<Outcome, String> {
    let mut queues = CapabilityQueues::new(inputs);
    let mut outcome = run(instance, Directive::RunToCompletion);
    loop {
        match &outcome {
            Outcome::SuspendedImport(req) => {
                let joined = req
                    .path
                    .iter()
                    .map(|s| s.as_ref())
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
            Outcome::Suspended(req) => {
                let name = capability_name(req.capability.0)
                    .ok_or_else(|| format!("unknown capability id {}", req.capability.0))?;
                let response = queues
                    .next(name)
                    .ok_or_else(|| format!("no scripted `input:` for capability `{name}`"))?;
                for &handle in &req.args {
                    let _ = instance.release(handle);
                }
                let resolution = build_resolution(instance, &response)?;
                outcome = resolve_capability(instance, resolution);
            }
            _ => return Ok(outcome),
        }
    }
}

/// Per-capability FIFO queues of scripted `input:` responses, drawn from as each capability
/// suspends (responses for one capability are consumed in header order).
struct CapabilityQueues {
    queues: Vec<(String, std::collections::VecDeque<ScriptResponse>)>,
}

impl CapabilityQueues {
    fn new(inputs: &[ScriptInput]) -> Self {
        let mut queues: Vec<(String, std::collections::VecDeque<ScriptResponse>)> = Vec::new();
        for input in inputs {
            match queues
                .iter_mut()
                .find(|(name, _)| *name == input.capability)
            {
                Some((_, queue)) => queue.push_back(input.response.clone()),
                None => queues.push((
                    input.capability.clone(),
                    std::collections::VecDeque::from([input.response.clone()]),
                )),
            }
        }
        CapabilityQueues { queues }
    }

    /// The next scripted response for `capability`, or `None` if its queue is empty/absent.
    fn next(&mut self, capability: &str) -> Option<ScriptResponse> {
        self.queues
            .iter_mut()
            .find(|(name, _)| name == capability)
            .and_then(|(_, queue)| queue.pop_front())
    }
}

/// Builds a [`Resolution`] from a scripted response: a value (materialized into a host handle) or a
/// raise (a message string handle raised at the call site, E§7.5).
pub(crate) fn build_resolution(
    instance: &mut Instance,
    response: &ScriptResponse,
) -> Result<Resolution, String> {
    Ok(match response {
        ScriptResponse::Value(value) => Resolution::Value(make_value(instance, value)?),
        ScriptResponse::Raise(message) => Resolution::Raise(
            instance
                .make_string(message.as_bytes())
                .map_err(|e| format!("bad raise message: {e:?}"))?,
        ),
    })
}

/// Materializes a [`ScriptValue`] into a host handle (E§4) for resolving a capability.
fn make_value(instance: &mut Instance, value: &ScriptValue) -> Result<Handle, String> {
    Ok(match value {
        ScriptValue::Str(s) => instance
            .make_string(s.as_bytes())
            .map_err(|e| format!("bad scripted string: {e:?}"))?,
        ScriptValue::Int(n) => instance.make_int(*n),
        ScriptValue::Float(f) => instance.make_float(*f),
        ScriptValue::Bool(b) => instance.make_bool(*b),
        ScriptValue::Nil => instance.make_nil(),
    })
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
pub(crate) fn error_messages(diagnostics: &[Diagnostic]) -> Vec<String> {
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

    // A fault (E§10) is a distinct terminal outcome from a Doodle raise: when the fixture declares
    // `expect-fault`, the run must fault with that kind (`nested-suspend`, a limit, …) and nothing
    // else. Output is matched separately, so a fixture may print then fault.
    if let Some(kind) = test.expectations.iter().find_map(|e| match e {
        Expectation::Fault { kind } => Some(kind),
        _ => None,
    }) {
        return match outcome {
            Outcome::Faulted(fault) => {
                let actual = fault_kind(*fault);
                if &actual == kind {
                    Ok(())
                } else {
                    Err(vec![format!("expected fault {kind}, got fault {actual}")])
                }
            }
            other => Err(vec![format!(
                "expected fault {kind}, but {}",
                describe_outcome(other)
            )]),
        };
    }

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
        // `drive_to_terminal` resolves every import (a multi-module fixture's siblings, else
        // `NotFound`) before returning, so a `SuspendedImport` never reaches here.
        Outcome::SuspendedImport(_) => Err(vec![
            "internal: import left unresolved by the drive loop".to_string(),
        ]),
        Outcome::Paused(_) => Err(vec!["program paused (no observation at M2a)".to_string()]),
    }
}

/// A short description of an actual outcome, for a FAIL report when a fixture expected a fault.
fn describe_outcome(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Completed(_) => "the program completed".to_string(),
        Outcome::Raised(..) => "the program raised".to_string(),
        Outcome::Faulted(f) => format!("faulted {}", fault_kind(*f)),
        Outcome::Suspended(_) => "the program suspended (capability)".to_string(),
        Outcome::SuspendedImport(_) => "the program suspended (import)".to_string(),
        Outcome::Paused(_) => "the program paused".to_string(),
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
            Expectation::Fault { kind } => reasons.push(format!(
                "`expect-fault: {kind}` is not valid at `stage: {label}`"
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

    /// Parses `src` as a fixture and runs it through `execute`, returning the pass/fail result.
    fn run_fixture(src: &str) -> Result<(), Vec<String>> {
        let test =
            crate::directive::parse_test("X1.1/fixture.doodle", src).expect("fixture parses");
        execute(&test, src, None)
    }

    #[test]
    fn a_run_fixture_resolves_a_capability_from_the_input_queue() {
        // `read_line` suspends; the scripted `input:` queue supplies its value, which `print` emits.
        let src = "#! clause: X1.1\n#! mode: run\n#! expect-out: hi\n\
                   #! input: read_line -> \"hi\"\nprint(read_line())\n";
        assert_eq!(run_fixture(src), Ok(()), "run with a scripted read_line");
    }

    #[test]
    fn a_run_fixture_draws_capability_responses_in_order() {
        // Two `read_line` requests draw the queue front-to-back.
        let src = "#! clause: X1.1\n#! mode: run\n#! expect-out: a\n#! expect-out: b\n\
                   #! input: read_line -> \"a\"\n#! input: read_line -> \"b\"\n\
                   print(read_line())\nprint(read_line())\n";
        assert_eq!(run_fixture(src), Ok(()), "two scripted reads, in order");
    }

    #[test]
    fn a_run_fixture_without_a_scripted_response_fails() {
        // An unscripted capability request is a mis-authored fixture, not a silent pass.
        let src = "#! clause: X1.1\n#! mode: run\nprint(read_line())\n";
        assert!(run_fixture(src).is_err(), "unscripted read_line must fail");
    }

    #[test]
    fn a_drive_fixture_scripts_a_capability_resolution() {
        // `read_line` suspends as a capability stop; `resolve:` feeds the value and the run finishes.
        // The program is on line 7 (six `#!` header lines precede it).
        let src = "#! clause: X1.1\n#! mode: drive\n#! do: run\n\
                   #! expect: suspended read_line @ 7:1\n#! resolve: \"hi\"\n#! expect: completed\n\
                   print(read_line())\n";
        assert_eq!(run_fixture(src), Ok(()), "drive resolves the capability");
    }

    #[test]
    fn a_run_fixture_matches_a_nested_suspend_fault() {
        // A capability inside a foreign block-consumer (`each`) faults `nested-suspend` (S-15);
        // `expect-fault` asserts the fault kind as a terminal run outcome. No `input:` — it never
        // resolves.
        let src = "#! clause: E5.4\n#! mode: run\n#! requires: each, read_line\n\
                   #! expect-fault: nested-suspend\neach([1]) do (x)\n  read_line()\nend\n";
        assert_eq!(
            run_fixture(src),
            Ok(()),
            "each+capability faults nested-suspend"
        );
    }

    #[test]
    fn a_wrong_expected_fault_kind_fails() {
        let src = "#! clause: E5.4\n#! mode: run\n#! requires: each, read_line\n\
                   #! expect-fault: step-budget\neach([1]) do (x)\n  read_line()\nend\n";
        assert!(run_fixture(src).is_err(), "wrong fault kind must fail");
    }

    #[test]
    fn a_completing_program_with_expect_fault_fails() {
        // `expect-fault` on a program that completes is a mismatch (a fault is a distinct terminal).
        let src = "#! clause: E5\n#! mode: run\n#! expect-fault: nested-suspend\nlet x = 1\n";
        assert!(
            run_fixture(src).is_err(),
            "completion must fail an expect-fault"
        );
    }

    #[test]
    fn an_unregistered_requirement_fails_at_fixture_start() {
        // A `requires:` naming a primitive not in the manifest is a fixture error (loud), not a
        // mysterious name-not-defined mid-run.
        let src = "#! clause: E5\n#! mode: run\n#! requires: no_such_primitive\nlet x = 1\n";
        assert!(
            run_fixture(src)
                .unwrap_err()
                .iter()
                .any(|r| r.contains("no registered manifest primitive")),
            "an unknown requirement fails loudly",
        );
    }

    #[test]
    fn a_drive_fixture_can_raise_from_a_capability() {
        // `resolve-raise:` raises at the capability's call site (E§7.5). Program on line 7.
        let src = "#! clause: X1.1\n#! mode: drive\n#! do: run\n\
                   #! expect: suspended read_line @ 7:1\n#! resolve-raise: \"eof\"\n\
                   #! expect: raised eof @ 7:1\nread_line()\n";
        assert_eq!(run_fixture(src), Ok(()), "drive raises from the capability");
    }

    fn lex_test(expectations: Vec<Expectation>) -> Test {
        Test {
            id: "L3.6.1-num-001".to_string(),
            clauses: vec!["L3.6.1".to_string()],
            mode: Mode::Static,
            required: Stage::Lex,
            expectations,
            inputs: Vec::new(),
            requires: Vec::new(),
            drive: None,
        }
    }

    fn expect_error(substring: &str, line: u32, column: u32) -> Expectation {
        Expectation::StaticError {
            substring: substring.to_string(),
            pos: Position { line, column },
        }
    }

    fn reasons_of(test: &Test, source: &str) -> Vec<String> {
        execute(test, source, None).unwrap_err()
    }

    #[test]
    fn clean_source_with_no_expectations_passes() {
        assert!(execute(&lex_test(vec![]), "let x = 1 + 2\n", None).is_ok());
    }

    #[test]
    fn matches_a_static_error_by_substring_and_position() {
        let t = lex_test(vec![expect_error("between digits", 1, 1)]);
        assert!(execute(&t, "1__0\n", None).is_ok());
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
        assert!(execute(&t, "42\n", None).is_err());
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
            inputs: Vec::new(),
            requires: Vec::new(),
            drive: None,
        }
    }

    #[test]
    fn parse_stage_matches_a_syntax_error() {
        // `1 = 2` is a parse-stage static error (a non-lvalue assignment target)
        // the lexer alone would never surface — so this exercises the parser arm.
        let t = parse_test(vec![expect_error("the left side of", 1, 1)]);
        assert!(execute(&t, "1 = 2\n", None).is_ok());
    }

    #[test]
    fn parse_stage_clean_source_passes() {
        assert!(execute(&parse_test(vec![]), "let x = 1\nx = x + 1\n", None).is_ok());
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
            inputs: Vec::new(),
            requires: Vec::new(),
            drive: None,
        }
    }

    #[test]
    fn full_stage_matches_a_resolver_error() {
        // Reassigning a `const` is a resolver-stage static error that lex+parse
        // alone never surface — so this exercises the resolver (full) arm.
        let t = full_test(vec![expect_error("can't assign to", 2, 1)]);
        assert!(execute(&t, "const c = 1\nc = 2\n", None).is_ok());
    }

    #[test]
    fn full_stage_clean_source_passes() {
        assert!(execute(&full_test(vec![]), "fn f()\n  1\nend\n", None).is_ok());
    }
}
