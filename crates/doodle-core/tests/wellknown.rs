//! Well-known protocol tests (M5.7, L§15, D-M5-1): real `Stringable`/`Hashable` dispatch.
//! String interpolation drives an explicit `implement Stringable` (a real, can-raise call) and
//! falls to the native renderer otherwise; dict keys drive an explicit `implement Hashable`
//! and fall to the native structural hash otherwise. `x is Stringable`/`x is Hashable` reflect
//! the native coverage (all values render; only actually-hashable values hash). Built-in scalar
//! rendering/hashing stays final; compound values keep the M9a placeholder text.

use doodle_core::diag::Severity;
use doodle_core::drive::{Directive, Outcome, run};
use doodle_core::machine::{Instance, Registry, print_intrinsic, read_line_intrinsic};
use doodle_core::parse::parse_program;
use doodle_core::resolve::resolve as resolve_module;
use doodle_core::source::normalize;
use doodle_core::span::ModuleId;

/// Loads `main` as the entry module with the `print`/`read_line` intrinsics, asserting it
/// compiles clean.
fn instance(main: &str) -> Instance {
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
        "resolve diagnostic(s): {:?}",
        resolved.diagnostics
    );
    let mut registry = Registry::new();
    registry.register(print_intrinsic()).unwrap();
    registry.register(read_line_intrinsic()).unwrap();
    Instance::load_with_intrinsics(resolved.module, registry)
}

/// Runs `main` to completion, asserting clean completion, and returns its captured output.
fn run_output(main: &str) -> String {
    let mut inst = instance(main);
    let outcome = run(&mut inst, Directive::RunToCompletion);
    assert!(
        matches!(outcome, Outcome::Completed(_)),
        "expected clean completion, got {outcome:?}"
    );
    String::from_utf8(inst.output().to_vec()).expect("utf-8 output")
}

/// Runs `main` expecting a raise, returning the `Error.kind` slug and its message.
fn run_raise(main: &str) -> (String, String) {
    let mut inst = instance(main);
    let outcome = run(&mut inst, Directive::RunToCompletion);
    let Outcome::Raised(value, _) = outcome else {
        panic!("expected a raise, got {outcome:?}");
    };
    let (kind, message) = inst.describe_raised(value);
    (kind.to_string(), message.to_string())
}

// --- Stringable ---

#[test]
fn interpolation_drives_an_explicit_stringable() {
    let out = run_output(
        "record Point with x, y end\n\
         implement Stringable for Point\n\
         \x20   fn to_string(self)\n\
         \x20       return \"P({self.x}, {self.y})\"\n\
         \x20   end\n\
         end\n\
         let p = Point(x: 1, y: 2)\n\
         print(\"here: {p}\")\n",
    );
    assert_eq!(out, "here: P(1, 2)\n");
}

#[test]
fn scalars_render_natively_through_dispatch() {
    // No implementations: the native default renders each scalar to its final form.
    let out = run_output("print(\"{5} {true} {nil} {3.5} {\"hi\"}\")\n");
    assert_eq!(out, "5 true nil 3.5 hi\n");
}

#[test]
fn a_record_without_stringable_keeps_the_placeholder() {
    // Compound render text is deferred to M9a (D-M5-1): a record with no `implement
    // Stringable` still renders the provisional placeholder, not an error.
    let out = run_output("record Q with a end\nlet q = Q(a: 1)\nprint(\"{q}\")\n");
    assert_eq!(out, "<record>\n");
}

#[test]
fn a_bare_to_string_call_uses_the_native_default() {
    // `to_string` is a real callable name; on a built-in with no impl it renders natively.
    let out = run_output("print(to_string(42))\n");
    assert_eq!(out, "42\n");
}

#[test]
fn the_qualified_form_drives_the_implementation() {
    let out = run_output(
        "record Point with x, y end\n\
         implement Stringable for Point\n\
         \x20   fn to_string(self)\n\
         \x20       return \"({self.x},{self.y})\"\n\
         \x20   end\n\
         end\n\
         let p = Point(x: 7, y: 8)\n\
         print(Stringable.to_string(p))\n",
    );
    assert_eq!(out, "(7,8)\n");
}

#[test]
fn a_to_string_that_returns_a_non_string_raises() {
    let (kind, message) = run_raise(
        "record Point with x, y end\n\
         implement Stringable for Point\n\
         \x20   fn to_string(self)\n\
         \x20       return 5\n\
         \x20   end\n\
         end\n\
         let p = Point(x: 1, y: 2)\n\
         print(\"{p}\")\n",
    );
    assert_eq!(kind, "type-mismatch");
    assert!(message.contains("String"), "{message}");
}

