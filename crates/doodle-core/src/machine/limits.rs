//! Safe-point limit enforcement (engine spec E§10.2, machine-design §9): the fused
//! counter's one decrement-and-branch per statement-level safe point, plus the heap
//! and non-tail stack-depth checks that fire there.
//!
//! A **safe point** (E§7.4) is a place the engine may stop with fully inspectable,
//! resumable state: between statements, at call entry, and at return. These are the
//! points at which resource limits are evaluated (and, from M2a.10, where GC may
//! trigger). [`Machine::safe_point`] is called at each; [`Machine::check_stack_depth`]
//! additionally guards the one place the frame stack grows (call/block entry).

use super::control::Namespace;
use super::{Machine, gc};
use crate::drive::{EngineFault, LimitKind, Limits};
use crate::heap::Heap;

/// The floor for the GC trigger threshold, and its initial value (machine-design
/// §15): a program with little live data is not collected more often than every
/// [`GC_MIN_BYTES`] of fresh allocation, so GC stays cheap when there is nothing to
/// reclaim.
pub(crate) const GC_MIN_BYTES: u64 = 1 << 20;

/// After a collection, the next trigger is armed at the surviving set's size times
/// this factor (floored at [`GC_MIN_BYTES`]) — so a program with a large live set
/// is not swept on every safe point (machine-design §15).
const GC_GROWTH: u64 = 2;

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

/// A statement-level safe point (E§7.4): tick the fused counter, run a collection
/// if allocation crossed the GC threshold **or** the heap limit (machine-design
/// §15), then fault if still over the heap limit. Returns the fault to surface, if
/// any. Called at each safe point — between statements, at call entry, and at return.
///
/// The collect fires on **either** trigger, so the heap limit is only ever checked
/// *after a collect that failed to bring it under* (MD §15) — a program that drops
/// garbage under a tight limit reclaims and continues even when that limit sits
/// below the GC threshold's floor, rather than faulting on garbage (E§10.2, §7.7).
pub(crate) fn safe_point(
    heap: &mut Heap,
    machine: &mut Machine,
    namespace: &Namespace,
) -> Result<(), EngineFault> {
    // Fused counter (MD §9): one decrement-and-branch. Exhaustion is the step
    // budget — the engine owns no clock, so this is how a host bounds runtime.
    if machine.fuel.remaining == 0 {
        return Err(EngineFault::LimitExceeded(LimitKind::StepBudget));
    }
    machine.fuel.remaining -= 1;
    // Collect when accounted bytes cross the GC threshold (routine growth) or the
    // heap limit (the last-ditch collect MD §15 requires before the limit can
    // fault) — whichever is lower. Then re-arm the threshold at the survivors' next
    // doubling (floored at GC_MIN_BYTES), so a large *live* set is not swept every
    // safe point.
    if heap.bytes_allocated() > machine.gc_threshold.min(machine.limits.heap_bytes) {
        gc::collect(heap, machine, namespace);
        machine.gc_threshold = heap
            .bytes_allocated()
            .saturating_mul(GC_GROWTH)
            .max(GC_MIN_BYTES);
    }
    // Heap limit (E§10.2): only a heap still over the limit after the collect faults.
    if heap.bytes_allocated() > machine.limits.heap_bytes {
        return Err(EngineFault::LimitExceeded(LimitKind::Heap));
    }
    Ok(())
}

impl Machine {
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
