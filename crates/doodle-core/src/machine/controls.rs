//! The host **control** surface on [`Instance`]: the cancel stop button (E§10.1) and the
//! pause button (E§8.8), plus the cancel-reap a late `resolve` uses. Split from `machine.rs`
//! (the `Machine`/`Instance` definitions) to keep that file within the hygiene length limit;
//! the flags these read live on the [`Machine`](super::Machine), and the token types are
//! `machine/cancel.rs` and `machine/pause.rs`.

use super::{CancelToken, Instance, PauseToken, unwind};
use std::sync::Arc;
use std::sync::atomic::Ordering;

impl Instance {
    /// A [`CancelToken`] for this instance (E§10.1): the host's stop button. The token
    /// is cloneable and thread-safe, so a host may hold it (or a clone) elsewhere — e.g.
    /// on another thread — and request cancellation while a drive is running. All tokens
    /// for one instance share its cancel flag.
    pub fn cancel_token(&self) -> CancelToken {
        CancelToken::new(Arc::clone(&self.machine.cancel))
    }

    /// Whether host cancellation has been requested (E§10.1) — a plain read of the cancel
    /// flag, distinct from the safe-point poll ([`Machine::poll_cancel`]) that *arms* the
    /// unwind. Lets `resolve` (E§7.5) reap a cancellation that arrived while the instance
    /// was suspended, so a host raise racing the stop button does not escape it (S-23).
    pub(crate) fn cancel_requested(&self) -> bool {
        self.machine.cancel.load(Ordering::Relaxed)
    }

    /// A [`PauseToken`] for this instance (E§8.8): the host's pause button. The token is
    /// cloneable and thread-safe, so a host may hold it (or a clone) elsewhere — e.g. on a
    /// UI thread — and request a pause while a drive is running. All tokens for one instance
    /// share its pause flag.
    pub fn pause_token(&self) -> PauseToken {
        PauseToken::new(Arc::clone(&self.machine.host_pause))
    }

    /// Consumes a pending host-pause request (E§8.8): atomically reads **and clears** the
    /// pause flag, returning whether one was set. The drive loop calls this at each safe
    /// point; a `true` stops the drive `Paused(HostPause)` with state intact and resumable.
    /// Clearing here makes the request one-shot, so the re-drive continues rather than
    /// pausing again on the same request. Distinct from cancel's [`poll_cancel`]: no unwind
    /// is armed and no fault is raised — the pause is a resumable stop, not a teardown.
    pub(crate) fn take_host_pause(&mut self) -> bool {
        self.machine.host_pause.swap(false, Ordering::Relaxed)
    }

    /// Discards a parked capability request and arms the cancel unwind (E§10.1, S-23):
    /// resuming the drive then tears the stack down to `Faulted(Cancelled)` **without**
    /// running the parked call's continuation, so a host resolution that lost to a pending
    /// cancellation has no program-visible effect. Only valid while suspended — a request is
    /// parked and the frame stack is non-empty (a suspend never empties it), which the
    /// caller establishes by checking [`cancel_requested`](Self::cancel_requested) at a
    /// `Suspended` instance.
    pub(crate) fn discard_pending_and_cancel(&mut self) {
        self.machine.pending = None;
        debug_assert!(
            self.machine.unwind.is_none() && !self.machine.frames.is_empty(),
            "cancel-reap requires a parked suspension with an intact stack"
        );
        self.machine.unwind = Some(unwind::Unwind::Cancel);
    }
}
