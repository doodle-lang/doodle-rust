//! Safe-point limit enforcement (engine spec E§10.2, machine-design §9): the fused
//! counter's one decrement-and-branch per statement-level safe point, plus the heap
//! and non-tail stack-depth checks that fire there.
//!
//! A **safe point** (E§7.4) is a place the engine may stop with fully inspectable,
//! resumable state: between statements, at call entry, and at return. These are the
//! points at which resource limits are evaluated (and, from M2a.10, where GC may
//! trigger). [`Machine::safe_point`] is called at each; [`Machine::check_stack_depth`]
//! additionally guards the one place the frame stack grows (call/block entry).

use super::Machine;
use crate::drive::{EngineFault, LimitKind, Limits};

/// The fused safe-point counter (MD §9): a single value decremented once per
/// statement-level safe point, so the hot path is one decrement-and-branch. At
/// M2a.9 its only contributor is the **step budget**; slice fuel and the
/// distance-to-next-armed-event fuse in when slicing and observation land, each
/// re-arming `remaining` to the running minimum. Reaching zero enters the slow
/// path — at M2a.9 the only slow-path outcome is `LimitExceeded(StepBudget)`.
pub(crate) struct FusedCounter {
    /// Safe points still permitted before the step budget is exhausted.
    remaining: u64,
}

impl FusedCounter {
    /// A counter armed to `limits.step_budget`.
    pub(crate) fn new(limits: &Limits) -> Self {
        FusedCounter {
            remaining: limits.step_budget,
        }
    }
}

impl Machine {
    /// A statement-level safe point (E§7.4): decrement the fused counter and check
    /// the heap limit. Returns the fault to surface, if any. Called at each safe
    /// point — between statements, at call entry, and at return.
    ///
    /// `bytes_allocated` is the heap's current payload total (MD §5, caches
    /// excluded). At M2a.10 a collection runs here first, and only a heap still over
    /// the limit after collecting faults; until GC exists, crossing the threshold
    /// faults directly.
    pub(crate) fn safe_point(&mut self, bytes_allocated: u64) -> Result<(), EngineFault> {
        // Fused counter (MD §9): one decrement-and-branch. Exhaustion is the step
        // budget — the engine owns no clock, so this is how a host bounds runtime.
        if self.fuel.remaining == 0 {
            return Err(EngineFault::LimitExceeded(LimitKind::StepBudget));
        }
        self.fuel.remaining -= 1;
        // Heap limit (E§10.2).
        if bytes_allocated > self.limits.heap_bytes {
            return Err(EngineFault::LimitExceeded(LimitKind::Heap));
        }
        Ok(())
    }

    /// The non-tail stack-depth limit (E§10.2), checked at call/block entry — the
    /// only place the frame stack grows. Proper tail calls reuse a frame (L§8.7), so
    /// a tail loop never reaches here with a growing `depth`; unbounded non-tail
    /// recursion does. `depth` is the post-push frame count.
    pub(crate) fn check_stack_depth(&self, depth: usize) -> Result<(), EngineFault> {
        if depth > self.limits.stack_depth as usize {
            return Err(EngineFault::LimitExceeded(LimitKind::StackDepth));
        }
        Ok(())
    }
}
