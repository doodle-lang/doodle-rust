//! Small [`Machine`] state helpers (machine-design §8–§15): frame-serial minting,
//! the parked-fault slot, foreign-call rooting, reentrancy depth, the tail-elided ring,
//! and the safe-point cancel poll. Split from `machine.rs` (the `Machine`/`Instance`
//! definitions) so that file stays within the hygiene length limit; this is part of the
//! same `impl Machine`.

use super::{CalIdx, MAX_REENTRY_DEPTH, Machine, Value, ring, unwind};
use crate::drive::EngineFault;
use std::sync::atomic::Ordering;

impl Machine {
    /// The next frame serial (post-increment): a fresh, monotonic frame identity.
    pub(crate) fn next_frame_serial(&mut self) -> u64 {
        let serial = self.frame_serial;
        self.frame_serial += 1;
        serial
    }

    /// Takes any fault parked during the transition (`step` surfaces it, MD §14).
    pub(crate) fn take_pending_fault(&mut self) -> Option<EngineFault> {
        self.pending_fault.take()
    }

    /// Parks a fault for `step` to surface, for a transition that cannot return an
    /// `EngineFault` through its Raise-typed result (a string `*` over the heap limit).
    pub(crate) fn set_pending_fault(&mut self, fault: EngineFault) {
        self.pending_fault = Some(fault);
    }

    /// Roots the arguments of an entering synchronous foreign call (MD §15), returning
    /// the prior stack length to [`pop_foreign_roots`](Self::pop_foreign_roots) on return.
    pub(crate) fn push_foreign_roots(&mut self, values: &[Value]) -> usize {
        let base = self.foreign_roots.len();
        self.foreign_roots.extend_from_slice(values);
        base
    }

    /// Un-roots a returning foreign call's arguments (truncating to `base`).
    pub(crate) fn pop_foreign_roots(&mut self, base: usize) {
        self.foreign_roots.truncate(base);
    }

    /// Whether entering another reentrant drive would exceed the native-stack nesting
    /// bound (MD §14) — a program recursing through a native block-consumer must fault,
    /// not overflow the Rust stack.
    pub(crate) fn reentry_would_overflow(&self) -> bool {
        self.reentry_depth >= MAX_REENTRY_DEPTH
    }

    /// Enters a reentrant drive (increments the nesting depth); pair with [`exit_reentry`].
    pub(crate) fn enter_reentry(&mut self) {
        self.reentry_depth += 1;
    }

    /// Leaves a reentrant drive (decrements the nesting depth).
    pub(crate) fn exit_reentry(&mut self) {
        self.reentry_depth -= 1;
    }

    /// Records a tail-elided frame in the ring (machine-design §11).
    pub(crate) fn record_elided(&mut self, callable: CalIdx, consuming_serial: u64) {
        self.ring.record(ring::ElidedFrame {
            callable,
            consuming_serial,
        });
    }

    /// Polls the host cancel flag at a safe point (E§10.1). If cancellation was
    /// requested and no transfer is already in flight, **arms the cancel unwind** (§12)
    /// and returns `true`; the caller (`step`) then yields so the drive runs the
    /// teardown. The common no-cancel case is a single relaxed atomic load — the whole
    /// hot-path cost.
    ///
    /// A cancel first observed at the safe point that **drains the last frame** (the
    /// module's completing transition) is *not* armed: the program has fully executed and
    /// there is nothing left to unwind, so it completes rather than arming a dead unwind
    /// on a terminal instance (a cancel racing exactly with completion loses to it).
    pub(crate) fn poll_cancel(&mut self) -> bool {
        if self.unwind.is_none() && !self.frames.is_empty() && self.cancel.load(Ordering::Relaxed) {
            self.unwind = Some(unwind::Unwind::Cancel);
            true
        } else {
            false
        }
    }
}
