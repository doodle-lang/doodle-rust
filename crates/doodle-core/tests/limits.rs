//! Resource-limit smoke tests (engine spec E§10.2): drive real Doodle source under
//! small [`Limits`] and check that a runaway program stops with the right
//! `Faulted(LimitExceeded(kind))` — the step budget, the heap, and the non-tail
//! stack-depth ceilings — while a proper tail loop stays exempt from the stack
//! limit (L§8.7). These are the M2a.9 exit criteria (deep non-tail recursion →
//! stack fault; unbounded allocation → heap fault at a deterministic step).

use doodle_core::diag::Severity;
use doodle_core::drive::{Directive, EngineFault, LimitKind, Limits, Outcome, run};
use doodle_core::machine::Instance;
use doodle_core::parse::parse_program;
use doodle_core::resolve::resolve;
use doodle_core::source::normalize;
use doodle_core::span::ModuleId;

/// Loads Doodle `src` through the real pipeline (normalize → parse → resolve) under
/// `limits`, asserting it loads clean.
fn instance_with_limits(src: &str, limits: Limits) -> Instance {
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
    Instance::load_with_limits(resolved.module, limits)
}

/// Asserts driving `src` under `limits` faults with `LimitExceeded(kind)`.
fn assert_limit(src: &str, limits: Limits, kind: LimitKind) {
    let mut inst = instance_with_limits(src, limits);
    match run(&mut inst, Directive::RunToCompletion) {
        Outcome::Faulted(EngineFault::LimitExceeded(got)) => assert_eq!(got, kind),
        other => panic!("expected LimitExceeded({kind:?}), got {other:?}"),
    }
}

/// An infinite empty loop exhausts the **step budget**: each iteration passes a
/// statement-level safe point, so the fused counter runs out and the drive faults
/// deterministically rather than spinning forever.
#[test]
fn an_infinite_loop_exhausts_the_step_budget() {
    assert_limit(
        "loop do\nend\n",
        Limits {
            step_budget: 1_000,
            ..Limits::default()
        },
        LimitKind::StepBudget,
    );
}

/// Unbounded **non-tail** recursion trips the stack-depth limit: `1 + f(n)` keeps
/// `f` on the stack (the call is an operand, not a tail call), so each activation
/// pushes a frame until the ceiling is crossed at call entry. (M2a.9 exit criterion:
/// deep non-tail recursion → stack fault.)
#[test]
fn deep_non_tail_recursion_hits_the_stack_depth_limit() {
    assert_limit(
        "fn f(n)\n1 + f(n)\nend\nf(0)\n",
        Limits {
            stack_depth: 64,
            ..Limits::default()
        },
        LimitKind::StackDepth,
    );
}

/// A loop that allocates each iteration and never reuses the memory (no GC yet)
/// grows `bytes_allocated` monotonically until it crosses the **heap** limit — a
/// fault at a deterministic step. (M2a.9 exit criterion: unbounded allocation →
/// heap fault.)
#[test]
fn unbounded_allocation_hits_the_heap_limit() {
    assert_limit(
        "loop do\nb\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\nend\n",
        Limits {
            heap_bytes: 8_192,
            ..Limits::default()
        },
        LimitKind::Heap,
    );
}

/// A loop allocating **empty** objects (`b""`, zero payload) still trips the heap
/// limit: each allocation carries a fixed per-object overhead, so object *count*
/// counts. Without that overhead `bytes_allocated` would stay flat while real
/// memory grew — the M2a.1 object-count hole — and the fault would never fire.
#[test]
fn a_flood_of_empty_objects_still_hits_the_heap_limit() {
    assert_limit(
        "loop do\nb\"\"\nend\n",
        Limits {
            heap_bytes: 4_096,
            ..Limits::default()
        },
        LimitKind::Heap,
    );
}

/// A **tail** loop never trips the stack-depth limit even set tiny: a proper tail
/// call reuses the current frame (L§8.7), so depth stays constant and the program
/// completes. This is the counterpart to the non-tail case above — the exemption
/// that makes bounded loops expressible as recursion.
#[test]
fn a_tail_loop_is_exempt_from_the_stack_depth_limit() {
    let mut inst = instance_with_limits(
        "to count_down(n)\nif n > 0 then\ncount_down(n - 1)\nend\nend\ncount_down(1000)\n",
        Limits {
            stack_depth: 16,
            ..Limits::default()
        },
    );
    assert!(matches!(
        run(&mut inst, Directive::RunToCompletion),
        Outcome::Completed(None)
    ));
}
