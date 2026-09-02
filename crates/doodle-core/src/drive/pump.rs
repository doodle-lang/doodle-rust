//! The core drive loop (engine spec E§7): [`drive`] steps an instance to a stopping
//! [`Outcome`](crate::drive::Outcome) under a directive, honoring capability suspends,
//! breakpoints, the raise-trap, host pauses, and the `Step*` pause decision
//! ([`should_pause`], E§8.5). Split from the host entry points and outcome types in the
//! parent `drive` module to keep that file within the hygiene length limit.

use super::{Directive, EngineFault, Outcome, PauseReason, SafePoint, SafePointKind};
use crate::machine::{Halt, Instance, InstanceState};

/// The core drive loop: steps `instance` to a stopping [`Outcome`] under `directive`,
/// pausing a `Step*` at the next matching safe point and suspending at a capability
/// call. Shared by [`run`](super::run) and the resume side of [`resolve`](super::resolve).
pub(super) fn drive(instance: &mut Instance, directive: Directive, fuel: Option<u64>) -> Outcome {
    // Anchor `StepOver`/`StepOut` depth judgments (E§8.5): fresh at the current depth, but
    // **preserved** across a step's internal suspend/slice re-entries. A `StepOver` of a call
    // that suspends a capability (`forward` calling `draw_line`) must treat the whole call as one
    // step, not re-anchor at the deep resume depth. Remember the directive so a `resolve` after a
    // suspend resumes under it (E§7.3), and arm this call's bounded-run fuel (S-40).
    let anchor_depth = instance.step_anchor(directive);
    instance.set_directive(directive);
    instance.arm_slice(fuel);
    instance.set_state(InstanceState::Running);
    loop {
        if instance.is_halted() {
            instance.set_state(InstanceState::Completed);
            // E§7.2: a top-level module drive completes with **no value** (Void) — a
            // module runs for effect (L§6.11). `Completed`'s value is present only for
            // a returning `fn` (a reentrant callable return, E§7.6, M2b.5).
            return Outcome::Completed(None);
        }
        // The drive call's slice fuel is spent (S-40): yield here, resumable — including
        // the `fuel == Some(0)` case (yield before running anything) and mid-drive
        // exhaustion (the previous step's safe point flagged it). Checked **after**
        // `is_halted`, so a program that finishes on the same safe point that spends the
        // last fuel still wins `Completed` at the exact boundary (completion is a fact
        // about past work; a slice end is a bound on future work — there is none left).
        if instance.sliced_out() {
            // A slice end is an internal yield, not a host-visible stop: the next drive continues
            // the same step, so keep the step anchor (E§8.5).
            instance.mark_step_resuming();
            instance.set_state(InstanceState::Paused);
            return Outcome::Paused(PauseReason::SliceEnd);
        }
        // Raise-trap (E§8.7, S-18): a freshly-armed raise pauses here — **before** the next
        // `step` runs any unwind — so the debugger sees the raising frame with the stack
        // intact. Checked before `step` because a raise armed by the previous step (or by a
        // host `resolve(Raise)` before this drive) unwinds inside `step`; `take_raise_trap`
        // marks it one-shot so the resumed drive steps into the unwind instead of re-trapping.
        // Ignored under `RunToCompletion` (a host directive, like breakpoints — the M6 matrix).
        if directive != Directive::RunToCompletion && instance.take_raise_trap() {
            instance.set_state(InstanceState::Paused);
            return Outcome::Paused(PauseReason::RaiseTrap);
        }
        match instance.step() {
            Ok(safe_point) => {
                // A capability call parked a request (E§7.5, MD §14): suspend, no state
                // torn down. Checked before the pause decision (they cannot coincide —
                // a capability call is not a statement-level safe point). A slice
                // exhausted on this same step is honored on the next loop turn (a suspend
                // wins — the instance is `Suspended`, resumed via `resolve`).
                if instance.is_suspended() {
                    // A capability suspend is internal to a step — the host resolves it and the
                    // step continues — so keep the step anchor across the resolve (E§8.5): a
                    // `StepOver` of a call that suspends must not degrade to `StepInto`.
                    instance.mark_step_resuming();
                    instance.set_state(InstanceState::Suspended);
                    // An `import` of an unloaded module (E§6) suspends the same way but is
                    // resolved through `resolve_import`, so it surfaces as its own outcome
                    // that routes the host there.
                    return if instance.is_import_suspended() {
                        Outcome::SuspendedImport(instance.import_request())
                    } else {
                        Outcome::Suspended(instance.capability_request())
                    };
                }
                // A host-requested pause (E§8.8): stops at the next safe point **regardless
                // of directive** — a host control like cancel, not a `Step*` decision — so it
                // is checked before `should_pause` and wins over it. Only at an actual safe
                // point (`safe_point.is_some()`), never mid-expression; consumed here
                // (`take_host_pause` reads-and-clears) so the request is one-shot and the
                // re-drive continues. State stays intact and resumable — a pause is not a
                // fault, so no unwind is armed.
                if safe_point.is_some() && instance.take_host_pause() {
                    instance.set_state(InstanceState::Paused);
                    return Outcome::Paused(PauseReason::HostPause);
                }
                // A breakpoint at this safe point (E§8.6): stop under `Continue` or a `Step*`
                // directive, never under `RunToCompletion` (which ignores breakpoints).
                // `breakpoint_hit` matches the statement about to run, so a loop-body
                // breakpoint re-fires each iteration. Checked before the `Step*` decision so a
                // breakpoint that coincides with a step reports `Breakpoint`, the specific
                // reason.
                if safe_point.is_some()
                    && directive != Directive::RunToCompletion
                    && let Some(id) = instance.breakpoint_hit()
                {
                    instance.set_state(InstanceState::Paused);
                    return Outcome::Paused(PauseReason::Breakpoint(id));
                }
                if let Some(sp) = safe_point
                    && should_pause(directive, anchor_depth, sp)
                {
                    instance.set_state(InstanceState::Paused);
                    return Outcome::Paused(PauseReason::Step);
                }
            }
            // An uncaught raise reached the outermost boundary (no handlers yet;
            // `try`/`rescue` is M4). The instance enters the terminal `Raised` state
            // (E§3.3/§9), distinct from `Faulted`; the outcome carries exception + trace.
            Err(Halt::Raise(value, trace)) => {
                instance.set_state(InstanceState::Raised);
                return Outcome::Raised(value, trace);
            }
            // A resource limit (E§10.2), host cancellation (E§10.1), or the S-15
            // `NestedSuspend` relayed from a native consumer's reentrant drive (§7.6): a
            // non-resumable fault (`state()` becomes terminal `Faulted`).
            Err(Halt::Fault(fault)) => {
                instance.set_state(InstanceState::Faulted);
                return Outcome::Faulted(fault);
            }
        }
    }
}

