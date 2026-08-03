//! Drive-state-machine tests (M2b.3): the resumable [`run`]/[`resolve`] loop, the
//! `Step*`/`Continue` directives (basic granularity), the E§3.3 outcome↔state
//! correspondence, and the host-contract phase guards.
//!
//! These drive through the public API only — `run`, `state`, `result`, `output` —
//! so they double as the "drive-directive determinism" check (E§7.7): the same
//! program reaches the same terminal under a step-through as under a fast run.

use doodle_core::drive::{
    CapabilityId, Directive, EngineFault, LimitKind, Limits, Outcome, Resolution, resolve, run,
};
use doodle_core::machine::{
    Instance, InstanceState, Registry, each_intrinsic, print_intrinsic, read_line_intrinsic,
};
use doodle_core::parse::parse_program;
use doodle_core::resolve::resolve as resolve_module;
use doodle_core::source::normalize;
use doodle_core::span::ModuleId;

/// Loads Doodle `src` into an instance through the real pipeline, asserting it loads
/// clean.
fn instance(src: &str) -> Instance {
    load(src, Limits::default())
}

fn load(src: &str, limits: Limits) -> Instance {
    use doodle_core::diag::Severity;
    let nfc = normalize(src);
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
    Instance::load_with_limits(resolved.module, limits)
}

/// Loads `src` with the `print` and `read_line` intrinsics registered — `print`
/// (index 0) is a synchronous `to`, `read_line` (index 1) a suspending `fn` capability.
fn instance_with_caps(src: &str) -> Instance {
    use doodle_core::diag::Severity;
    let nfc = normalize(src);
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
    Instance::load_with_intrinsics(resolved.module, caps_registry())
}

/// print (0), read_line (1), each (2) — the demo intrinsics the drive tests use.
fn caps_registry() -> Registry {
    let mut registry = Registry::new();
    registry.register(print_intrinsic()).unwrap();
    registry.register(read_line_intrinsic()).unwrap();
    registry.register(each_intrinsic()).unwrap();
    registry
}

/// Resolves `src` clean and returns the resolved module (for a custom `Instance` build).
fn resolve_clean(src: &str) -> doodle_core::resolve::ResolvedModule {
    use doodle_core::diag::Severity;
    let nfc = normalize(src);
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
    resolved.module
}

/// Drives `inst` to a terminal outcome under a fixed `directive`, re-driving past each
/// `Paused`; returns how many times it paused and the terminal outcome.
fn drive_counting(inst: &mut Instance, directive: Directive) -> (usize, Outcome) {
    let mut pauses = 0;
    let mut outcome = run(inst, directive);
    while matches!(outcome, Outcome::Paused(_)) {
        pauses += 1;
        assert!(pauses < 100_000, "step loop did not terminate");
        outcome = run(inst, directive);
    }
    (pauses, outcome)
}

// ---- outcome ↔ state correspondence (E§3.3) ----

#[test]
fn a_clean_program_completes_and_leaves_completed() {
    let mut inst = instance("let a = 1\nlet b = 2\n");
    assert!(matches!(
        run(&mut inst, Directive::RunToCompletion),
        Outcome::Completed(None)
    ));
    assert_eq!(inst.state(), InstanceState::Completed);
}

#[test]
fn an_uncaught_raise_leaves_the_instance_raised() {
    // A distinct terminal `Raised` state (E§3.3/§9), NOT `Faulted` — `state()` alone
    // tells a Doodle exception from an engine fault.
    let mut inst = instance("1 / 0\n");
    assert!(matches!(
        run(&mut inst, Directive::RunToCompletion),
        Outcome::Raised(..)
    ));
    assert_eq!(inst.state(), InstanceState::Raised);
}

#[test]
fn a_limit_fault_leaves_the_instance_faulted() {
    // A tiny step budget trips at a safe point → Faulted(LimitExceeded(StepBudget)).
    let limits = Limits {
        step_budget: 50,
        ..Limits::default()
    };
    let mut inst = load("loop do\n1\nend\n", limits);
    let outcome = run(&mut inst, Directive::RunToCompletion);
    assert!(
        matches!(
            outcome,
            Outcome::Faulted(EngineFault::LimitExceeded(LimitKind::StepBudget))
        ),
        "{outcome:?}"
    );
    assert_eq!(inst.state(), InstanceState::Faulted);
}

// ---- host-contract phase guards ----

#[test]
#[should_panic(expected = "not re-drivable")]
fn re_driving_a_terminal_instance_is_a_contract_violation() {
    let mut inst = instance("1\n");
    run(&mut inst, Directive::RunToCompletion); // → Completed
    let _ = run(&mut inst, Directive::RunToCompletion); // debug-asserts
}

