//! Unit tests for the intrinsic foreign-function mechanism (registry, synchronous
//! dispatch, `print`), driven through the real front end. Split from `mod.rs` so that
//! file stays within the hygiene length limit.

use super::*;
use crate::drive::{Directive, Outcome, run};
use crate::machine::Instance;
use crate::span::ModuleId;

/// An `fn` intrinsic returning `42`, for testing value-yielding foreign calls.
fn answer() -> Intrinsic {
    Intrinsic {
        name: "answer".into(),
        kind: BodyKind::Func,
        params: Vec::new(),
        body: ForeignBody::Sync(|_ctx| Ok(Some(Value::Int(42)))),
    }
}

/// Loads `src` (which must load clean) with `registry`, driving it to completion
/// and returning the instance so the caller can read its output/outcome.
fn run_with(src: &str, registry: Registry) -> (Instance, Outcome) {
    use crate::diag::Severity;
    let nfc = crate::source::normalize(src);
    let parsed = crate::parse::parse_program(nfc.as_ref(), ModuleId(0));
    assert!(
        !parsed
            .diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error),
        "parse error(s): {:?}",
        parsed.diagnostics
    );
    let resolved = crate::resolve::resolve(parsed.ast, parsed.root, ModuleId(0));
    assert!(
        resolved.diagnostics.is_empty(),
        "resolve diagnostic(s): {:?}",
        resolved.diagnostics
    );
    let mut inst = Instance::load_with_intrinsics(resolved.module, registry);
    let outcome = run(&mut inst, Directive::RunToCompletion);
    (inst, outcome)
}

fn registry_with(intrinsics: Vec<Intrinsic>) -> Registry {
    let mut r = Registry::new();
    for i in intrinsics {
        r.register(i).unwrap();
    }
    r
}

#[test]
fn register_rejects_a_duplicate_name() {
    let mut r = Registry::new();
    r.register(print()).unwrap();
    assert_eq!(
        r.register(print()),
        Err(HostError::DuplicateIntrinsic("print".into()))
    );
}

#[test]
fn register_rejects_a_builtin_type_value_name() {
    let mut r = Registry::new();
    let shadow_int = Intrinsic {
        name: "Int".into(),
        kind: BodyKind::Func,
        params: Vec::new(),
        body: ForeignBody::Sync(|_| Ok(Some(Value::Nil))),
    };
    assert_eq!(
        r.register(shadow_int),
        Err(HostError::CollidesWithBuiltin("Int".into()))
    );
}

#[test]
fn print_renders_its_argument_and_appends_a_newline() {
    let (inst, outcome) = run_with("print(1 + 2)\n", registry_with(vec![print()]));
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"3\n");
}

#[test]
fn print_renders_each_demo_scalar_kind() {
    let (inst, _) = run_with(
        "print(nil)\nprint(true)\nprint(-7)\nprint(1.5)\nprint(\"hi\")\n",
        registry_with(vec![print()]),
    );
    assert_eq!(inst.output(), b"nil\ntrue\n-7\n1.5\nhi\n");
}

#[test]
fn an_fn_intrinsic_yields_a_value_the_call_consumes() {
    let (inst, _) = run_with("print(answer())\n", registry_with(vec![print(), answer()]));
    assert_eq!(inst.output(), b"42\n");
}

#[test]
fn a_user_declaration_shadows_an_intrinsic() {
    // The program declares its own `print` (a no-op `to`), so the intrinsic never
    // runs — a user global is found first in the namespace scan (S-43 order).
    let (inst, outcome) = run_with(
        "to print(x)\nx\nend\nprint(\"hi\")\n",
        registry_with(vec![print()]),
    );
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"", "the intrinsic print was shadowed");
}

#[test]
fn a_missing_argument_raises() {
    let (_, outcome) = run_with("print()\n", registry_with(vec![print()]));
    assert!(matches!(outcome, Outcome::Raised(..)), "{outcome:?}");
}

#[test]
fn a_block_passed_to_a_block_less_intrinsic_raises() {
    // Parity with a source callable that takes no block (block.rs): passing a
    // `do … end` to `print` raises rather than silently dropping the block.
    let (_, outcome) = run_with("print(1) do\n1\nend\n", registry_with(vec![print()]));
    assert!(matches!(outcome, Outcome::Raised(..)), "{outcome:?}");
}

#[test]
fn a_to_intrinsic_result_used_as_a_value_raises() {
    // `print` is a `to` (Void); consuming its result in an expression raises at
    // the consuming site (the runtime Void backstop — the resolver's static
    // voidcheck only knows current-module `to`s, not intrinsics).
    let (_, outcome) = run_with("print(1) + 1\n", registry_with(vec![print()]));
    assert!(matches!(outcome, Outcome::Raised(..)), "{outcome:?}");
}
