//! The host-callback boundary (E§5.2/§5.4, M7.2b): a host foreign function's synchronous
//! callback runs *inside* the engine's drive and may **call back in** — to read its arguments,
//! invoke its block, emit output, and report a result or a raise. The genuinely hard soundness
//! spot (FFI review C-3): the callback's re-entry must route through the `&mut IntrinsicCtx`
//! the engine already threaded down — never reconstruct a second `&mut Instance` (two live
//! `&mut Instance` is instantaneous UB, independent of any data race) — and its window of
//! validity is exactly the callback's dynamic extent.
//!
//! # Soundness
//!
//! - **No second `&mut Instance`.** A [`DoodleCallCtx`] wraps the raw `&mut IntrinsicCtx`; every
//!   `doodle_call_*` reaches the engine through it, and `doodle_call_block` routes to
//!   [`IntrinsicCtx::invoke_block_handles`], so the instance pointer is never touched inside the
//!   callback. The outer `doodle_drive`'s borrow stays the only `&mut Instance`.
//! - **Lifetime.** The `DoodleCallCtx` is valid only while the C callback runs. A one-shot
//!   `live` flag is flipped off when the trampoline returns; every `doodle_call_*` checks it
//!   (→ `ErrContract`), so a stashed-and-reused ctx is caught, not a dangling deref.
//! - **Panic across the boundary.** The C callback runs inside the engine's `step`; the
//!   trampoline wraps the call in `catch_unwind` so a Rust panic (a nested Doodle call's
//!   `debug_assert!`/`unreachable!`, or a Rust-implemented callback) cannot cross the FFI — it
//!   becomes an `Internal` fault. Each `doodle_call_*` likewise runs under [`catch`], since it
//!   too is a Rust→C→Rust re-entry that must not unwind into the C caller.

use crate::abi::{self, DoodleBlockOutcome, DoodleHandle, DoodleStatus};
use crate::guard::catch;
use crate::value::write_out;
use doodle_core::machine::{Handle, HostCallback, HostReply, IntrinsicCtx};
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

/// A host `user_data` pointer the foreign-callback closure carries (the C `void*`). Raw
/// pointers are neither `Send` nor `Sync`; the ABI (the sole `unsafe` crate, AD1) asserts both
/// because an instance is driven from **one thread at a time** (E§11 determinism), so the
/// closure — and thus this pointer — is only ever touched on that single thread. Opaque to the
/// engine: handed to the C callback verbatim, never dereferenced here.
#[derive(Clone, Copy)]
pub(crate) struct SendPtr(pub(crate) *mut c_void);
// SAFETY: single-threaded use only — see the type doc.
unsafe impl Send for SendPtr {}
// SAFETY: single-threaded use only — see the type doc.
unsafe impl Sync for SendPtr {}

impl SendPtr {
    /// The wrapped raw pointer, consuming the wrapper. Taking `self` by value is a **whole**-
    /// value use, so a closure that calls this captures the `Send + Sync` `SendPtr` rather than
    /// the bare `*mut c_void` its field would capture under 2021 disjoint captures.
    fn into_raw(self) -> *mut c_void {
        self.0
    }
}

/// A C host foreign-function callback (E§5.2): given a [`DoodleCallCtx`] (valid only for the
/// call's dynamic extent) and its registered `user_data`, it reads arguments, may invoke its
/// block, and reports a result/raise via `doodle_call_set_result`/`doodle_call_set_raise`.
/// Returns `DoodleStatus_Ok` on success; a non-`Ok` return (or a panic) faults the drive
/// `Internal` — a Doodle-level error is a *raise* (`doodle_call_set_raise`), not a status.
pub type DoodleForeignFn =
    extern "C" fn(ctx: *mut DoodleCallCtx, user_data: *mut c_void) -> DoodleStatus;

