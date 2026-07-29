//! Unit tests for the machine over real Doodle source, driven through the front
//! end. Split from `machine.rs` (the production code) so that file stays within
//! the hygiene length limit; these use the crate-internal stepping API to observe
//! the result register directly (which the integration tests in `tests/` cannot).

use super::*;

/// Builds an instance from Doodle source through the real front end, asserting
/// the program loads clean (no lex/parse/resolve diagnostics).
fn load_source(src: &str) -> Instance {
    use crate::diag::Severity;
    let nfc = crate::source::normalize(src);
    let parsed = crate::parse::parse_program(nfc.as_ref(), ModuleId(0));
    assert!(
        !parsed
            .diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error),
        "unexpected parse error(s): {:?}",
        parsed.diagnostics
    );
    let resolved = crate::resolve::resolve(parsed.ast, parsed.root, ModuleId(0));
    assert!(
        resolved.diagnostics.is_empty(),
        "unexpected resolve diagnostic(s): {:?}",
        resolved.diagnostics
    );
    Instance::load(resolved.module)
}

/// Advances until a value lands in the register or the machine halts.
fn step_to_first_value(inst: &mut Instance) {
    let mut steps = 0;
    while inst.result().is_none() && !inst.is_halted() {
        inst.step().expect("unexpected raise");
        steps += 1;
        assert!(steps < 1000, "machine failed to produce a value");
    }
}

/// Advances until the register holds `Int(want)`, failing if the machine
/// halts first (or runs away).
fn step_until_int(inst: &mut Instance, want: i64) {
    let mut steps = 0;
    while inst.result().and_then(Value::as_int) != Some(want) {
        assert!(
            !inst.is_halted(),
            "halted before the register reached {want}"
        );
        inst.step().expect("unexpected raise");
        steps += 1;
        assert!(steps < 1000, "register never reached {want}");
    }
}

/// Drives to halt, returning the last value the register held before the
/// module returned Void — i.e. the final expression's value.
fn drive_capturing_last_value(inst: &mut Instance) -> Option<Value> {
    let mut last = None;
    let mut steps = 0;
    while !inst.is_halted() {
        if let Some(v) = inst.result() {
            last = Some(v);
        }
        inst.step().expect("unexpected raise");
        steps += 1;
        assert!(steps < 10000, "machine failed to halt");
    }
    last
}

#[test]
fn value_readers_match_only_their_own_variant() {
    assert_eq!(Value::Int(7).as_int(), Some(7));
    assert_eq!(Value::Float(1.5).as_int(), None);
    assert_eq!(Value::Bool(true).as_bool(), Some(true));
    assert_eq!(Value::Int(0).as_bool(), None);
    // Avoid a float `==` (clippy::float_cmp); presence is enough to catch a
    // reader matching the wrong variant.
    assert!(Value::Float(2.5).as_float().is_some());
    assert!(Value::Nil.as_float().is_none());
    assert!(Value::Nil.is_nil());
    assert!(!Value::Int(0).is_nil());
}

#[test]
fn a_fresh_instance_is_ready_and_not_halted() {
    let inst = load_source("1\n");
    assert_eq!(inst.state(), InstanceState::Ready);
    assert!(!inst.is_halted());
}

#[test]
fn evaluates_an_int_literal_into_the_register() {
    let mut inst = load_source("42\n");
    step_to_first_value(&mut inst);
    assert_eq!(inst.result().and_then(Value::as_int), Some(42));
}

#[test]
fn a_bytes_literal_allocates_on_the_heap() {
    let mut inst = load_source("b\"hi\"\n");
    step_to_first_value(&mut inst);
    assert!(matches!(inst.result(), Some(Value::Bytes(_))));
}

#[test]
fn sequencing_runs_statements_in_order() {
    // Each statement's value lands in the register in turn: `1` then `2`.
    // A skip/miscount bug (e.g. advancing the sequence index by two) would
    // halt before the register ever reaches `2`, failing the second wait —
    // catching what the Void-completion tests alone cannot.
    let mut inst = load_source("1\n2\n");
    step_until_int(&mut inst, 1);
    step_until_int(&mut inst, 2);
}

#[test]
fn a_module_halts_and_completes_void() {
    // Several literal statements: sequencing runs each, and the top-level
    // return discards the final value (a module yields Void, L§6.11).
    let mut inst = load_source("1\ntrue\nnil\n");
    let mut steps = 0;
    while !inst.is_halted() {
        inst.step().expect("unexpected raise");
        steps += 1;
        assert!(steps < 1000, "machine failed to halt");
    }
    assert!(inst.result().is_none());
}

#[test]
fn arithmetic_evaluates_through_the_machine() {
    // Precedence + associativity flow through the continuation stack.
    let mut inst = load_source("2 * 3 + 4\n");
    assert_eq!(
        drive_capturing_last_value(&mut inst).and_then(Value::as_int),
        Some(10)
    );
}

#[test]
fn integer_overflow_promotes_to_bigint_through_the_machine() {
    let mut inst = load_source("9223372036854775807 + 1\n");
    assert!(matches!(
        drive_capturing_last_value(&mut inst),
        Some(Value::BigInt(_))
    ));
}

#[test]
fn comparison_and_boolean_ops_evaluate_through_the_machine() {
    for (src, expected) in [
        ("3 < 5\n", true),
        ("5 <= 5\n", true),
        ("2 != 2\n", false),
        ("1 == 1.0\n", true),
        ("true and false\n", false),
        ("false or true\n", true),
        ("not false\n", true),
    ] {
        let mut inst = load_source(src);
        let got = drive_capturing_last_value(&mut inst);
        assert!(
            matches!(got, Some(Value::Bool(b)) if b == expected),
            "{src:?} should evaluate to {expected}, got {got:?}"
        );
    }
}