#[test]
#[should_panic(expected = "Suspended")]
fn resolving_a_non_suspended_instance_is_a_contract_violation() {
    let mut inst = instance("1\n");
    // Nothing has suspended (capabilities are M2b.4), so resolve is misuse.
    let handle = inst.make_int(0);
    let _ = resolve(&mut inst, doodle_core::drive::Resolution::Value(handle));
}

// ---- Step directives ----

#[test]
fn step_pauses_statement_by_statement_then_completes() {
    let mut inst = instance("let a = 1\nlet b = 2\nlet c = 3\n");
    let (pauses, outcome) = drive_counting(&mut inst, Directive::Step);
    // Every `Step` stops at a statement-level safe point, so a three-statement module
    // pauses a few times before completing (the exact count is an implementation
    // detail of safe-point placement; what matters is it steps and then completes).
    assert!(pauses >= 3, "expected per-statement pauses, got {pauses}");
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.state(), InstanceState::Completed);
}

#[test]
fn stepping_through_reaches_the_same_terminal_as_a_fast_run() {
    // Drive-directive determinism (E§7.7): the terminal outcome + state is the same
    // whether stepped or run straight through.
    let src = "let a = 1\nlet b = 2\nlet c = 3\n";
    let mut fast = instance(src);
    let fast_outcome = run(&mut fast, Directive::RunToCompletion);

    let mut stepped = instance(src);
    let (_, step_outcome) = drive_counting(&mut stepped, Directive::Step);

    assert!(matches!(fast_outcome, Outcome::Completed(None)));
    assert!(matches!(step_outcome, Outcome::Completed(None)));
    assert_eq!(fast.state(), stepped.state());
}

// ---- suspending capabilities (M2b.4) ----

#[test]
fn a_capability_suspends_with_its_identity_and_resolves_with_a_value() {
    // `read_line()` suspends; the request names read_line's capability id (registry
    // index 1 — print is 0) and carries no args. Resolving with a value makes it the
    // call's result, which `print` then emits.
    let mut inst = instance_with_caps("print(read_line())\n");
    let outcome = run(&mut inst, Directive::RunToCompletion);
    let Outcome::Suspended(request) = outcome else {
        panic!("expected Suspended, got {outcome:?}");
    };
    assert_eq!(request.capability, CapabilityId(1));
    assert!(request.args.is_empty());
    assert_eq!(inst.state(), InstanceState::Suspended);

    let line = inst.make_string(b"hello").unwrap();
    let resumed = resolve(&mut inst, Resolution::Value(line));
    assert!(matches!(resumed, Outcome::Completed(None)), "{resumed:?}");
    assert_eq!(inst.output(), b"hello\n");
    assert_eq!(inst.state(), InstanceState::Completed);
}

#[test]
fn a_capability_resolved_with_a_raise_surfaces_raised() {
    let mut inst = instance_with_caps("print(read_line())\n");
    assert!(matches!(
        run(&mut inst, Directive::RunToCompletion),
        Outcome::Suspended(_)
    ));
    let reason = inst.make_string(b"end of input").unwrap();
    let outcome = resolve(&mut inst, Resolution::Raise(reason));
    assert!(matches!(outcome, Outcome::Raised(..)), "{outcome:?}");
    assert_eq!(inst.state(), InstanceState::Raised);
    // The host rejected the call, so `print` never ran.
    assert_eq!(inst.output(), b"");
}

#[test]
fn resolving_with_a_stale_handle_faults_terminally() {
    // A stale resolution handle is a host-contract violation: the drive returns
    // Faulted AND the instance is left terminally Faulted (not a resumable Suspended
    // half-state) — a Faulted outcome always implies state() == Faulted (E§3.3).
    let mut inst = instance_with_caps("print(read_line())\n");
    assert!(matches!(
        run(&mut inst, Directive::RunToCompletion),
        Outcome::Suspended(_)
    ));
    let stale = inst.make_int(0);
    inst.release(stale).unwrap(); // now names a freed slot
    let outcome = resolve(&mut inst, Resolution::Value(stale));
    assert!(matches!(outcome, Outcome::Faulted(_)), "{outcome:?}");
    assert_eq!(inst.state(), InstanceState::Faulted);
}

