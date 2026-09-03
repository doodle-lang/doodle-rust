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
//! - **No second `&mut IntrinsicCtx` (hence no second `&mut Instance`).** Every `doodle_call_*`
//!   reaches the engine only through the **innermost** live [`DoodleCallCtx`], and
//!   `doodle_call_block` routes to [`IntrinsicCtx::invoke_block_handles`], so the instance
//!   pointer is never touched inside a callback and the outer `doodle_drive`'s borrow stays the
//!   only `&mut Instance`. The catch is re-entrancy: a `doodle_call_block` runs a nested drive
//!   that may call a foreign function again, whose callback is a **deeper** activation. While
//!   that deeper drive runs, the ancestor ctx's `&mut IntrinsicCtx`/`&mut Machine` is reborrowed
//!   into it, so touching the ancestor ctx would form a second, aliasing `&mut` — the same
//!   instantaneous UB, one level up. Prevented by the innermost gate below.
//! - **The innermost gate.** A thread-local [`CURRENT_CTX`] points to the innermost live
//!   `DoodleCallCtx` on this thread; the trampoline sets it on entry and **restores the previous
//!   on return** (so nesting is a stack). Every `doodle_call_*` accepts a ctx pointer only if it
//!   **equals** `CURRENT_CTX` — a pure pointer comparison that never dereferences the passed
//!   pointer first. This rejects (`ErrContract`) both a *returned* ctx (no longer current) and an
//!   *ancestor* ctx (a deeper call is current), so the only `&mut IntrinsicCtx` ever formed is to
//!   the innermost activation, with no ancestor aliasing and no read of freed stack memory. The
//!   single-threaded drive (E§11) makes the thread-local the whole story.
//! - **Panic across the boundary.** The C callback runs inside the engine's `step`; the
//!   trampoline wraps the call in `catch_unwind` so a Rust panic (a nested Doodle call's
//!   `debug_assert!`/`unreachable!`, or a Rust-implemented callback) cannot cross the FFI — it
//!   becomes an `Internal` fault. Each `doodle_call_*` likewise runs under [`catch`], since it
//!   too is a Rust→C→Rust re-entry that must not unwind into the C caller.

use crate::abi::{self, DoodleBlockOutcome, DoodleHandle, DoodleStatus};
use crate::guard::catch;
use crate::value::write_out;
use doodle_core::machine::{Handle, HostCallback, HostReply, IntrinsicCtx};
use std::cell::Cell;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

thread_local! {
    /// The **innermost** live [`DoodleCallCtx`] on this thread — the only one a `doodle_call_*`
    /// may touch. The trampoline sets it to its on-stack `callctx` on entry and restores the
    /// previous on return, so a reentrant nested foreign call makes the deeper ctx current and
    /// pops back to the ancestor on return. A `doodle_call_*` gates on *pointer equality* with
    /// this (never dereferencing the passed pointer first), so a *returned* ctx and an *ancestor*
    /// ctx mid-drive are both a defined `ErrContract`, never a dangling or aliasing deref.
    static CURRENT_CTX: Cell<*mut DoodleCallCtx> = const { Cell::new(std::ptr::null_mut()) };
}

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
/// `&mut IntrinsicCtx` the engine threaded down plus the callback's pending reply. **Valid only
/// while it is the innermost active callback** — its validity is tracked by [`CURRENT_CTX`], not
/// a field, so use-after-return and reentrant-ancestor use are both a defined `ErrContract`.
pub struct DoodleCallCtx {
    /// The engine activation, type-erased (its lifetime cannot be named in an FFI struct field).
    /// Cast back to `&mut IntrinsicCtx` only after a [`CURRENT_CTX`] pointer-equality check.
    ctx: *mut c_void,
    /// The callback's reported result/raise.
    reply: CallReply,
}

