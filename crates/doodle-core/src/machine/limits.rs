//! Safe-point limit enforcement (engine spec E§10.2, machine-design §9): the fused
//! counter's one decrement-and-branch per statement-level safe point, plus the heap
//! and non-tail stack-depth checks that fire there.
//!
//! A **safe point** (E§7.4) is a place the engine may stop with fully inspectable,
//! resumable state: between statements, at call entry, and at return. These are the
//! points at which resource limits are evaluated (and, from M2a.10, where GC may
//! trigger). [`Machine::safe_point`] is called at each; [`Machine::check_stack_depth`]
//! additionally guards the one place the frame stack grows (call/block entry).

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

/// The fused safe-point counter (MD §9, AD6): the safe-point limits, checked with a
/// decrement-and-branch per statement-level safe point. Two contributors now — the
/// lifetime **step budget** (E§10.2) and the per-drive-call **slice fuel** (S-40); the
/// distance-to-next-armed-event (breakpoints) fuses in at M6. Their **minimum** bounds
/// a drive: whichever reaches zero first stops it — the step budget with a
/// `LimitExceeded(StepBudget)` fault, the slice fuel with a resumable `Paused(SliceEnd)`.
pub(crate) struct FusedCounter {
    /// Lifetime step-budget safe points still permitted (E§10.2); `0` → `StepBudget`.
    budget: u64,
    /// The current drive call's slice fuel — safe points it may run before yielding
    /// `Paused(SliceEnd)` (S-40). `None` is an **unbounded** (no-fuel) drive call, which
    /// never counts down or flags.
    slice: Option<u64>,
    /// Set when the current slice's fuel reaches `0` (S-40): the top-level drive loop
    /// pauses at the next safe point. A nested drive cannot pause, so it runs on and this
    /// is honored when control returns to the top level. Cleared by [`arm_slice`].
    ///
    /// [`arm_slice`]: FusedCounter::arm_slice
    exhausted: bool,
}

impl FusedCounter {
    /// A counter armed to `limits.step_budget`, with an unbounded initial slice.
    pub(crate) fn new(limits: &Limits) -> Self {
        FusedCounter {
            budget: limits.step_budget,
            slice: None,
            exhausted: false,
        }
    }

    /// Arms the slice fuel for one drive call (S-40): `Some(n)` bounds it to `n` safe
    /// points and yields `Paused(SliceEnd)` when spent; `None` runs unbounded. `Some(0)`
    /// is already spent (yields `SliceEnd` before running any safe point). The lifetime
    /// step budget is untouched.
    pub(crate) fn arm_slice(&mut self, fuel: Option<u64>) {
        self.slice = fuel;
        self.exhausted = fuel == Some(0);
    }

    /// Whether the current slice's fuel is spent (S-40) — the top-level drive loop's
    /// signal to pause with `Paused(SliceEnd)`.
    pub(crate) fn sliced_out(&self) -> bool {
        self.exhausted
    }

    /// Spends one statement safe point against both counters (AD6). Faults if the
    /// lifetime step budget is out; otherwise decrements the budget and — for a
    /// **bounded** slice — the slice fuel, flagging `exhausted` when it just reached `0`.
    /// An **unbounded** slice (`None`) never counts down or flags. This is only the
    /// counter accounting; the drive loop turns the flag into a `Paused(SliceEnd)`.
    fn spend(&mut self) -> Result<(), EngineFault> {
        if self.budget == 0 {
            return Err(EngineFault::LimitExceeded(LimitKind::StepBudget));
        }
        self.budget -= 1;
        if let Some(slice) = self.slice.as_mut()
            && *slice > 0
        {
            *slice -= 1;
            if *slice == 0 {
                self.exhausted = true;
            }
        }
        Ok(())
    }

    /// Charges `cost` work units against the lifetime step budget (R8): a bignum-producing
    /// `*`/`**` costs proportional to its result size, not the flat 1 a statement costs, so
    /// the budget bounds real work and a single huge operation cannot slip under it. Returns
    /// `false` if the budget cannot cover `cost` (the caller faults `StepBudget` *without*
    /// doing the work); otherwise deducts it. The **slice fuel** is untouched — it paces
    /// safe points for responsiveness, and an atomic bignum op cannot yield mid-computation.
    pub(crate) fn charge(&mut self, cost: u64) -> bool {
        if self.budget < cost {
            return false;
        }
        self.budget -= cost;
        true
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
pub(crate) fn safe_point(heap: &mut Heap, machine: &mut Machine) -> Result<(), EngineFault> {
    // Spend one safe point against the fused step-budget + slice-fuel counter (AD6): a
    // `StepBudget` fault if the lifetime budget is out, else flag a `SliceEnd` if the
    // slice fuel just ran out. The slice fuel **does not gate execution** here — the
    // statement at this safe point still runs, and GC/heap limits below still fire, so a
    // sliced run executes exactly what an unbounded run does (the pause happens *between*
    // safe points, in the drive loop) and never perturbs the heap (E§7.7). A nested drive
    // cannot pause, so it runs on with the flag set until control returns to the top.
    machine.fuel.spend()?;
    // Collect when accounted bytes cross the GC threshold (routine growth) or the
    // heap limit (the last-ditch collect MD §15 requires before the limit can
    // fault) — whichever is lower. Then re-arm the threshold at the survivors' next
    // doubling (floored at GC_MIN_BYTES), so a large *live* set is not swept every
    // safe point.
    // (In a GC-stress test, `gc_every_safe_point` collects unconditionally — the re-arm
    // is then irrelevant, the flag overrides it at the next safe point.)
    if machine.gc_every_safe_point
        || heap.bytes_allocated() > machine.gc_threshold.min(machine.limits.heap_bytes)
    {
        gc::collect(heap, machine);
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

    /// Magnitude guard for a **result-growing operation** (`*`/`**`, string/list repetition, MD
    /// §9): given the result's estimated size in bytes, park a `LimitExceeded` fault **before**
    /// the operation runs if it would exceed one of the three rails — the **heap** (space), the
    /// **per-operation** result cap (latency, R8/S-40), or the remaining **step budget** (total
    /// work; a huge result cannot slip under a flat per-statement cost). Returns `false` if a fault
    /// was parked — the caller aborts with a placeholder value, and `step` surfaces the parked
    /// fault before the next transition, so the placeholder is never observed. `true` = proceed.
    ///
    /// The order picks the most fundamental reason: a result past the heap is *out of memory*
    /// (`Heap`); one that fits the heap but exceeds the per-op cap is *too big to compute in one
    /// step* (`OpResult`, the latency rail — the fix for a single op that would otherwise freeze
    /// the host, since an atomic op cannot yield mid-way, S-40). The estimate is a deterministic
    /// upper bound (E§11): the fault fires at the same operation for the same program and limits,
    /// and no oversized allocation is ever begun.
    pub(crate) fn admit_op_result(&mut self, result_bytes: u128) -> bool {
        if result_bytes > u128::from(self.limits.heap_bytes) {
            self.set_pending_fault(EngineFault::LimitExceeded(LimitKind::Heap));
            return false;
        }
        if result_bytes > u128::from(self.limits.max_op_result_bytes) {
            self.set_pending_fault(EngineFault::LimitExceeded(LimitKind::OpResult));
            return false;
        }
        // Past the heap check `result_bytes <= heap_bytes <= u64::MAX`, so the cast is exact.
        if !self.fuel.charge(result_bytes as u64) {
            self.set_pending_fault(EngineFault::LimitExceeded(LimitKind::StepBudget));
            return false;
        }
        true
    }
}
