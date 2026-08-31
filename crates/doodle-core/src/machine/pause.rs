//! The host pause handle (engine spec E§8.8): the instance's **pause button**.
//! Split from `machine.rs` (the `Machine`/`Instance` definitions) alongside its sibling
//! `cancel.rs`; the flag it shares lives on the [`Machine`](super::Machine), and the
//! read-and-clear the drive loop uses to consume a pending request is
//! [`Instance::take_host_pause`](super::Instance).
//!
//! A pause is **not** a fault: unlike [`CancelToken`](super::CancelToken), which tears
//! the stack down to `Faulted(Cancelled)`, a pause stops with the state fully intact and
//! **resumable** — re-driving continues exactly where it stopped. It is **one-shot**: the
//! drive loop consumes the request when it fires (so a re-drive is not immediately paused
//! again), and it fires **regardless of directive** (E§8.8) — a host control, like cancel,
//! not a `Step*` decision.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A host pause handle for an instance (engine spec E§8.8): the host's **pause button**.
/// Cloneable and thread-safe, so the host can request a pause from another thread (or a
/// UI event) while a drive is running — or before one. The engine stops with
/// [`Paused(HostPause)`](crate::drive::PauseReason::HostPause) at the instance's next safe
/// point, with state intact and resumable; a pause is **not** catchable by Doodle code and
/// **not** a fault.
#[derive(Clone)]
pub struct PauseToken(Arc<AtomicBool>);

impl PauseToken {
    /// Builds a token over an instance's shared host-pause flag.
    pub(crate) fn new(flag: Arc<AtomicBool>) -> Self {
        PauseToken(flag)
    }

    /// Requests a pause. Idempotent; takes effect at the instance's next safe point (or the
    /// first safe point of the next drive, if requested while not running). Consumed when it
    /// fires, so it pauses **once** per request — re-arm it to pause again.
    pub fn pause(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether a pause has been requested through this (or any cloned) token and not yet
    /// consumed by a drive.
    pub fn is_pause_requested(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}