#[test]
fn a_local_to_string_does_not_change_interpolation() {
    // Interpolation invokes the member directly (S-37): shadowing the name `to_string` with a
    // local can't hijack `{expr}`.
    let out = run_output(
        "record Point with x, y end\n\
         implement Stringable for Point\n\
         \x20   fn to_string(self)\n\
         \x20       return \"real\"\n\
         \x20   end\n\
         end\n\
         let to_string = 99\n\
         let p = Point(x: 1, y: 2)\n\
         print(\"{p}\")\n",
    );
    assert_eq!(out, "real\n");
}

// --- `is` over the well-known protocols (native coverage, D-M5-1) ---

#[test]
fn every_value_is_stringable() {
    let out = run_output(
        "record Q with a end\n\
         let q = Q(a: 1)\n\
         print(\"{5 is Stringable} {[1, 2] is Stringable} {q is Stringable}\")\n",
    );
    assert_eq!(out, "true true true\n");
}

#[test]
fn is_hashable_is_value_aware() {
    // Scalars and value records with hashable fields are Hashable; a list is not.
    let out = run_output(
        "record Pair with a, b end\n\
         let p = Pair(a: 1, b: 2)\n\
         print(\"{5 is Hashable} {\"s\" is Hashable} {[1] is Hashable} {p is Hashable}\")\n",
    );
    assert_eq!(out, "true true false true\n");
}

#[test]
fn a_record_with_a_list_field_is_not_hashable() {
    let out = run_output(
        "record Box with items end\n\
         let b = Box(items: [1, 2])\n\
         print(\"{b is Hashable}\")\n",
    );
    assert_eq!(out, "false\n");
}

// --- Hashable dispatch for dict keys ---

/// A `Point` whose `hash` returns `self.x`, used across the dict-key tests.
const HASHABLE_POINT: &str = "\
record Point with x, y end
implement Hashable for Point
    fn hash(self)
        return self.x
    end
end
";

#[test]
fn a_dict_drives_an_explicit_hashable_for_assign_and_read() {
    let out = run_output(&format!(
        "{HASHABLE_POINT}\
         let d = {{}}\n\
         d[Point(x: 1, y: 2)] = \"here\"\n\
         print(d[Point(x: 1, y: 2)])\n"
    ));
    assert_eq!(out, "here\n");
}

#[test]
fn a_dict_literal_drives_an_explicit_hashable() {
    let out = run_output(&format!(
        "{HASHABLE_POINT}\
         let d = {{Point(x: 5, y: 0): \"five\", Point(x: 6, y: 0): \"six\"}}\n\
         print(\"{{d[Point(x: 5, y: 0)]}} {{d[Point(x: 6, y: 0)]}}\")\n"
    ));
    assert_eq!(out, "five six\n");
}

#[test]
fn structural_equality_still_separates_hash_collisions() {
    // `hash` returns `self.x`, so these two keys share a bucket; structural `==` (all fields)
    // keeps them distinct entries.
    let out = run_output(&format!(
        "{HASHABLE_POINT}\
         let d = {{}}\n\
         d[Point(x: 1, y: 1)] = \"a\"\n\
         d[Point(x: 1, y: 2)] = \"b\"\n\
         print(\"{{d[Point(x: 1, y: 1)]}} {{d[Point(x: 1, y: 2)]}}\")\n"
    ));
    assert_eq!(out, "a b\n");
}

#[test]
fn a_missing_driven_key_raises_key_not_found() {
    let (kind, _message) = run_raise(&format!(
        "{HASHABLE_POINT}\
         let d = {{}}\n\
         d[Point(x: 1, y: 1)] = \"a\"\n\
         print(d[Point(x: 1, y: 2)])\n"
    ));
    assert_eq!(kind, "key-not-found");
}

#[test]
fn a_hash_that_returns_a_non_int_raises() {
    let (kind, message) = run_raise(
        "record Bad with n end\n\
         implement Hashable for Bad\n\
         \x20   fn hash(self)\n\
         \x20       return \"nope\"\n\
         \x20   end\n\
         end\n\
         let d = {}\n\
         d[Bad(n: 1)] = 5\n",
    );
    assert_eq!(kind, "type-mismatch");
    assert!(message.contains("Int"), "{message}");
}

#[test]
fn an_explicit_hashable_makes_a_natively_unhashable_record_a_key() {
    // A record with a list field isn't natively hashable, but an explicit `implement Hashable`
    // (hashing only a scalar field) makes it a usable dict key (D-M5-1).
    let out = run_output(
        "record Box with items, tag end\n\
         implement Hashable for Box\n\
         \x20   fn hash(self)\n\
         \x20       return self.tag\n\
         \x20   end\n\
         end\n\
         let b = Box(items: [1, 2], tag: 7)\n\
         let d = {}\n\
         d[b] = \"ok\"\n\
         print(\"{b is Hashable} {d[b]}\")\n",
    );
    assert_eq!(out, "true ok\n");
}
