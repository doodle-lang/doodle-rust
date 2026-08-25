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
        Outcome::Raised(value, _trace) => {
            let (slug, _) = inst.describe_raised(value);
            assert_eq!(slug, kind.slug());
        }
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

/// `and`/`or` short-circuit: a `false and (…)` / `true or (…)` must not evaluate
/// the right operand, so a failing right operand never raises.
#[test]
fn and_or_short_circuit_past_a_failing_right_operand() {
    for src in ["false and (1 / 0)\n", "true or (1 / 0)\n"] {
        let mut inst = instance(src);
        assert!(
            matches!(
                run(&mut inst, Directive::RunToCompletion),
                Outcome::Completed(None)
            ),
            "{src:?} should short-circuit and complete Void"
        );
    }
}

/// A non-`Bool` operand to a logical operator raises (strict booleans, L§4.3).
#[test]
fn a_non_bool_logical_operand_raises() {
    assert_raises("1 and true\n", ExceptionKind::TypeMismatch);
    assert_raises("not 1\n", ExceptionKind::TypeMismatch);
}

/// Ordering values for which it is undefined raises (L§6.6).
#[test]
fn ordering_an_undefined_type_raises() {
    assert_raises("1 < true\n", ExceptionKind::UndefinedOrdering);
}

/// Reading a module binding before its declaration executes raises (the temporal
/// dead zone) — here `a`'s initializer reads `b`, declared later.
#[test]
fn a_use_before_definition_raises() {
    assert_raises("let a = b\nconst b = 2\n", ExceptionKind::UsedBeforeDefined);
}

/// A reference to a name with no binding at all raises.
#[test]
fn an_undefined_name_raises() {
    assert_raises("nope\n", ExceptionKind::NameNotDefined);
}

/// A non-`Bool` `if`/`while` condition raises (strict booleans, L§4.3) — no
/// truthiness. (The `while` raises on the first check, so it never loops.)
#[test]
fn a_non_bool_condition_raises() {
    assert_raises("if 1 then 2 else 3 end\n", ExceptionKind::TypeMismatch);
    assert_raises("while 1 do nil end\n", ExceptionKind::TypeMismatch);
}

/// A `to`/`fn` call tree drives to Void completion — a top-level module always
/// yields Void (L§6.11), even when it defines and calls procedures/functions.
#[test]
fn a_call_tree_drives_to_void_completion() {
    let mut inst = instance("fn add(a, b) a + b end\nto shout() add(1, 2) end\nshout()\n");
    assert!(matches!(
        run(&mut inst, Directive::RunToCompletion),
        Outcome::Completed(None)
    ));
}

/// Calling something that is not a callable raises (L§6.4).
#[test]
fn calling_a_non_callable_raises() {
    assert_raises("let x = 5\nx()\n", ExceptionKind::NotCallable);
}

/// Using a procedure result where a value is required raises — a `to` yields Void
/// (L§6.11/§8.4). A *direct* call to a known `to` is a static error (S-6); here
/// the proc is reached through a `let`, whose kind the resolver cannot pin, so it
/// is the machine's runtime backstop (`take_value` on a Void register) that fires.
#[test]
fn a_procedure_result_used_as_a_value_raises() {
    assert_raises(
        "to p() 1 end\nlet g = p\nlet x = g()\n",
        ExceptionKind::ProcedureInExpression,
    );
}

/// Each L§8.3 argument-binding mismatch raises an argument error.
#[test]
fn argument_mismatches_raise() {
    // Missing a required argument.
    assert_raises("fn f(a, b) a + b end\nf(1)\n", ExceptionKind::ArgumentError);
    // Too many positional arguments.
    assert_raises("fn f(a) a end\nf(1, 2)\n", ExceptionKind::ArgumentError);
    // An unknown keyword.
    assert_raises("fn f(a) a end\nf(z: 1)\n", ExceptionKind::ArgumentError);
    // The same parameter bound twice (positional then keyword).
    assert_raises("fn f(a) a end\nf(1, a: 2)\n", ExceptionKind::ArgumentError);
}

/// Calling a `to`/`fn` before its declaration statement has executed raises —
/// the temporal dead zone extends to callables (they bind in execution order,
/// M2a.4a); here `f` is called before `to f` runs.
#[test]
fn calling_a_callable_before_its_declaration_raises() {
    assert_raises("f()\nto f() nil end\n", ExceptionKind::UsedBeforeDefined);
}

/// The right operand of `is` must be a type value; a non-type raises (L§6.5).
#[test]
fn is_with_a_non_type_right_operand_raises() {
    assert_raises("5 is 5\n", ExceptionKind::TypeMismatch);
}