/// What the C callback reported before returning (set by `doodle_call_set_result`/`_set_raise`).
#[derive(Clone, Copy)]
enum CallReply {
    /// Nothing set — Void for a `to` (a missing value for a `fn`, caught at the apply site).
    None,
    /// A `fn` result value (host handle).
    Result(DoodleHandle),
    /// Raise this value (host handle) at the call site.
    Raise(DoodleHandle),
}

/// The re-entry context handed to a host foreign callback (opaque to C). Wraps the raw
/// `&mut IntrinsicCtx` the engine threaded down, a one-shot liveness flag, and the callback's
/// pending reply. **Valid only for the dynamic extent of the callback** — the `live` flag makes
/// a stashed-and-reused ctx a defined `ErrContract`, never a dangling deref.
pub struct DoodleCallCtx {
    /// The engine activation, type-erased (its lifetime cannot be named in an FFI struct field).
    /// Cast back to `&mut IntrinsicCtx` only while `live`.
    ctx: *mut c_void,
    /// Whether the wrapped ctx is still valid (the callback has not yet returned).
    live: bool,
    /// The callback's reported result/raise.
    reply: CallReply,
}

/// Builds the engine [`HostCallback`] that drives the C `callback` with `user_data` (M7.2b):
/// wrap the `&mut IntrinsicCtx` as a [`DoodleCallCtx`], call the C fn under `catch_unwind`, flip
/// the ctx dead, and map its reply to a [`HostReply`]. A non-`Ok` status or a caught panic
/// faults the drive `Internal` ([`IntrinsicCtx::fault_host`]) — `apply` then stops on the parked
/// fault, superseding the reply.
pub(crate) fn trampoline(callback: DoodleForeignFn, user_data: SendPtr) -> HostCallback {
    Arc::new(move |ctx: &mut IntrinsicCtx| {
        // Take the whole `SendPtr` (via a `self`-consuming method) so the closure captures the
        // `Send + Sync` wrapper, not the bare `*mut c_void` (2021 disjoint captures).
        let user_data = user_data.into_raw();
        // Derive the raw pointer from the `&mut` and use it *exclusively* until the C call
        // returns (never touch `ctx` again in between) — the standard raw-from-`&mut` pattern,
        // so the callback's re-entries and the outer borrow never alias.
        let mut callctx = DoodleCallCtx {
            ctx: (ctx as *mut IntrinsicCtx).cast::<c_void>(),
            live: true,
            reply: CallReply::None,
        };
        let callctx_ptr: *mut DoodleCallCtx = &mut callctx;
        let status = catch_unwind(AssertUnwindSafe(|| callback(callctx_ptr, user_data)))
            .unwrap_or(DoodleStatus::ErrPanic);
        // The ctx is dead the instant the callback returns: a `doodle_call_*` on a stashed copy
        // now hits `ErrContract`, not the (about-to-be-invalid) raw pointer.
        callctx.live = false;
        match (status, callctx.reply) {
            (DoodleStatus::Ok, CallReply::None) => HostReply::Value(None),
            (DoodleStatus::Ok, CallReply::Result(h)) => {
                HostReply::Value(Some(Handle::from_bits(h)))
            }
            (DoodleStatus::Ok, CallReply::Raise(h)) => HostReply::Raise(Handle::from_bits(h)),
            // A non-`Ok` status or a caught panic: an ABI-level host failure a raise can't
            // express. Fault the drive; the returned reply is ignored (apply checks the fault
            // first). Re-borrowing `ctx` is sound — the raw pointer is dead (unused after the
            // call), and this is the original `&mut` reasserting exclusive access.
            _ => {
                ctx.fault_host();
                HostReply::Value(None)
            }
        }
    })
}

