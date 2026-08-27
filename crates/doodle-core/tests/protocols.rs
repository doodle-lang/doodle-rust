//! Protocol dispatch tests (M5.5a): `protocol`/`implement`, single dispatch on the first
//! argument's runtime type (L§10.3, S-31), protocol defaults, the qualified form
//! `P.member`, `x is P`, and the `protocol-not-implemented` / `ambiguous-member` errors.
//! Single-module programs driven through the public API, observing `print` output and the
//! raised `Error.kind`. The `extends` chain and static conformance checks are M5.5b.

use doodle_core::diag::Severity;
use doodle_core::drive::{Directive, Outcome, run};
use doodle_core::machine::{Instance, Registry, print_intrinsic, read_line_intrinsic};
use doodle_core::parse::parse_program;
use doodle_core::resolve::resolve as resolve_module;
use doodle_core::source::normalize;
use doodle_core::span::ModuleId;

/// Loads `main` as the entry module with the `print` and `read_line` intrinsics, asserting
/// it compiles clean (no parse/resolve errors).
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

/// Runs `main` to completion, asserting it completes cleanly, and returns its output.
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

const SPEAKER: &str = "\
record Dog with name end
record Cat with name end
protocol Speaker
    fn sound(self)
    end
end
implement Speaker for Dog
    fn sound(d)
        return \"woof\"
    end
end
implement Speaker for Cat
    fn sound(c)
        return \"meow\"
    end
end
";

#[test]
fn dispatch_picks_the_implementation_by_first_argument_type() {
    let src =
        format!("{SPEAKER}print(sound(Dog(name: \"Rex\")))\nprint(sound(Cat(name: \"Tom\")))\n");
    assert_eq!(run_output(&src), "woof\nmeow\n");
}

#[test]
fn a_default_is_used_when_a_member_is_left_unimplemented() {
    let src = "\
protocol Greeter
    fn greeting(self)
        return \"hello\"
    end
end
record Robot with id end
implement Greeter for Robot
end
print(greeting(Robot(id: 1)))
";
    assert_eq!(run_output(src), "hello\n");
}

#[test]
fn an_override_beats_the_default() {
    let src = "\
protocol Greeter
    fn greeting(self)
        return \"hello\"
    end
end
record Loud with n end
implement Greeter for Loud
    fn greeting(x)
        return \"HELLO\"
    end
end
print(greeting(Loud(n: 1)))
";
    assert_eq!(run_output(src), "HELLO\n");
}

#[test]
fn a_type_that_does_not_implement_the_protocol_raises_not_implemented() {
    let src = "\
protocol Speaker
    fn sound(self)
    end
end
print(sound(5))
";
    let (kind, message) = run_raise(src);
    assert_eq!(kind, "protocol-not-implemented");
    assert!(message.contains("Int"), "names the type: {message}");
    assert!(message.contains("Speaker"), "names the protocol: {message}");
    assert!(message.contains("sound"), "names the member: {message}");
    assert!(
        message.contains("implement Speaker for Int"),
        "points at the fix: {message}"
    );
}

#[test]
fn a_member_supplied_by_two_implemented_protocols_is_ambiguous() {
    let src = "\
protocol Collection
    fn size(self)
    end
end
protocol Shape
    fn size(self)
    end
end
record Box with w end
implement Collection for Box
    fn size(b)
        return 1
    end
end
implement Shape for Box
    fn size(b)
        return 2
    end
end
print(size(Box(w: 3)))
";
    let (kind, message) = run_raise(src);
    assert_eq!(kind, "ambiguous-member");
    assert!(message.contains("size"), "names the member: {message}");
    assert!(
        message.contains("Collection"),
        "names protocol a: {message}"
    );
    assert!(message.contains("Shape"), "names protocol b: {message}");
}

#[test]
fn the_qualified_form_disambiguates() {
    let src = "\
protocol Collection
    fn size(self)
    end
end
protocol Shape
    fn size(self)
    end
end
record Box with w end
implement Collection for Box
    fn size(b)
        return 1
    end
end
implement Shape for Box
    fn size(b)
        return 2
    end
end
print(Collection.size(Box(w: 3)))
print(Shape.size(Box(w: 3)))
";
    assert_eq!(run_output(src), "1\n2\n");
}

#[test]
fn dispatch_finds_the_first_argument_passed_by_keyword() {
    // S-31: the dispatch argument is the value bound to the member's first parameter,
    // however it arrived — here by its keyword name `self` (the protocol's, not the impl's).
    let src = format!("{SPEAKER}print(sound(self: Dog(name: \"Rex\")))\n");
    assert_eq!(run_output(&src), "woof\n");
}