/// Terminally faults `instance` on a host-contract violation (e.g. a stale resolution
/// handle): sets the state to `Faulted` so a returned `Faulted` outcome always implies
/// `state() == Faulted` (E§3.3 outcome↔state correspondence).
pub(super) fn fault(instance: &mut Instance) -> Outcome {
    instance.set_state(InstanceState::Faulted);
    Outcome::Faulted(EngineFault::Internal)
}

/// Whether `sp` stops the given `directive`, anchored at the depth the drive's step began
/// from (E§8.5).
fn should_pause(directive: Directive, anchor_depth: usize, sp: SafePoint) -> bool {
    match directive {
        // A fast run and a "resume the program" both run to the next capability /
        // fault / completion; the breakpoints and raise-trap that make `Continue`
        // distinct are M6.
        Directive::RunToCompletion | Directive::Continue => false,
        // Stop at the very next safe point, in any frame — including a call's entry
        // and a callee's return.
        Directive::Step | Directive::StepInto => true,
        // Treat a call as one step. A forward boundary at the anchor depth or shallower is
        // the next stop (a call's deeper entry is skipped); but a **return** at exactly the
        // anchor depth is a callee returning *into* the stepped-over frame mid-statement — not
        // a stop — so a return stops only strictly shallower (the anchor frame itself
        // returning), like `StepOut`. Without this, stepping over a statement that calls a
        // function stops twice on the same line (the call's return, then the next statement).
        Directive::StepOver => match sp.kind {
            SafePointKind::Boundary => sp.depth <= anchor_depth,
            SafePointKind::Return => sp.depth < anchor_depth,
        },
        // Run until the current frame returns: stop only strictly shallower.
        Directive::StepOut => sp.depth < anchor_depth,
    }
}