/// Borrows the [`DoodleCallCtx`] behind a raw pointer if it is non-NULL and still **live** (the
/// callback has not returned): `ErrNullPointer`/`ErrContract` otherwise. A dead ctx (stashed
/// past the callback) is caught here, so the engine pointer is never dereferenced stale.
fn live_ctx<'a>(ctx: *mut DoodleCallCtx) -> Result<&'a mut DoodleCallCtx, DoodleStatus> {
    // SAFETY: `as_mut` returns None for NULL; a non-null `ctx` is the `DoodleCallCtx` the
    // trampoline passed this callback, and the host's contract is to use it only during the
    // call — the `live` flag downgrades a use-after-return to `ErrContract`, never UB.
    let Some(cc) = (unsafe { ctx.as_mut() }) else {
        return Err(DoodleStatus::ErrNullPointer);
    };
    if cc.live {
        Ok(cc)
    } else {
        Err(DoodleStatus::ErrContract)
    }
}

/// The live engine activation behind a `DoodleCallCtx` (its `&mut IntrinsicCtx`). The returned
/// lifetime is unconstrained (an FFI raw deref); each caller uses it within one call, and the
/// [`live_ctx`] check plus the single-threaded drive keep it valid.
fn engine_ctx<'a>(cc: &mut DoodleCallCtx) -> &'a mut IntrinsicCtx<'a> {
    // SAFETY: `cc.ctx` is the `&mut IntrinsicCtx` the engine threaded into the trampoline,
    // valid because `cc.live` (the caller checked via `live_ctx`): the callback runs on the
    // Rust stack above the engine's `apply`, which handed it exclusive access, so no other
    // `&mut` to that ctx is live. Block re-entry routes through `invoke_block_handles`, so this
    // forms no second `&mut Instance`.
    unsafe { &mut *(cc.ctx as *mut IntrinsicCtx<'a>) }
}

/// The live engine activation behind a raw `DoodleCallCtx`, or an error status — the null +
/// liveness check ([`live_ctx`]) then the engine deref ([`engine_ctx`]). Shared with the
/// ctx value functions ([`crate::call_value`]).
pub(crate) fn engine_of<'a>(
    ctx: *mut DoodleCallCtx,
) -> Result<&'a mut IntrinsicCtx<'a>, DoodleStatus> {
    Ok(engine_ctx(live_ctx(ctx)?))
}

/// Writes the number of bound (non-block) arguments this call received (E§5.2) — read each with
/// [`doodle_call_arg`]. `ErrContract` if the ctx has outlived the callback.
///
/// # Safety
/// `ctx` must be the callback's live `DoodleCallCtx`; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_arg_count(
    ctx: *mut DoodleCallCtx,
    out: *mut u32,
) -> DoodleStatus {
    catch(|| match engine_of(ctx) {
        Ok(engine) => {
            let count = u32::try_from(engine.arg_count()).unwrap_or(u32::MAX);
            if write_out(out, count) {
                DoodleStatus::Ok
            } else {
                DoodleStatus::ErrNullPointer
            }
        }
        Err(status) => status,
    })
}

/// Interns the `index`-th bound argument as a fresh **host-owned** handle (E§4.2) and writes it
/// to `out`; the host reads it with the ordinary `doodle_as_*` and `doodle_release`s it.
/// `ErrIndexOutOfBounds` past the last argument; `ErrContract` if the ctx has outlived the call.
///
/// # Safety
/// `ctx` must be the callback's live `DoodleCallCtx`; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_arg(
    ctx: *mut DoodleCallCtx,
    index: u32,
    out: *mut DoodleHandle,
) -> DoodleStatus {
    catch(|| {
        let engine = match engine_of(ctx) {
            Ok(engine) => engine,
            Err(status) => return status,
        };
        match engine.arg_handle(index as usize) {
            Some(handle) if write_out(out, handle.bits()) => DoodleStatus::Ok,
            Some(_) => DoodleStatus::ErrNullPointer,
            None => DoodleStatus::ErrIndexOutOfBounds,
        }
    })
}