#[test]
fn is_holds_exactly_for_implementing_types() {
    let src = format!(
        "{SPEAKER}\
print(Dog(name: \"Rex\") is Speaker)
print(Cat(name: \"Tom\") is Speaker)
print(5 is Speaker)
"
    );
    assert_eq!(run_output(&src), "true\ntrue\nfalse\n");
}

#[test]
fn dispatch_drives_a_block_argument() {
    // A member taking a `do … end` block (L§8.5) dispatched to an implementation that
    // invokes it: the value-producing member `each` visits the range's elements.
    let src = "\
protocol Iterable
    to each(self, do body)
    end
end
record Span with lo, hi end
implement Iterable for Span
    to each(s, do body)
        body(s.lo)
        body(s.lo + 1)
    end
end
each(Span(lo: 10, hi: 12)) do(i)
    print(i)
end
";
    assert_eq!(run_output(src), "10\n11\n");
}

#[test]
fn a_required_member_left_out_of_the_implementation_raises() {
    // M5.5a: an `implement` block that omits a required member (no default) resolves to
    // not-implemented at the call — the static missing-member check is M5.5b.
    let src = "\
protocol Pair
    fn first(self)
    end
    fn second(self)
    end
end
record P with a, b end
implement Pair for P
    fn first(p)
        return p.a
    end
end
print(second(P(a: 1, b: 2)))
";
    let (kind, _message) = run_raise(src);
    assert_eq!(kind, "protocol-not-implemented");
}

// --- M5.5b: the `extends` chain (S-61) ---

/// A three-deep chain `Child extends Parent extends Grand`: one `implement Child for T`
/// block covers the chain; inherited members fall to their nearest default, the required
/// one to the implementation.
#[test]
fn an_extends_chain_resolves_members_along_the_whole_chain() {
    let src = "\
protocol Grand
    fn g(self)
        return \"grand-g\"
    end
end
protocol Parent extends Grand
    fn p(self)
        return \"parent-p\"
    end
end
protocol Child extends Parent
    fn c(self)
    end
end
record T with x end
implement Child for T
    fn c(t)
        return \"child-c\"
    end
end
print(g(T(x: 1)))
print(p(T(x: 1)))
print(c(T(x: 1)))
";
    assert_eq!(run_output(src), "grand-g\nparent-p\nchild-c\n");
}

/// The nearest declaring protocol's default wins over an ancestor's (S-61); an explicit
/// implementation beats every default.
#[test]
fn the_nearest_default_wins_and_an_implementation_beats_all() {
    let base = "\
protocol Grand
    fn describe(self)
        return \"grand\"
    end
end
protocol Child extends Grand
    fn describe(self)
        return \"child\"
    end
end
record T with x end
";
    // Child re-declares `describe` with its own default — nearest wins over Grand's.
    let inherited = format!("{base}implement Child for T\nend\nprint(describe(T(x: 1)))\n");
    assert_eq!(run_output(&inherited), "child\n");
    // An explicit implementation of `describe` beats even the nearest default.
    let overridden = format!(
        "{base}\
implement Child for T
    fn describe(t)
        return \"impl\"
    end
end
print(describe(T(x: 1)))
"
    );
    assert_eq!(run_output(&overridden), "impl\n");
}

/// `x is Parent` holds for a type that implements a `Child` (requirements are transitive
/// along `extends`, S-61).
#[test]
fn is_is_transitive_along_the_extends_chain() {
    let src = "\
protocol Grand
    fn g(self)
    end
end
protocol Parent extends Grand
    fn p(self)
    end
end
record T with x end
implement Parent for T
    fn g(t)
        return 1
    end
    fn p(t)
        return 2
    end
end
print(T(x: 1) is Parent)
print(T(x: 1) is Grand)
print(5 is Grand)
";
    assert_eq!(run_output(src), "true\ntrue\nfalse\n");
}

/// An inherited required member the implementation omits (no default anywhere in the chain)
/// raises `protocol-not-implemented` at the call — the runtime face of "extends parent
/// requirements enforced".
#[test]
fn an_unimplemented_inherited_requirement_raises_at_the_call() {
    let src = "\
protocol Base
    fn need(self)
    end
end
protocol Derived extends Base
    fn extra(self)
    end
end
record T with x end
implement Derived for T
    fn extra(t)
        return 1
    end
end
print(need(T(x: 1)))
";
    let (kind, _message) = run_raise(src);
    assert_eq!(kind, "protocol-not-implemented");
}

/// `extends` referencing a name that is not an already-defined protocol raises at load — a
/// forward or self reference reads an uninitialized cell, so a chain is acyclic and
/// parent-first by construction (an `extends` cycle is unconstructable).
#[test]
fn extends_of_an_undefined_protocol_raises() {
    let src = "\
protocol Child extends Parent
    fn c(self)
    end
end
";
    let (kind, _message) = run_raise(src);
    // `Parent` is declared nowhere — a free name with no binding.
    assert_eq!(kind, "name-not-defined");
}
