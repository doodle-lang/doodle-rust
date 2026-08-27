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
