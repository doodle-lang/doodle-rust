//! Drive-loop smoke tests: load real Doodle source through the front end, drive
//! the machine to completion, and check the outcome through the public API.
//!
//! A top-level module runs for effect, so it completes **Void** (`Completed(None)`)
//! regardless of its final statement's value (L§6.11; E§7.2 — the value is present
//! only for a returning `fn`). This is the resolution of the M0.3 provisional,
//! which returned the last expression's value.

use doodle_core::diag::Severity;
use doodle_core::drive::{Directive, Outcome, run};
use doodle_core::machine::{Instance, InstanceState};
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