/// A `to`/`fn` with a block argument and three-tier exits drives to Void
/// completion — a top-level module always yields Void (L§6.11). Here a bare
/// `break` exits the block-consuming call.
#[test]
fn a_block_program_with_exits_drives_to_void_completion() {
    let mut inst = instance(
        "to each3(do body)\nbody()\nbody()\nbody()\nend\n\
         to go()\neach3() do break end\nend\ngo()\n",
    );
    assert!(matches!(
        run(&mut inst, Directive::RunToCompletion),
        Outcome::Completed(None)
    ));
}

/// The open S-10 to-consumer half: a **valued** `break` exiting a **procedure**
/// consuming call has no value destination (a `to` yields Void), so the machine
/// raises **provisionally** rather than silently discard the value. Tracked
/// pending the user's ruling on the S-10 to-consumer half.
#[test]
fn a_valued_break_into_a_procedure_consumer_raises_provisionally() {
    assert_raises(
        "to each1(do body)\nbody()\nend\nto wrap()\neach1() do break 5 end\nend\nwrap()\n",
        ExceptionKind::NoValueDestination,
    );
}

/// A block whose tail is value-less yields **Void**, not the previous statement's
/// transient value (the register is cleared at each statement boundary). Here the
/// block ends in a `while … break` (a loop yields Void, L§7.6), so `body()` yields
/// Void and `let v = body()` (which uses the value) raises. A bug that leaked the
/// loop body's last value (`42`) would let this bind `v = 42` and complete.
#[test]
fn a_block_ending_in_a_loop_yields_void() {
    assert_raises(
        "fn consume(do body)\nlet v = body()\nv\nend\n\
         to go()\nconsume() do\nwhile true do\n42\nbreak\nend\nend\nend\ngo()\n",
        ExceptionKind::ProcedureInExpression,
    );
}

/// An empty block yields Void; consuming that value raises (it does not leak a
/// prior statement's transient value). The value is consumed *inside* `m`
/// (`let v = body()`), so it is a consuming-site error there — a bug that leaked
/// the prior `99` would bind `v = 99` and complete.
#[test]
fn an_empty_block_yields_void() {
    assert_raises(
        "fn m(do body)\n99\nlet v = body()\nv\nend\nto go()\nm() do end\nend\ngo()\n",
        ExceptionKind::ProcedureInExpression,
    );
}

/// A `fn` that dynamically falls off the end without a value raises at its **own**
/// completion (L§8.4/§8.7), independent of whether the caller uses the value. This
/// is the `fn`-tail-`to` case: `f`'s tail call `g()` has a runtime-indeterminate
/// kind (`g` is a parameter), so the resolver defers the judgment; when `g` is a
/// `to`, the S-55 kind gate runs the call as an ordinary frame (no reuse), the `to`
/// completes Void, and `f` reaches its `ReturnBarrier` with a Void register — which
/// now raises. Here `f(noop)` is a bare statement, so nothing consumes the result;
/// the raise is the fn's own falls-off enforcement, not a consuming-site error.
#[test]
fn a_function_that_falls_off_the_end_raises() {
    assert_raises(
        "to noop() end\nfn f(g) g() end\nf(noop)\n",
        ExceptionKind::FunctionFellOffEnd,
    );
}

/// The S-55 mixed-kind cases run as ordinary (non-tail) frames: a `to` that
/// tail-calls an `fn` **discards** the value (the `to` still yields Void), so
/// reaching it through a `let` (where the resolver can't pin the kind) raises a
/// consuming-site error rather than yielding the `fn`'s value.
#[test]
fn a_procedure_tail_calling_a_function_discards_the_value() {
    assert_raises(
        "fn add(a, b) a + b end\nto run() add(1, 2) end\nlet g = run\nlet r = g()\n",
        ExceptionKind::ProcedureInExpression,
    );
}

/// A **bare** `return` in a `fn` (non-tail, so the resolver can't reject it — the
/// fn's *tail* still produces a value) makes the function value-less on that path.
/// Reaching it raises `FunctionFellOffEnd` at the fn's completion — the same rule
/// the `ReturnBarrier` applies on fall-through, but the `return` unwind path must
/// enforce it too (it delivers the result without touching the barrier). Both the
/// unconsumed and consumed cases raise the same error at the same site.
#[test]
fn a_bare_return_in_a_function_falls_off() {
    assert_raises(
        "fn f(c)\nif c then return end\n42\nend\nf(true)\n",
        ExceptionKind::FunctionFellOffEnd,
    );
    assert_raises(
        "fn f(c)\nif c then return end\n42\nend\nlet y = f(true)\ny\n",
        ExceptionKind::FunctionFellOffEnd,
    );
}