#[test]
fn a_scripted_capability_replays_to_the_same_terminal() {
    // Replay determinism (E§11): the same program + the same resolution sequence
    // reconstructs the same output/terminal on a fresh instance.
    fn drive_with_line(line: &[u8]) -> (Vec<u8>, bool) {
        let mut inst = instance_with_caps("print(read_line())\nprint(read_line())\n");
        // Start with `run`; every subsequent Suspended is continued with `resolve`.
        let mut outcome = run(&mut inst, Directive::RunToCompletion);
        loop {
            match outcome {
                Outcome::Suspended(_) => {
                    let h = inst.make_string(line).unwrap();
                    outcome = resolve(&mut inst, Resolution::Value(h));
                }
                Outcome::Completed(_) => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        (
            inst.output().to_vec(),
            inst.state() == InstanceState::Completed,
        )
    }
    assert_eq!(drive_with_line(b"x"), drive_with_line(b"x"));
    assert_eq!(drive_with_line(b"x").0, b"x\nx\n");
}

// ---- reentrant native block-consumer: `each` (M2b.5a) ----

#[test]
fn each_invokes_the_block_once_per_element() {
    let mut inst = instance_with_caps("each([1, 2, 3]) do (x)\nprint(x)\nend\n");
    let outcome = run(&mut inst, Directive::RunToCompletion);
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"1\n2\n3\n");
}

#[test]
fn each_over_an_empty_list_runs_the_block_zero_times() {
    let mut inst = instance_with_caps("each([]) do (x)\nprint(x)\nend\n");
    assert!(matches!(
        run(&mut inst, Directive::RunToCompletion),
        Outcome::Completed(None)
    ));
    assert_eq!(inst.output(), b"");
}

#[test]
fn continue_in_an_each_block_ends_that_iteration() {
    // `continue` ends the block invocation early, so `print` is skipped for x == 2;
    // `each` proceeds to the next element (a normal Completed, not a NonLocalExit).
    let mut inst =
        instance_with_caps("each([1, 2, 3]) do (x)\nif x == 2 then continue end\nprint(x)\nend\n");
    assert!(matches!(
        run(&mut inst, Directive::RunToCompletion),
        Outcome::Completed(None)
    ));
    assert_eq!(inst.output(), b"1\n3\n");
}

#[test]
fn a_raise_inside_an_each_block_propagates() {
    let mut inst = instance_with_caps("each([1, 2]) do (x)\n1 / 0\nend\n");
    assert!(matches!(
        run(&mut inst, Directive::RunToCompletion),
        Outcome::Raised(..)
    ));
}

#[test]
fn each_needs_a_list_to_iterate() {
    let mut inst = instance_with_caps("each(5) do (x)\nprint(x)\nend\n");
    assert!(matches!(
        run(&mut inst, Directive::RunToCompletion),
        Outcome::Raised(..)
    ));
}

#[test]
fn a_limit_tripped_inside_an_each_block_faults() {
    // The reentry-fault channel (MD §14): a step budget exhausted inside the nested
    // drive surfaces as `Faulted` after the native `each` returns — a limit is
    // instance-global and cannot flow through the Raise-typed callback chain.
    let limits = Limits {
        step_budget: 300,
        ..Limits::default()
    };
    let module = resolve_clean("each([1]) do (x)\nloop do\n1\nend\nend\n");
    let mut inst = Instance::load_with_intrinsics_and_limits(module, limits, caps_registry());
    let outcome = run(&mut inst, Directive::RunToCompletion);
    assert!(
        matches!(
            outcome,
            Outcome::Faulted(EngineFault::LimitExceeded(LimitKind::StepBudget))
        ),
        "{outcome:?}"
    );
    assert_eq!(inst.state(), InstanceState::Faulted);
}

#[test]
fn reentrant_recursion_through_each_faults_instead_of_overflowing_the_stack() {
    // A program that recurses through the native `each`: each level nests a reentrant
    // drive on the host's Rust stack. Bounded (MD §14) so it faults with StackDepth
    // rather than aborting the host process by overflowing the native stack.
    let mut inst = instance_with_caps("to r(x)\neach([x]) do (y)\nr(y)\nend\nend\nr(1)\n");
    let outcome = run(&mut inst, Directive::RunToCompletion);
    assert!(
        matches!(
            outcome,
            Outcome::Faulted(EngineFault::LimitExceeded(LimitKind::StackDepth))
        ),
        "{outcome:?}"
    );
    assert_eq!(inst.state(), InstanceState::Faulted);
}

// ---- S-46: non-local exits crossing the native boundary (M2b.5b) ----

#[test]
fn break_ends_the_each_and_completes_the_call() {
    // A `break` inside the block targets the `each` call (L§7.10), crossing the native
    // boundary as an S-46 NonLocalExit: the block runs for 1, then `break` at x == 2
    // stops the iteration and completes `each` normally (3 is never reached).
    let mut inst = instance_with_caps(
        "each([1, 2, 3]) do (x)\nprint(x)\nif x == 2 then break end\nend\nprint(99)\n",
    );
    let outcome = run(&mut inst, Directive::RunToCompletion);
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    // 1, 2 print; break ends `each`; the following statement (99) still runs.
    assert_eq!(inst.output(), b"1\n2\n99\n");
}

#[test]
fn return_across_an_each_block_returns_from_the_enclosing_fn() {
    // A `return` inside the block targets the enclosing `f`, punching through the native
    // `each` (S-46): `f` yields 1 (the first element) and the trailing `99` never runs.
    let mut inst =
        instance_with_caps("fn f()\neach([1, 2, 3]) do (x)\nreturn x\nend\n99\nend\nprint(f())\n");
    let outcome = run(&mut inst, Directive::RunToCompletion);
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"1\n");
}

