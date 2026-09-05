//! The cross-thread **control** surface (M7.6, D-M7-5): a standalone [`DoodleControl`] holding an
//! instance's cancel + pause tokens, obtained **once** on the thread that owns the instance and
//! then usable from ANOTHER thread to cancel/pause a running drive **without** re-forming a
//! `&DoodleInstance`.
//!
//! Why this exists: [`doodle_cancel`](crate::instance::doodle_cancel) /
//! [`doodle_pause`](crate::observe::doodle_pause) reach the cancel/pause flag by forming
//! `&DoodleInstance` on every call. Same-thread that is fine, but from another thread while a drive
//! holds `&mut Instance` it is a shared+mutable alias of the same instance (UB in Rust's model,
//! even though only the atomic flag is touched). The engine's tokens ([`CancelToken`] /
//! [`PauseToken`]) each own a **clone** of the instance's shared cancel/pause `Arc<AtomicBool>`, so
//! they reach the (heap-allocated) atomic directly, never through the `Instance`. Capturing them
//! once in a `DoodleControl` is the sound cross-thread stop/pause button (the tokens are `Send`).

use crate::instance::{DoodleInstance, di_ref};
use doodle_core::machine::{CancelToken, PauseToken};
use std::ptr;

/// An instance's cross-thread control handle (E§8.8/§10.1): its cancel + pause tokens, each an
/// `Arc<AtomicBool>` clone. `Send + Sync`, so a host obtains it on the owning thread and may hand
/// it to another thread. Opaque to C; free with [`doodle_control_free`]. Outlives drives.
pub struct DoodleControl {
    cancel: CancelToken,
    pause: PauseToken,
}

/// Obtains a [`DoodleControl`] for `instance` (D-M7-5). **Call on the thread that owns the
/// instance** — typically before handing the instance to a drive thread; it forms `&instance`
/// exactly here, so it must not race a concurrent drive. Returns NULL on a NULL instance. The
/// returned control is thread-safe and the host frees it with [`doodle_control_free`].
///
/// # Safety
/// `instance` must be a live pointer from `doodle_load` (or NULL), not concurrently driven on
/// another thread at the moment of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_control(instance: *const DoodleInstance) -> *mut DoodleControl {
    match di_ref(instance) {
        Some(di) => Box::into_raw(Box::new(DoodleControl {
            cancel: di.inner.cancel_token(),
            pause: di.inner.pause_token(),
        })),
        None => ptr::null_mut(),
    }
}

/// Requests cancellation through `control` (E§10.1): the drive tears down to `Faulted(Cancelled)`
/// at its next safe point. Sound from any thread — it touches only the token's own atomic, never
/// the instance. Idempotent; no-op on NULL.
///
/// # Safety
/// `control` must be a live pointer from [`doodle_control`] (or NULL).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_control_cancel(control: *const DoodleControl) {
    // SAFETY: a live `DoodleControl` from `doodle_control`, or NULL. A shared `&DoodleControl` is
    // sound from any thread (the tokens are `Sync`; no `&mut Instance` is formed).
    if let Some(control) = unsafe { control.as_ref() } {
        control.cancel.cancel();
    }
}

/// Requests a pause through `control` (E§8.8): the drive stops `Paused(HostPause)` at its next
/// safe point, resumable. Sound from any thread. No-op on NULL.
///
/// # Safety
/// `control` must be a live pointer from [`doodle_control`] (or NULL).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_control_pause(control: *const DoodleControl) {
    // SAFETY: as `doodle_control_cancel`.
    if let Some(control) = unsafe { control.as_ref() } {
        control.pause.pause();
    }
}

/// Frees a [`DoodleControl`] from [`doodle_control`]. NULL is a no-op. Do not free while another
/// thread may still call `doodle_control_cancel`/`_pause` on it.
///
/// # Safety
/// `control` must be a pointer from [`doodle_control`] not already freed, and no longer in use.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_control_free(control: *mut DoodleControl) {
    if !control.is_null() {
        // SAFETY: a `Box::into_raw` pointer from `doodle_control`, not freed/in-use, by contract.
        drop(unsafe { Box::from_raw(control) });
    }
}
