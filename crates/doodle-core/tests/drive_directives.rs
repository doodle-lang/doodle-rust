//! Drive-state-machine tests (M2b.3): the resumable [`run`]/[`resolve`] loop, the
//! `Step*`/`Continue` directives (basic granularity), the E§3.3 outcome↔state
//! correspondence, and the host-contract phase guards.
//!
//! These drive through the public API only — `run`, `state`, `result`, `output` —
//! so they double as the "drive-directive determinism" check (E§7.7): the same
//! program reaches the same terminal under a step-through as under a fast run.

use doodle_core::drive::{
    CapabilityId, Directive, EngineFault, LimitKind, Limits, Outcome, PauseReason, Resolution,
    resolve, run, run_slice,
};
use doodle_core::machine::{
    Instance, InstanceState, Registry, each_intrinsic, print_intrinsic, random_intrinsic,
    read_line_intrinsic, time_intrinsic,
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
    Instance::load(resolved.module, limits, Registry::new(), "main")
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
    Instance::load(resolved.module, Limits::default(), caps_registry(), "main")
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
#[should_panic(expected = "suspended on a capability")]
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
fn a_custom_entry_path_names_the_entry_module() {
    // The engine has no magic entry id (D-M7-17): the host supplies it and it becomes the entry
    // module's canonical id — what `module_canonical_id` reports and what a breakpoint addresses
    // (E§3.2/§8.6). A breakpoint on that id binds and fires; one on a different id names no
    // loaded module and stays pending, so it never fires.
    let module = resolve_clean("let a = 1\nlet b = 2\nprint(b)\n");
    let mut inst = Instance::load(module, Limits::default(), caps_registry(), "prog.doodle");
    assert_eq!(inst.module_canonical_id(ModuleId(0)), Some("prog.doodle"));

    inst.set_breakpoint("prog.doodle", 3); // binds: the entry is named prog.doodle
    inst.set_breakpoint("main", 3); // a wrong id — no such loaded module, so it stays pending
    let outcome = run(&mut inst, Directive::Continue);
    let Outcome::Paused(PauseReason::Breakpoint(_)) = outcome else {
        panic!("expected a breakpoint pause on prog.doodle:3, got {outcome:?}");
    };
    assert_eq!(inst.output(), b"", "paused before print(b) ran");
}

#[test]
fn time_and_random_suspend_as_capabilities_and_resolve_to_values() {
    // The ambient reads `time`/`random` are suspending capabilities (E§5.3, D-M7-16/S-19): each
    // parks a request the host resolves across the recordable boundary, rather than reading a
    // clock/RNG inline. Registration order fixes the ids — print(0), time(1), random(2) — so a
    // recording replays each resolution against a stable identity (E§11).
    let mut registry = Registry::new();
    registry.register(print_intrinsic()).unwrap();
    registry.register(time_intrinsic()).unwrap();
    registry.register(random_intrinsic()).unwrap();
    let module = resolve_clean("print(time())\nprint(random())\n");
    let mut inst = Instance::load(module, Limits::default(), registry, "main");

    // `time()` suspends first, naming its capability id and carrying no args.
    let Outcome::Suspended(req) = run(&mut inst, Directive::RunToCompletion) else {
        panic!("time() should suspend");
    };
    assert_eq!(req.capability, CapabilityId(1));
    assert!(req.args.is_empty());
    // The host supplies the clock reading; the program prints it and runs on to `random()`.
    let clock = inst.make_int(7);
    let Outcome::Suspended(req2) = resolve(&mut inst, Resolution::Value(clock)) else {
        panic!("random() should suspend after time() resolves");
    };
    assert_eq!(req2.capability, CapabilityId(2));
    assert!(req2.args.is_empty());
    let draw = inst.make_int(3);
    let done = resolve(&mut inst, Resolution::Value(draw));
    assert!(matches!(done, Outcome::Completed(None)), "{done:?}");
    assert_eq!(inst.output(), b"7\n3\n");
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
fn a_cancelled_suspended_instance_resolved_with_a_raise_faults_cancelled() {
    // S-23: a cancellation requested while suspended wins over the host's resolution — a
    // *raise* included. The raise arm surfaces `Raised` without driving, so it cannot reap
    // a cancel at a safe point the way the value arm does; it must instead discard the
    // rejection and fault `Cancelled`. Regression: it once surfaced `Raised`, letting a
    // host raise that raced the stop button escape cancellation (M3.6 review).
    let mut inst = instance_with_caps("print(read_line())\n");
    assert!(matches!(
        run(&mut inst, Directive::RunToCompletion),
        Outcome::Suspended(_)
    ));
    // Stop button pressed while parked on the capability; then the host rejects the call.
    inst.cancel_token().cancel();
    let reason = inst.make_string(b"end of input").unwrap();
    let outcome = resolve(&mut inst, Resolution::Raise(reason));
    assert!(
        matches!(outcome, Outcome::Faulted(EngineFault::Cancelled)),
        "cancel must win over a host raise, got {outcome:?}"
    );
    assert_eq!(inst.state(), InstanceState::Faulted);
    // The rejection was discarded and the parked call's continuation never ran: no output.
    assert_eq!(inst.output(), b"");
}

#[test]
fn a_cancelled_suspended_instance_resolved_with_a_value_faults_cancelled_when_work_remains() {
    // The value arm's companion: a cancellation requested while suspended is reaped at the
    // resumed drive's next safe point. The first statement finishes (consuming the resolved
    // value), then the cancel faults before the second statement runs — program work
    // remained, so cancel wins (contrast: a value resolution that *completed* the program
    // would stand, a cancel racing completion losing, §10.1).
    let mut inst = instance_with_caps("print(read_line())\nprint(\"after\")\n");
    assert!(matches!(
        run(&mut inst, Directive::RunToCompletion),
        Outcome::Suspended(_)
    ));
    inst.cancel_token().cancel();
    let line = inst.make_string(b"hello").unwrap();
    let outcome = resolve(&mut inst, Resolution::Value(line));
    assert!(
        matches!(outcome, Outcome::Faulted(EngineFault::Cancelled)),
        "{outcome:?}"
    );
    assert_eq!(inst.state(), InstanceState::Faulted);
    // The resumed statement ran to its end (printing the resolved value); the cancel then
    // stopped the second statement.
    assert_eq!(inst.output(), b"hello\n");
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
    let mut inst = Instance::load(module, limits, caps_registry(), "main");
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

// ---- bounded-run fuel + Paused(SliceEnd) (M3.1, S-40) ----

#[test]
fn a_fuel_bounded_drive_pauses_at_sliceend_and_resumes_to_the_same_terminal() {
    // S-40: `run_slice(fuel)` runs at most `fuel` statement safe points, then yields
    // `Paused(SliceEnd)`; re-driving resumes to the **same terminal** as one unbounded
    // run (E§7.7) — slicing changes only where it yields, not what executes.
    let src = "let a = 1\nlet b = 2\nlet c = 3\nlet d = 4\n";
    let mut fast = instance(src);
    assert!(matches!(
        run(&mut fast, Directive::RunToCompletion),
        Outcome::Completed(None)
    ));

    let mut sliced = instance(src);
    let mut pauses = 0;
    loop {
        match run_slice(&mut sliced, Directive::RunToCompletion, Some(1)) {
            Outcome::Paused(PauseReason::SliceEnd) => {
                pauses += 1;
                assert!(pauses < 1000, "slice loop did not terminate");
            }
            Outcome::Completed(None) => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(
        pauses >= 1,
        "a tiny fuel produced slice-end pauses, got {pauses}"
    );
    assert_eq!(fast.state(), sliced.state());
    assert_eq!(sliced.state(), InstanceState::Completed);
}

#[test]
fn completion_wins_the_exact_fuel_boundary() {
    // When a drive's fuel exactly covers the program's remaining safe points — including
    // the frame-drain step that both spends the last fuel and finishes the program — the
    // outcome is `Completed`, not a spurious `Paused(SliceEnd)` on an already-empty stack
    // (completion is a fact about past work; a slice end bounds future work, of which
    // there is none). Count the safe points via fuel=1 slices, then drive with exactly
    // that many + 1 (the completing step).
    let src = "let a = 1\nlet b = 2\n";
    let mut counter = instance(src);
    let mut pauses = 0u64;
    loop {
        match run_slice(&mut counter, Directive::RunToCompletion, Some(1)) {
            Outcome::Paused(PauseReason::SliceEnd) => pauses += 1,
            Outcome::Completed(None) => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    let exact = pauses + 1;
    let mut inst = instance(src);
    let outcome = run_slice(&mut inst, Directive::RunToCompletion, Some(exact));
    assert!(
        matches!(outcome, Outcome::Completed(None)),
        "fuel={exact} (exact): {outcome:?}"
    );
    assert_eq!(inst.state(), InstanceState::Completed);
}

#[test]
fn zero_fuel_yields_sliceend_before_running_anything() {
    // `Some(0)` is a fully-spent slice: it yields `Paused(SliceEnd)` before executing a
    // single safe point (it must not silently run unbounded), and re-driving makes progress.
    let mut inst = instance("let a = 1\nlet b = 2\n");
    assert!(matches!(
        run_slice(&mut inst, Directive::RunToCompletion, Some(0)),
        Outcome::Paused(PauseReason::SliceEnd)
    ));
    assert_eq!(inst.state(), InstanceState::Paused);
    // A real slice from here still completes.
    let mut pauses = 0;
    loop {
        match run_slice(&mut inst, Directive::RunToCompletion, Some(1)) {
            Outcome::Paused(PauseReason::SliceEnd) => {
                pauses += 1;
                assert!(pauses < 1000);
            }
            Outcome::Completed(None) => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(inst.state(), InstanceState::Completed);
}

#[test]
fn an_unbounded_fuel_never_slice_ends() {
    // `None` fuel (the `run` default) runs to a real stop, never `SliceEnd`.
    let mut inst = instance("let a = 1\nlet b = 2\n");
    assert!(matches!(
        run_slice(&mut inst, Directive::RunToCompletion, None),
        Outcome::Completed(None)
    ));
}

#[test]
fn slice_end_is_a_resumable_pause_distinct_from_the_step_budget_fault() {
    // Exhausting the per-call slice fuel is a resumable `Paused(SliceEnd)` (state stays
    // Paused); exhausting the lifetime step budget is a terminal `Faulted(StepBudget)`.
    let mut sliced = instance("loop do\n1\nend\n");
    assert!(matches!(
        run_slice(&mut sliced, Directive::RunToCompletion, Some(5)),
        Outcome::Paused(PauseReason::SliceEnd)
    ));
    assert_eq!(sliced.state(), InstanceState::Paused);

    let limits = Limits {
        step_budget: 50,
        ..Limits::default()
    };
    let mut bounded = load("loop do\n1\nend\n", limits);
    assert!(matches!(
        run(&mut bounded, Directive::RunToCompletion),
        Outcome::Faulted(EngineFault::LimitExceeded(LimitKind::StepBudget))
    ));
    assert_eq!(bounded.state(), InstanceState::Faulted);
}

#[test]
fn the_step_budget_is_enforced_regardless_of_slice_size() {
    // S-20: the lifetime step budget counts safe points, not slices — an infinite loop
    // under a fixed budget faults `StepBudget` whether driven unbounded or in tiny
    // slices; slicing cannot let it run past the budget.
    fn drive_to_terminal(fuel: Option<u64>) -> Outcome {
        let limits = Limits {
            step_budget: 40,
            ..Limits::default()
        };
        let mut inst = load("loop do\n1\nend\n", limits);
        loop {
            match run_slice(&mut inst, Directive::RunToCompletion, fuel) {
                Outcome::Paused(PauseReason::SliceEnd) => continue,
                other => return other,
            }
        }
    }
    for fuel in [None, Some(1), Some(3), Some(7)] {
        let outcome = drive_to_terminal(fuel);
        assert!(
            matches!(
                outcome,
                Outcome::Faulted(EngineFault::LimitExceeded(LimitKind::StepBudget))
            ),
            "fuel {fuel:?}: {outcome:?}"
        );
    }
}

#[test]
fn slicing_does_not_change_program_output_even_across_a_nested_drive() {
    // Determinism (E§7.7): a program's output is identical whether run unbounded or in
    // tiny slices. This also covers a native block-consumer (`each`): its reentrant drive
    // cannot pause, so a slice exhausted mid-`each` overruns to the call's end — but the
    // program still executes exactly the same and produces the same output.
    let src = "each([1, 2, 3, 4, 5]) do (x)\nprint(x)\nend\n";
    let mut fast = instance_with_caps(src);
    assert!(matches!(
        run(&mut fast, Directive::RunToCompletion),
        Outcome::Completed(None)
    ));

    let mut sliced = instance_with_caps(src);
    loop {
        match run_slice(&mut sliced, Directive::RunToCompletion, Some(2)) {
            Outcome::Paused(PauseReason::SliceEnd) => continue,
            Outcome::Completed(None) => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(fast.output(), sliced.output());
    assert_eq!(fast.output(), b"1\n2\n3\n4\n5\n");
}

// ---- observation surface (M2b.7, E§8.1/§8.2) ----

#[test]
fn current_position_points_into_the_source_at_a_pause() {
    // E§8.1: a paused instance reports where it is as a module id + byte span into the
    // source; the host renders line/column. The span is in-bounds and non-empty.
    let src = "let a = 1\nlet b = 2\nlet c = 3\n";
    let mut inst = instance(src);
    assert!(matches!(
        run(&mut inst, Directive::Step),
        Outcome::Paused(_)
    ));
    let pos = inst
        .current_position()
        .expect("a paused instance has a position");
    assert_eq!(pos.module, ModuleId(0));
    assert!(
        pos.span.start < pos.span.end && (pos.span.end as usize) <= src.len(),
        "position span out of bounds: {:?}",
        pos.span
    );
}

#[test]
fn stack_walk_at_module_top_is_one_frameless_frame() {
    // At the module top level the stack is a single frame with no callable and no call
    // site (it was not entered by a Doodle call).
    let mut inst = instance("let a = 1\nlet b = 2\n");
    assert!(matches!(
        run(&mut inst, Directive::Step),
        Outcome::Paused(_)
    ));
    let frames = inst.stack_walk();
    assert_eq!(frames.len(), 1, "module top is a single frame");
    assert!(frames[0].callable.is_none(), "module top is not a callable");
    assert!(frames[0].call_site.is_none(), "module top has no call site");
    assert_eq!(frames[0].tail_count, 0);
}

#[test]
fn stack_walk_inside_a_call_shows_the_callee_and_its_call_site() {
    // Stepping into `f` puts its frame innermost, carrying a callable handle and the
    // call-site span of `f()`; the module top stays outermost, frameless.
    let mut inst = instance("to f()\nlet x = 1\nlet y = 2\nend\nf()\n");
    // Step in until we are inside `f` (a two-frame stack); release the minted handles
    // each probe so the walk itself does not leak them. The first probe sees the Ready
    // instance's lone module frame, then steps in.
    loop {
        let frames = inst.stack_walk();
        let depth = frames.len();
        for frame in &frames {
            if let Some(h) = frame.callable {
                inst.release(h).unwrap();
            }
        }
        if depth >= 2 {
            break;
        }
        let outcome = run(&mut inst, Directive::StepInto);
        assert!(
            matches!(outcome, Outcome::Paused(_)),
            "never entered f: {outcome:?}"
        );
    }
    let frames = inst.stack_walk();
    assert_eq!(frames.len(), 2, "inside f: f's frame + the module top");
    assert!(
        frames[0].callable.is_some(),
        "innermost frame f is a callable"
    );
    assert!(frames[0].call_site.is_some(), "f was entered by a call");
    assert!(frames[1].callable.is_none(), "module top is frameless");
    assert!(frames[1].call_site.is_none());
    for frame in &frames {
        if let Some(h) = frame.callable {
            inst.release(h).unwrap();
        }
    }
}

#[test]
fn current_position_is_never_the_zero_width_origin_across_a_step_through() {
    // Regression for the end-of-body fallback: a Step that lands as a frame drains to its
    // return (conts empty, or a bare ReturnBarrier) must not report the (0,0) file-start
    // glitch — it falls back to a position at the end of the module instead.
    let src = "to f()\nlet x = 1\nend\nf()\n";
    let mut inst = instance(src);
    loop {
        match run(&mut inst, Directive::StepInto) {
            Outcome::Paused(_) => {
                if let Some(pos) = inst.current_position() {
                    assert!(
                        !(pos.span.start == 0 && pos.span.end == 0),
                        "current_position returned the (0,0) origin glitch"
                    );
                }
            }
            Outcome::Completed(_) => break,
            other => panic!("unexpected {other:?}"),
        }
    }
}

// ---- cancellation (M2b.7, E§10.1) ----

#[test]
fn cancel_stops_an_infinite_loop_and_faults_cancelled() {
    // The stop button (E§10.1): an otherwise-infinite loop is torn down at the next safe
    // point once cancellation is requested, faulting `Cancelled` — a terminal, non-
    // resumable state. (Requested before the drive here; a real host may request it from
    // another thread while the drive runs, via the cloneable token.)
    let mut inst = instance("loop do\n1\nend\n");
    inst.cancel_token().cancel();
    let outcome = run(&mut inst, Directive::RunToCompletion);
    assert!(
        matches!(outcome, Outcome::Faulted(EngineFault::Cancelled)),
        "{outcome:?}"
    );
    assert_eq!(inst.state(), InstanceState::Faulted);
}

#[test]
fn an_unrequested_cancel_token_does_not_affect_a_normal_run() {
    // Holding a cancel token but never requesting cancellation leaves execution
    // untouched — the poll is a plain atomic load with no effect (no determinism leak).
    let mut inst = instance("let a = 1\nlet b = 2\n");
    let _token = inst.cancel_token();
    assert!(matches!(
        run(&mut inst, Directive::RunToCompletion),
        Outcome::Completed(None)
    ));
    assert_eq!(inst.state(), InstanceState::Completed);
}

#[test]
fn cancelling_a_suspended_instance_faults_when_resumed() {
    // E§10.1 covers a **suspended** instance too: request cancellation while parked at a
    // capability, then resume — the drive tears down at the first safe point after the
    // resume and faults `Cancelled`, so the trailing loop never runs.
    let mut inst = instance_with_caps("let x = read_line()\nloop do\n1\nend\n");
    assert!(matches!(
        run(&mut inst, Directive::RunToCompletion),
        Outcome::Suspended(_)
    ));
    inst.cancel_token().cancel();
    let line = inst.make_string(b"hi").unwrap();
    let outcome = resolve(&mut inst, Resolution::Value(line));
    assert!(
        matches!(outcome, Outcome::Faulted(EngineFault::Cancelled)),
        "{outcome:?}"
    );
    assert_eq!(inst.state(), InstanceState::Faulted);
}

// (Cancellation *inside* a native consumer's reentrant drive — the S-46 risk-peak
// teardown — needs a cancel that arrives mid-block, which a cancel-before-`run` cannot
// reach; it is covered crate-internally by
// `intrinsic::tests::cancelling_inside_a_native_consumers_reentrant_drive_tears_it_down`,
// which cancels from a foreign call the block invokes.)

// ---- foreign values in a running program (M2b.6, E§4.5) ----

#[test]
fn a_foreign_value_is_inert_in_doodle_and_finalizes_at_destroy() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
    // A host foreign value injected as a capability result: Doodle receives and holds it,
    // but it is opaque — arithmetic on it raises (E§4.5). Its finalizer never runs while
    // the program executes (a finalizer's effects cannot cross into Doodle, so they can't
    // perturb the deterministic run), only once when the instance is destroyed. `Arc`/atomic
    // shared state (not `Rc`), so the finalizer is `Send` — the bound the type now carries.
    let count = Arc::new(AtomicU32::new(0));
    let c = count.clone();
    let mut inst = instance_with_caps("read_line() + 1\n");
    assert!(matches!(
        run(&mut inst, Directive::RunToCompletion),
        Outcome::Suspended(_)
    ));
    let finalizer: doodle_core::heap::Finalizer = Box::new(move |ptr| {
        assert_eq!(ptr, 42, "the finalizer receives the host ptr");
        c.fetch_add(1, Relaxed);
    });
    let foreign = inst.make_foreign(7, 42, Some(finalizer));
    let outcome = resolve(&mut inst, Resolution::Value(foreign));
    assert!(
        matches!(outcome, Outcome::Raised(..)),
        "arithmetic on a foreign value raises: {outcome:?}"
    );
    assert_eq!(inst.state(), InstanceState::Raised);
    assert_eq!(
        count.load(Relaxed),
        0,
        "the finalizer must not run during the drive"
    );
    inst.destroy();
    assert_eq!(
        count.load(Relaxed),
        1,
        "the foreign's finalizer ran once at destroy"
    );
}

#[test]
fn a_suspend_inside_a_native_block_consumer_faults_nestedsuspend() {
    // S-15 (Decision #2, forbid-and-fault): a suspending capability reached inside a
    // NATIVE block-consumer's reentrant drive is a terminal, deterministic `NestedSuspend`
    // fault — the nested drive runs on the Rust stack, so the native consumer's progress
    // cannot be frozen and resumed. The block's `read_line()` suspends mid-`each`. The
    // exact-variant match pins it as `NestedSuspend` (not a `Suspended` the host could
    // resolve, nor a generic `Internal` fault).
    let mut inst = instance_with_caps("each([1]) do (x)\nread_line()\nend\n");
    let outcome = run(&mut inst, Directive::RunToCompletion);
    assert!(
        matches!(outcome, Outcome::Faulted(EngineFault::NestedSuspend)),
        "{outcome:?}"
    );
    assert_eq!(inst.state(), InstanceState::Faulted);
}

#[test]
fn nestedsuspend_reaches_the_same_terminal_stepped_and_sliced() {
    // Determinism (E§7.7): the `NestedSuspend` terminal is directive- and slice-independent
    // — a Step-through and a tiny-fuel sliced run both reach the same fault as a fast run
    // (the nested-drive loop ignores the directive/fuel, so the fault cannot be dropped or
    // deferred by observation granularity). Guards against a future refactor routing the
    // fault through the pause path.
    let src = "each([1]) do (x)\nread_line()\nend\n";
    let mut fast = instance_with_caps(src);
    assert!(matches!(
        run(&mut fast, Directive::RunToCompletion),
        Outcome::Faulted(EngineFault::NestedSuspend)
    ));

    let mut stepped = instance_with_caps(src);
    let mut outcome = run(&mut stepped, Directive::Step);
    for _ in 0..10_000 {
        if !matches!(outcome, Outcome::Paused(_)) {
            break;
        }
        outcome = run(&mut stepped, Directive::Step);
    }
    assert!(
        matches!(outcome, Outcome::Faulted(EngineFault::NestedSuspend)),
        "stepped: {outcome:?}"
    );

    let mut sliced = instance_with_caps(src);
    let mut outcome = run_slice(&mut sliced, Directive::RunToCompletion, Some(1));
    for _ in 0..10_000 {
        if !matches!(outcome, Outcome::Paused(PauseReason::SliceEnd)) {
            break;
        }
        outcome = run_slice(&mut sliced, Directive::RunToCompletion, Some(1));
    }
    assert!(
        matches!(outcome, Outcome::Faulted(EngineFault::NestedSuspend)),
        "sliced: {outcome:?}"
    );
}

#[test]
fn a_doubly_nested_native_consumer_still_faults_nestedsuspend() {
    // A suspend two native consumers deep faults the same way: the inner `each`'s drive
    // parks `NestedSuspend`, and the outer `each`'s reentrant loop re-propagates it
    // through its `Err(Halt::Fault)` arm rather than swallowing it or leaking a pending
    // request past the boundary.
    let mut inst =
        instance_with_caps("each([1]) do (x)\neach([1]) do (y)\nread_line()\nend\nend\n");
    let outcome = run(&mut inst, Directive::RunToCompletion);
    assert!(
        matches!(outcome, Outcome::Faulted(EngineFault::NestedSuspend)),
        "{outcome:?}"
    );
    assert_eq!(inst.state(), InstanceState::Faulted);
}

#[test]
fn a_doodle_block_consumer_suspends_normally_unlike_the_native_one() {
    // The forbid-and-fault rule is specific to NATIVE consumers. A Doodle `to` that
    // invokes its block parameter runs the block on the ordinary cont stack (not a
    // reentrant Rust-stack drive), so a suspending capability there suspends the whole
    // instance normally and resolves to completion — the case the turtle demo's Doodle
    // `repeat` relies on. (`run_block` invokes its block once.)
    let mut inst = instance_with_caps(
        "to run_block(do body)\nbody()\nend\nrun_block() do\nread_line()\nend\n",
    );
    let outcome = run(&mut inst, Directive::RunToCompletion);
    assert!(matches!(outcome, Outcome::Suspended(_)), "{outcome:?}");
    assert_eq!(inst.state(), InstanceState::Suspended);
    let line = inst.make_string(b"hi").unwrap();
    let done = resolve(&mut inst, Resolution::Value(line));
    assert!(matches!(done, Outcome::Completed(None)), "{done:?}");
}
