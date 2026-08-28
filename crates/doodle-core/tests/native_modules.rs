//! Native module tests (M5.4a, engine spec E§5.5, S-44): a host registers native modules
//! (foreign functions + constants) before the first load; `import`ing one finds it ahead of
//! source lookup and its members are reached exactly like a Doodle module's — `m.member`,
//! wildcards, and cross-module calls. Foreign-value and record members are M5.4b.

use doodle_core::diag::Severity;
use doodle_core::drive::{Directive, Outcome, run};
use doodle_core::machine::{
    ConstValue, HostError, Instance, NativeModule, Registry, length_intrinsic, print_intrinsic,
};
use doodle_core::parse::parse_program;
use doodle_core::resolve::resolve as resolve_module;
use doodle_core::source::normalize;
use doodle_core::span::ModuleId;

/// A `util` native module: a constant `answer` (42), a constant `greeting` ("hi"), and a
/// foreign function `size` (the engine's `length`).
fn util_module() -> NativeModule {
    NativeModule::new("util")
        .constant("answer", ConstValue::Int(42))
        .constant("greeting", ConstValue::Str("hi".into()))
        .function("size", length_intrinsic())
}

/// Loads `main` with `print` and the given native modules registered, asserting it compiles
/// clean, and runs it to completion, returning its output.
fn run_with_natives(main: &str, natives: Vec<NativeModule>) -> String {
    let nfc = normalize(main);
    let parsed = parse_program(nfc.as_ref(), ModuleId(0));
    assert!(
        parsed
            .diagnostics
            .iter()
            .all(|d| d.severity != Severity::Error),
        "parse error(s): {:?}",
        parsed.diagnostics
    );
    let resolved = resolve_module(parsed.ast, parsed.root, ModuleId(0));
    assert!(
        resolved.diagnostics.is_empty(),
        "{:?}",
        resolved.diagnostics
    );
    let mut registry = Registry::new();
    registry.register(print_intrinsic()).unwrap();
    for native in natives {
        registry.register_module(native).unwrap();
    }
    let mut inst = Instance::load_with_intrinsics(resolved.module, registry);
    let outcome = run(&mut inst, Directive::RunToCompletion);
    assert!(
        matches!(outcome, Outcome::Completed(_)),
        "expected clean completion, got {outcome:?}"
    );
    String::from_utf8(inst.output().to_vec()).expect("utf-8 output")
}

#[test]
fn a_native_module_binds_and_its_members_are_reached() {
    let src = "\
import util
print(util.answer)
print(util.size(\"hello\"))
print(util.greeting)
";
    assert_eq!(run_with_natives(src, vec![util_module()]), "42\n5\nhi\n");
}

#[test]
fn a_native_module_import_does_not_suspend_for_source() {
    // The import resolves internally (pre-loaded), so a bundling host is never consulted —
    // a plain `run` to completion (no import-resolution loop) suffices.
    let src = "import util\nprint(util.answer)\n";
    assert_eq!(run_with_natives(src, vec![util_module()]), "42\n");
}

#[test]
fn a_wildcard_import_of_a_native_module_brings_its_members_in() {
    let src = "\
import util.*
print(answer)
print(size(\"abcd\"))
";
    assert_eq!(run_with_natives(src, vec![util_module()]), "42\n4\n");
}

#[test]
fn a_missing_member_of_a_native_module_raises() {
    let src = "import util\nprint(util.nope)\n";
    let nfc = normalize(src);
    let parsed = parse_program(nfc.as_ref(), ModuleId(0));
    let resolved = resolve_module(parsed.ast, parsed.root, ModuleId(0));
    let mut registry = Registry::new();
    registry.register(print_intrinsic()).unwrap();
    registry.register_module(util_module()).unwrap();
    let mut inst = Instance::load_with_intrinsics(resolved.module, registry);
    let outcome = run(&mut inst, Directive::RunToCompletion);
    let Outcome::Raised(value, _) = outcome else {
        panic!("expected a raise, got {outcome:?}");
    };
    let (kind, _message) = inst.describe_raised(value);
    assert_eq!(kind, "no-such-field");
}

#[test]
fn registering_two_native_modules_with_one_name_is_a_host_error() {
    let mut registry = Registry::new();
    registry.register_module(NativeModule::new("dup")).unwrap();
    let err = registry
        .register_module(NativeModule::new("dup"))
        .unwrap_err();
    assert_eq!(err, HostError::DuplicateModule("dup".into()));
}

#[test]
fn a_native_record_member_constructs_and_tests_with_is() {
    // A record-type member: `shapes.Point` constructs instances and `x is shapes.Point`
    // tests against it (nominal identity, L§6.5), all through the ordinary record machinery.
    let src = "\
import shapes
let p = shapes.Point(x: 1, y: 2)
print(p.x)
print(p is shapes.Point)
print(5 is shapes.Point)
";
    let shapes = NativeModule::new("shapes").record("Point", vec!["x".into(), "y".into()], false);
    assert_eq!(run_with_natives(src, vec![shapes]), "1\ntrue\nfalse\n");
}

#[test]
fn a_native_foreign_value_member_binds_and_its_finalizer_runs_once() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    // A foreign-value member binds like any member; its exactly-once finalizer runs when the
    // instance is dropped (E§4.5).
    let finalized = Arc::new(AtomicU32::new(0));
    let flag = Arc::clone(&finalized);
    let host = NativeModule::new("host").foreign(
        "handle",
        7,
        0,
        Some(Box::new(move |_ptr| {
            flag.fetch_add(1, Ordering::Relaxed);
        })),
    );
    let src = "import host\nlet h = host.handle\n";
    // Bind and run: the member is reachable without error.
    assert_eq!(run_with_natives(src, vec![host]), "");
    // `run_with_natives` drops the instance at its end, running the finalizer exactly once.
    assert_eq!(finalized.load(Ordering::Relaxed), 1);
}

#[test]
fn a_second_native_module_gets_its_own_namespace() {
    let src = "\
import util
import extra
print(util.answer)
print(extra.tag)
";
    let extra = NativeModule::new("extra").constant("tag", ConstValue::Str("X".into()));
    assert_eq!(run_with_natives(src, vec![util_module(), extra]), "42\nX\n");
}
