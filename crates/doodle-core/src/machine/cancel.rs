//! The host cancellation handle (engine spec E§10.1): the instance's **stop button**.
//! Split from `machine.rs` (the `Machine`/`Instance` definitions) for length; the flag it
//! shares lives on the [`Machine`](super::Machine), and the safe-point poll that *arms* the
//! cancel unwind is [`Machine::poll_cancel`](super::Machine).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A cancellation handle for an instance (engine spec E§10.1): the host's **stop
/// button**. Cloneable and thread-safe, so the host can request cancellation from
/// another thread (or a signal handler) while a drive is running — or before one. The
/// engine polls it at the instance's next safe point, unwinds the stack (running block/
/// `with` cleanup, as for an exception), and returns
/// [`Faulted(Cancelled)`](crate::drive::EngineFault::Cancelled); cancellation is **not**
/// catchable by Doodle code.
#[derive(Clone)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// Builds a token over an instance's shared cancel flag.
    pub(crate) fn new(flag: Arc<AtomicBool>) -> Self {
        CancelToken(flag)
    }

    /// Requests cancellation. Idempotent; takes effect at the instance's next safe point
    /// (or the first safe point of the next drive, if requested while not running).
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether cancellation has been requested through this (or any cloned) token.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}