#[test]
fn module_bindings_read_and_write_through_the_machine() {
    for (src, want) in [
        ("let x = 1\nx + 1\n", 2),        // read a `let`
        ("let x = 1\nx = x + 1\nx\n", 2), // reassign a `let`
        ("const c = 5\nc * 2\n", 10),     // read a `const`
    ] {
        let mut inst = load_source(src);
        assert_eq!(
            drive_capturing_last_value(&mut inst).and_then(Value::as_int),
            Some(want),
            "{src:?}"
        );
    }
}

#[test]
fn control_flow_evaluates_through_the_machine() {
    for (src, want) in [
        ("let x = if 3 > 2 then 10 else 20 end\nx\n", 10), // if-expr, true arm
        ("let x = if 2 > 3 then 10 else 20 end\nx\n", 20), // if-expr, else
        (
            "let s = if 5 < 0 then 1 else if 5 > 0 then 2 else 3 end\ns\n",
            2,
        ), // else-if
        ("let x = 0\nif 3 > 2 then x = 5 end\nx\n", 5),    // statement if mutates a global
        ("let n = 0\nwhile n < 3 do n = n + 1 end\nn\n", 3), // while counter
        // A construct-body local (`step`, a frame slot) summed across the loop.
        (
            "let t = 0\nlet i = 0\nwhile i < 3 do\n\
             let step = i + 1\nt = t + step\ni = i + 1\nend\nt\n",
            6,
        ),
    ] {
        let mut inst = load_source(src);
        assert_eq!(
            drive_capturing_last_value(&mut inst).and_then(Value::as_int),
            Some(want),
            "{src:?}"
        );
    }
}

#[test]
fn a_loop_keeps_running_until_something_stops_it() {
    // `break`/`return`/limits arrive later (M2a.6/M2a.9); the reloop mechanism
    // itself just cycles the body forever.
    let mut inst = load_source("loop do 1 end\n");
    for _ in 0..500 {
        assert!(!inst.is_halted());
        inst.step().expect("unexpected raise");
    }
    assert!(!inst.is_halted());
}

#[test]
fn calls_evaluate_through_the_machine() {
    for (src, want) in [
        ("fn add(a, b) a + b end\nadd(2, 3)\n", 5), // a plain function call
        // A call tree: nested calls as arguments, evaluated before the outer.
        (
            "fn double(x) x * 2 end\nfn add(a, b) a + b end\nadd(double(3), double(4))\n",
            14,
        ),
        ("fn sub(a, b) a - b end\nsub(b: 1, a: 10)\n", 9), // keyword args, out of order
        ("fn sub(a, b) a - b end\nsub(10, b: 3)\n", 7),    // positional + keyword
        ("fn f(x, y = 10) x + y end\nf(5)\n", 15),         // default supplies an omitted param
        ("fn f(x, y = 10) x + y end\nf(5, 1)\n", 6),       // an explicit arg overrides the default
        ("fn f(x, y = 10) x + y end\nf(x: 1, y: 2)\n", 3), // all by keyword
        // Non-tail recursion: `fib` calls itself twice per frame (a call tree).
        (
            "fn fib(n) if n < 2 then n else fib(n - 1) + fib(n - 2) end end\nfib(10)\n",
            55,
        ),
        ("let g = fn(x) x + 1 end\ng(41)\n", 42), // an anonymous function value, then called
        // A procedure mutates a module global across calls; its result is Void.
        ("let c = 0\nto bump() c = c + 1 end\nbump()\nbump()\nc\n", 2),
    ] {
        let mut inst = load_source(src);
        assert_eq!(
            drive_capturing_last_value(&mut inst).and_then(Value::as_int),
            Some(want),
            "{src:?}"
        );
    }
}

#[test]
fn the_is_operator_classifies_values() {
    for (src, expected) in [
        ("5 is Int\n", true),
        ("5 is Number\n", true),
        ("5 is Float\n", false),
        ("5 is Bool\n", false),
        ("3.0 is Float\n", true),
        ("3.0 is Number\n", true),
        ("true is Bool\n", true),
        ("nil is Nil\n", true),
        ("b\"hi\" is Bytes\n", true),
        ("let f = fn(x) x end\nf is Procedure\n", true),
        // Integer beyond i64 is a promoted BigInt, still an `Int` (MD §3).
        ("9223372036854775807 + 1 is Int\n", true),
    ] {
        let mut inst = load_source(src);
        let got = drive_capturing_last_value(&mut inst);
        assert!(
            matches!(got, Some(Value::Bool(b)) if b == expected),
            "{src:?} should be {expected}, got {got:?}"
        );
    }
}

#[test]
fn a_callable_is_one_canonical_value_across_reads() {
    // A plain `to`/`fn` interns one `CalObj` (MD §8): every read of the name
    // yields the same slab index (callable equality is identity, L§4.9).
    let mut inst = load_source("fn f(x) x end\nf\nf\n");
    let mut seen = Vec::new();
    while !inst.is_halted() {
        if let Some(Value::Callable(c)) = inst.result() {
            seen.push(c.0);
        }
        inst.step().expect("unexpected raise");
    }
    assert!(seen.len() >= 2, "expected to observe the callable twice");
    assert!(
        seen.iter().all(|&c| c == seen[0]),
        "each read of `f` must be the same canonical callable: {seen:?}"
    );
}
