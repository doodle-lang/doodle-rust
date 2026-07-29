//! Drive-loop smoke tests: load real Doodle source through the front end, drive
//! the machine to completion, and check the outcome through the public API.
//!
//! A top-level module runs for effect, so it completes **Void** (`Completed(None)`)
//! regardless of its final statement's value (L§6.11; E§7.2 — the value is present
//! only for a returning `fn`). This is the resolution of the M0.3 provisional,
//! which returned the last expression's value.

use doodle_core::diag::Severity;
use doodle_core::drive::{Directive, Outcome, run};
use doodle_core::machine::{ExceptionKind, Instance, InstanceState};
use doodle_core::parse::parse_program;
use doodle_core::resolve::resolve;
use doodle_core::source::normalize;
use doodle_core::span::ModuleId;

/// Loads Doodle `src` into an instance through the real pipeline (normalize →
/// parse → resolve), asserting it loads clean.
fn instance(src: &str) -> Instance {
    let nfc = normalize(src);
    let parsed = parse_program(nfc.as_ref(), ModuleId(0));
    assert!(
        parsed
            .diagnostics
            .iter()
            .all(|d| d.severity != Severity::Error),
        "unexpected parse error(s): {:?}",
        parsed.diagnostics
    );
    let resolved = resolve(parsed.ast, parsed.root, ModuleId(0));
    assert!(
        resolved.diagnostics.is_empty(),
        "unexpected resolve diagnostic(s): {:?}",
        resolved.diagnostics
    );
    Instance::load(resolved.module)
}

/// A single literal expression statement drives to `Completed(None)` — the
/// top-level module yields Void, and the result register reads Void.
#[test]
fn drives_a_literal_statement_to_void_completion() {
    let mut inst = instance("42\n");
    assert_eq!(inst.state(), InstanceState::Ready);

    let outcome = run(&mut inst, Directive::RunToCompletion);

    assert_eq!(inst.state(), InstanceState::Completed);
    assert!(matches!(outcome, Outcome::Completed(None)));
    // `Value` has no `PartialEq` (machine-design §3); inspect the option directly.
    assert!(inst.result().is_none());
}

/// An empty module body drives to `Completed(None)`.
#[test]
fn drives_an_empty_module_to_void_completion() {
    let mut inst = instance("");
    let outcome = run(&mut inst, Directive::RunToCompletion);

    assert_eq!(inst.state(), InstanceState::Completed);
    assert!(matches!(outcome, Outcome::Completed(None)));
    assert!(inst.result().is_none());
}

/// Several statements of different literal kinds (including a heap-allocated
/// bytes literal) sequence and drive to Void completion.
#[test]
fn drives_a_multi_statement_program_to_void_completion() {
    let mut inst = instance("nil\ntrue\nb\"hi\"\n");
    let outcome = run(&mut inst, Directive::RunToCompletion);

    assert_eq!(inst.state(), InstanceState::Completed);
    assert!(matches!(outcome, Outcome::Completed(None)));
    assert!(inst.result().is_none());
}

/// Asserts driving `src` to completion raises an uncaught exception of `kind`.
fn assert_raises(src: &str, kind: ExceptionKind) {
    let mut inst = instance(src);
    match run(&mut inst, Directive::RunToCompletion) {
        Outcome::Raised(exception, _trace) => assert_eq!(exception.kind, kind),
        other => panic!("expected Raised({kind:?}), got {other:?}"),
    }
}

/// A runtime type mismatch (`1 + true`) has no handler yet, so it surfaces as
/// `Raised`.
#[test]
fn a_type_error_surfaces_as_an_uncaught_raise() {
    assert_raises("1 + true\n", ExceptionKind::TypeMismatch);
}

/// Division by zero raises (L§4.2).
#[test]
fn division_by_zero_surfaces_as_an_uncaught_raise() {
    assert_raises("1 / 0\n", ExceptionKind::DivisionByZero);
}

/// A float operation whose result would be nonfinite raises (S-56).
#[test]
fn a_nonfinite_float_result_surfaces_as_an_uncaught_raise() {
    assert_raises("1e308 * 10.0\n", ExceptionKind::NonFiniteFloat);
}