#[test]
fn a_break_in_a_nested_each_targets_only_the_inner_each() {
    // Composition (S-46 rider 2): the inner `break` ends the *inner* `each` (after y == 3
    // each time), and the outer `each` continues to its next element — the break resolves
    // at the innermost native consumer, not the outer one.
    let mut inst = instance_with_caps(
        "each([1, 2]) do (x)\neach([3, 4]) do (y)\nprint(y)\nbreak\nend\nprint(x)\nend\n",
    );
    let outcome = run(&mut inst, Directive::RunToCompletion);
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    // x=1: inner prints 3 then breaks, then print(1); x=2: inner prints 3 then breaks, print(2).
    assert_eq!(inst.output(), b"3\n1\n3\n2\n");
}

#[test]
fn a_return_from_a_nested_each_unwinds_through_both_consumers() {
    // Composition innermost-out (S-46 rider 2): the `return` crosses *both* native `each`
    // boundaries to reach `f`, which yields 3 (the first inner element).
    let mut inst = instance_with_caps(
        "fn f()\neach([1, 2]) do (x)\neach([3, 4]) do (y)\nreturn y\nend\nend\n0\nend\n\
         print(f())\n",
    );
    let outcome = run(&mut inst, Directive::RunToCompletion);
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"3\n");
}

#[test]
fn a_valued_break_to_the_procedure_each_raises() {
    // Parity with a Doodle `to` block-consumer (S-10 open half): `each` is a procedure,
    // so a valued `break` has no value destination and raises `NoValueDestination`.
    let mut inst = instance_with_caps("each([1]) do (x)\nbreak 5\nend\n");
    let outcome = run(&mut inst, Directive::RunToCompletion);
    assert!(matches!(outcome, Outcome::Raised(..)), "{outcome:?}");
    assert_eq!(inst.state(), InstanceState::Raised);
}

#[test]
fn stepping_a_break_across_each_reaches_the_same_terminal_as_a_fast_run() {
    // Drive-directive determinism (E§7.7) over the S-46 path: a `break` crossing the
    // native boundary reaches the same terminal + output whether stepped or run straight.
    let src = "each([1, 2, 3]) do (x)\nprint(x)\nif x == 2 then break end\nend\n";
    let mut fast = instance_with_caps(src);
    let fast_outcome = run(&mut fast, Directive::RunToCompletion);

    let mut stepped = instance_with_caps(src);
    let (_, step_outcome) = drive_counting(&mut stepped, Directive::Step);

    assert!(
        matches!(fast_outcome, Outcome::Completed(None)),
        "{fast_outcome:?}"
    );
    assert!(
        matches!(step_outcome, Outcome::Completed(None)),
        "{step_outcome:?}"
    );
    assert_eq!(fast.output(), stepped.output());
    assert_eq!(fast.state(), stepped.state());
}

#[test]
fn step_into_descends_into_a_call_but_step_over_does_not() {
    // `f` has a body of two statements. StepInto pauses at them (descends, depth 2);
    // StepOver treats the call as one step (skips the deeper safe points), so it
    // pauses strictly fewer times.
    let src = "to f()\nlet x = 1\nlet y = 2\nend\nf()\n";
    let mut into = instance(src);
    let (into_pauses, into_outcome) = drive_counting(&mut into, Directive::StepInto);
    let mut over = instance(src);
    let (over_pauses, over_outcome) = drive_counting(&mut over, Directive::StepOver);

    assert!(matches!(into_outcome, Outcome::Completed(None)));
    assert!(matches!(over_outcome, Outcome::Completed(None)));
    assert!(
        into_pauses > over_pauses,
        "StepInto ({into_pauses}) should pause more than StepOver ({over_pauses}) — it \
         descends into f's body"
    );
}