/// Builds the engine [`HostCallback`] that drives the C `callback` with `user_data` (M7.2b):
/// wrap the `&mut IntrinsicCtx` as a [`DoodleCallCtx`], make it the innermost active ctx
/// ([`CURRENT_CTX`]), call the C fn under `catch_unwind`, restore the previous innermost, and map
/// its reply to a [`HostReply`]. A non-`Ok` status or a caught panic faults the drive `Internal`
/// ([`IntrinsicCtx::fault_host`]) — `apply` stops on the parked fault, superseding the reply.
pub(crate) fn trampoline(callback: DoodleForeignFn, user_data: SendPtr) -> HostCallback {
    Arc::new(move |ctx: &mut IntrinsicCtx| {
        // Take the whole `SendPtr` (via a `self`-consuming method) so the closure captures the
        // `Send + Sync` wrapper, not the bare `*mut c_void` (2021 disjoint captures).
        let user_data = user_data.into_raw();
        // Derive the raw pointer from the `&mut` and use it *exclusively* until the C call
        // returns (never touch `ctx` again in between) — the standard raw-from-`&mut` pattern.
        let mut callctx = DoodleCallCtx {
            ctx: (ctx as *mut IntrinsicCtx).cast::<c_void>(),
            reply: CallReply::None,
        };
        let callctx_ptr: *mut DoodleCallCtx = &mut callctx;
        // This callback is now the innermost active ctx; save the previous (an ancestor, or null)
        // and restore it on return, so a reentrant nested call nests and pops correctly.
        let previous = CURRENT_CTX.replace(callctx_ptr);
        let status = catch_unwind(AssertUnwindSafe(|| callback(callctx_ptr, user_data)))
            .unwrap_or(DoodleStatus::ErrPanic);
        // Restore even on a caught panic (`catch_unwind` returned normally): a `doodle_call_*` on
        // a stashed copy now fails the `CURRENT_CTX` equality check, without touching this frame.
        CURRENT_CTX.set(previous);
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

/// Borrows the [`DoodleCallCtx`] behind a raw pointer **only if it is the innermost active ctx**
/// ([`CURRENT_CTX`]): `ErrContract` otherwise (a returned ctx, or an ancestor whose block is
/// mid-drive). The check is a pure pointer comparison — the passed pointer is **not**
/// dereferenced unless it equals `CURRENT_CTX`, so a stale/ancestor pointer never reads freed or
/// aliased memory. Shared with the ctx value functions (`call_read`/`call_value`).
pub(crate) fn current_ctx<'a>(
    ctx: *mut DoodleCallCtx,
) -> Result<&'a mut DoodleCallCtx, DoodleStatus> {
    if ctx.is_null() {
        return Err(DoodleStatus::ErrNullPointer);
    }
    if CURRENT_CTX.get() != ctx {
        return Err(DoodleStatus::ErrContract);
    }
    // SAFETY: `ctx == CURRENT_CTX`, which the active trampoline set to its live on-stack `callctx`
    // (and has not yet restored), so `ctx` is valid and, being the innermost activation, is not
    // reborrowed into a deeper drive — this `&mut` is the only live one. Single-threaded (E§11).
    Ok(unsafe { &mut *ctx })
}

/// The engine activation behind a `DoodleCallCtx` (its `&mut IntrinsicCtx`). The returned lifetime
/// is unconstrained (an FFI raw deref); each caller uses it within one call. Sound only because
/// `cc` is the innermost ctx ([`current_ctx`] checked), so its `&mut IntrinsicCtx` is not
/// reborrowed into a deeper drive.
fn engine_ctx<'a>(cc: &mut DoodleCallCtx) -> &'a mut IntrinsicCtx<'a> {
    // SAFETY: `cc.ctx` is the `&mut IntrinsicCtx` the engine threaded into the trampoline. `cc` is
    // the innermost active ctx (`current_ctx` gated on `CURRENT_CTX`), so the engine's `apply`
    // handed it exclusive access and no deeper drive holds a reborrow of it — no other `&mut` to
    // this ctx is live. Block re-entry routes through `invoke_block_handles`, forming no
    // second `&mut Instance`.
    unsafe { &mut *(cc.ctx as *mut IntrinsicCtx<'a>) }
}

/// The engine activation behind the innermost `DoodleCallCtx`, or an error status — the
/// innermost-ctx check ([`current_ctx`]) then the engine deref ([`engine_ctx`]). Shared with the
/// ctx value functions ([`crate::call_read`]/[`crate::call_value`]).
pub(crate) fn engine_of<'a>(
    ctx: *mut DoodleCallCtx,
) -> Result<&'a mut IntrinsicCtx<'a>, DoodleStatus> {
    Ok(engine_ctx(current_ctx(ctx)?))
}

/// Writes the number of bound (non-block) arguments this call received (E§5.2) — read each with
/// [`doodle_call_arg`]. `ErrContract` unless `ctx` is the innermost active callback.
///
/// # Safety
/// `ctx` must be the callback's current `DoodleCallCtx`; `out` writable.
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
/// `ErrIndexOutOfBounds` past the last argument; `ErrContract` unless `ctx` is the innermost call.
///
/// # Safety
/// `ctx` must be the callback's current `DoodleCallCtx`; `out` writable.
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
/// [`IntrinsicCtx::invoke_block_handles`] (never a second `&mut Instance`). `ErrContract` unless
/// `ctx` is the innermost active callback.
///
/// # Safety
/// `ctx` must be the callback's current `DoodleCallCtx`; `args` points to `n` readable handles (or
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
/// read via `doodle_output`). `ErrContract` unless `ctx` is the innermost active callback.
///
/// # Safety
/// `ctx` must be the callback's current `DoodleCallCtx`; `bytes` points to `len` readable bytes (or
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
/// `ErrContract` unless `ctx` is the innermost active callback.
///
/// # Safety
/// `ctx` must be the callback's current `DoodleCallCtx`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_set_result(
    ctx: *mut DoodleCallCtx,
    handle: DoodleHandle,
) -> DoodleStatus {
    catch(|| match current_ctx(ctx) {
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
/// `doodle_call_set_result` does): do not release or reuse it. `ErrContract` unless `ctx` is
/// the innermost active callback.
///
/// # Safety
/// `ctx` must be the callback's current `DoodleCallCtx`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_set_raise(
    ctx: *mut DoodleCallCtx,
    handle: DoodleHandle,
) -> DoodleStatus {
    catch(|| match current_ctx(ctx) {
        Ok(cc) => {
            cc.reply = CallReply::Raise(handle);
            DoodleStatus::Ok
        }
        Err(status) => status,
    })
}