/// Invokes this call's received block **reentrantly** (E§5.4/§7.6) with the `n` argument handles
/// at `args`, writing the [`DoodleBlockOutcome`] to `out`. On `NonLocalExit`/`Halted` the
/// callback **must return promptly with no result**. Routes through
/// [`IntrinsicCtx::invoke_block_handles`] (never a second `&mut Instance`). `ErrContract` if the
/// ctx has outlived the callback.
///
/// # Safety
/// `ctx` must be the callback's live `DoodleCallCtx`; `args` points to `n` readable handles (or
/// NULL when `n` is 0); `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_block(
    ctx: *mut DoodleCallCtx,
    args: *const DoodleHandle,
    n: u32,
    out: *mut DoodleBlockOutcome,
) -> DoodleStatus {
    catch(|| {
        let engine = match engine_of(ctx) {
            Ok(engine) => engine,
            Err(status) => return status,
        };
        let handles: Vec<Handle> = if n == 0 {
            Vec::new()
        } else if args.is_null() {
            return DoodleStatus::ErrNullPointer;
        } else {
            // SAFETY: `n != 0` and `args` is non-null (checked), so by the caller's contract it
            // points to `n` readable `DoodleHandle`s; the slice is copied out, not held.
            let slice = unsafe { std::slice::from_raw_parts(args, n as usize) };
            slice.iter().map(|&h| Handle::from_bits(h)).collect()
        };
        let outcome = abi::block_outcome(engine.invoke_block_handles(&handles));
        if write_out(out, outcome) {
            DoodleStatus::Ok
        } else {
            DoodleStatus::ErrNullPointer
        }
    })
}

/// Appends `len` bytes at `bytes` to the instance's output sink (the same sink `print` writes,
/// read via `doodle_output`). `ErrContract` if the ctx has outlived the callback.
///
/// # Safety
/// `ctx` must be the callback's live `DoodleCallCtx`; `bytes` points to `len` readable bytes (or
/// NULL when `len` is 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_emit(
    ctx: *mut DoodleCallCtx,
    bytes: *const u8,
    len: usize,
) -> DoodleStatus {
    catch(|| {
        let engine = match engine_of(ctx) {
            Ok(engine) => engine,
            Err(status) => return status,
        };
        if len == 0 {
            engine.emit(&[]);
            return DoodleStatus::Ok;
        }
        if bytes.is_null() {
            return DoodleStatus::ErrNullPointer;
        }
        // SAFETY: `len != 0` and `bytes` is non-null (checked), so by the caller's contract it
        // points to `len` readable bytes; the slice is not held past the `emit` call.
        let slice = unsafe { std::slice::from_raw_parts(bytes, len) };
        engine.emit(slice);
        DoodleStatus::Ok
    })
}

/// Sets this `fn` call's result to the value named by `handle` (E§5.2) — the value the call
/// yields. A `to` sets nothing (leaving Void). **Consumes `handle`**: the engine resolves it and
/// releases it after the callback returns, so the host must not release or reuse it afterward.
/// `ErrContract` if the ctx has outlived the call.
///
/// # Safety
/// `ctx` must be the callback's live `DoodleCallCtx`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_set_result(
    ctx: *mut DoodleCallCtx,
    handle: DoodleHandle,
) -> DoodleStatus {
    catch(|| match live_ctx(ctx) {
        Ok(cc) => {
            cc.reply = CallReply::Result(handle);
            DoodleStatus::Ok
        }
        Err(status) => status,
    })
}

/// Raises the value named by `handle` at this call site (E§5.2/§7.5), like a capability's
/// `resolve` raise — the value *is* the exception (a program `rescue e` binds it as-is). The
/// callback should then return `DoodleStatus_Ok`. **Consumes `handle`** (as
/// `doodle_call_set_result` does): do not release or reuse it. `ErrContract` if the ctx is dead.
///
/// # Safety
/// `ctx` must be the callback's live `DoodleCallCtx`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_set_raise(
    ctx: *mut DoodleCallCtx,
    handle: DoodleHandle,
) -> DoodleStatus {
    catch(|| match live_ctx(ctx) {
        Ok(cc) => {
            cc.reply = CallReply::Raise(handle);
            DoodleStatus::Ok
        }
        Err(status) => status,
    })
}
