//! The value **constructors** inside a foreign callback (E§4.3/§4.4/§4.5, M7.2b): the
//! `doodle_call_make_*`/`doodle_call_list_*` a host uses to build a `fn` result or a value to
//! raise — the counterparts of the instance-based `doodle_make_*`, routed through the callback's
//! [`DoodleCallCtx`] instead of a `DoodleInstance` (so no second `&mut Instance`). Each
//! `make_*`/`list_get` returns a **host-owned** handle the callback frees with
//! [`doodle_call_release`](crate::call_read::doodle_call_release). The readers + `release` are in
//! [`call_read`](crate::call_read); both split from `call.rs` and share its `engine_of` gate.

use crate::abi::{self, DoodleFinalizer, DoodleHandle, DoodleStatus};
use crate::call::{DoodleCallCtx, engine_of};
use crate::guard::catch;
use crate::value::write_out;
use doodle_core::machine::{Finalizer, Handle, IntrinsicCtx, ValueError};

/// Constructs a value on the callback's engine ctx and writes its handle bits to `out`.
fn make(
    ctx: *mut DoodleCallCtx,
    maker: impl FnOnce(&mut IntrinsicCtx) -> Handle,
    out: *mut DoodleHandle,
) -> DoodleStatus {
    catch(|| match engine_of(ctx) {
        Ok(engine) => emit(maker(engine), out),
        Err(status) => status,
    })
}

/// Like [`make`] but for a constructor that can fail (`make_string`/`make_int_decimal`/`list_get`).
fn make_fallible(
    ctx: *mut DoodleCallCtx,
    maker: impl FnOnce(&mut IntrinsicCtx) -> Result<Handle, ValueError>,
    out: *mut DoodleHandle,
) -> DoodleStatus {
    catch(|| {
        let engine = match engine_of(ctx) {
            Ok(engine) => engine,
            Err(status) => return status,
        };
        match maker(engine) {
            Ok(handle) => emit(handle, out),
            Err(err) => abi::value_error(err),
        }
    })
}

/// Writes a made value's handle bits to `out`, or reports NULL.
fn emit(handle: Handle, out: *mut DoodleHandle) -> DoodleStatus {
    if write_out(out, handle.bits()) {
        DoodleStatus::Ok
    } else {
        DoodleStatus::ErrNullPointer
    }
}

/// Reads a `(ptr, len)` UTF-8 argument into an owned `String`, or `None` if not UTF-8 (or NULL
/// with a non-zero length). `len == 0` is the empty string.
fn utf8_arg(ptr: *const u8, len: usize) -> Option<String> {
    if len == 0 {
        return Some(String::new());
    }
    if ptr.is_null() {
        return None;
    }
    // SAFETY: `len != 0` and `ptr` is non-null (checked), so by the caller's contract it points
    // to `len` readable bytes; the slice is copied, not held.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    std::str::from_utf8(bytes).ok().map(str::to_string)
}

/// Reads a `(ptr, len)` byte argument as a borrowed slice, or `None` if NULL with a non-zero
/// length. `len == 0` is the empty slice.
fn bytes_arg<'a>(ptr: *const u8, len: usize) -> Option<&'a [u8]> {
    if len == 0 {
        return Some(&[]);
    }
    if ptr.is_null() {
        return None;
    }
    // SAFETY: `len != 0` and `ptr` is non-null (checked), so by the caller's contract it points
    // to `len` readable bytes; the slice's use does not outlive the call.
    Some(unsafe { std::slice::from_raw_parts(ptr, len) })
}

// ---- constructors --------------------------------------------------------------------------

/// Constructs an integer (E§4.3) on the callback's ctx.
///
/// # Safety
/// `ctx` the callback's current `DoodleCallCtx`; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_make_int(
    ctx: *mut DoodleCallCtx,
    value: i64,
    out: *mut DoodleHandle,
) -> DoodleStatus {
    make(ctx, |engine| engine.make_int(value), out)
}

/// Constructs an integer of any magnitude from base-10 `decimal` (E§4.3). `ErrMalformedInt` if
/// the text is not a base-10 integer literal.
///
/// # Safety
/// `ctx` live; `decimal` points to `decimal_len` readable bytes; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_make_int_decimal(
    ctx: *mut DoodleCallCtx,
    decimal: *const u8,
    decimal_len: usize,
    out: *mut DoodleHandle,
) -> DoodleStatus {
    let Some(text) = utf8_arg(decimal, decimal_len) else {
        return DoodleStatus::ErrMalformedInt;
    };
    make_fallible(ctx, |engine| engine.make_int_decimal(&text), out)
}

/// Constructs a float (E§4.3); any NaN is canonicalized (E§11).
///
/// # Safety
/// `ctx` live; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_make_float(
    ctx: *mut DoodleCallCtx,
    value: f64,
    out: *mut DoodleHandle,
) -> DoodleStatus {
    make(ctx, |engine| engine.make_float(value), out)
}

/// Constructs a boolean (E§4.1).
///
/// # Safety
/// `ctx` live; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_make_bool(
    ctx: *mut DoodleCallCtx,
    value: bool,
    out: *mut DoodleHandle,
) -> DoodleStatus {
    make(ctx, |engine| engine.make_bool(value), out)
}

/// Constructs `nil` (E§4.9).
///
/// # Safety
/// `ctx` live; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_make_nil(
    ctx: *mut DoodleCallCtx,
    out: *mut DoodleHandle,
) -> DoodleStatus {
    make(ctx, |engine| engine.make_nil(), out)
}

/// Constructs a string from UTF-8 `bytes` (E§4.4): validated + NFC-normalized. `ErrInvalidUtf8`
/// if the bytes are not well-formed UTF-8.
///
/// # Safety
/// `ctx` live; `bytes` points to `bytes_len` readable bytes (or NULL with 0); `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_make_string(
    ctx: *mut DoodleCallCtx,
    bytes: *const u8,
    bytes_len: usize,
    out: *mut DoodleHandle,
) -> DoodleStatus {
    let Some(slice) = bytes_arg(bytes, bytes_len) else {
        return DoodleStatus::ErrNullPointer;
    };
    make_fallible(ctx, |engine| engine.make_string(slice), out)
}

/// Constructs a byte string (E§4.5): raw bytes, no encoding or normalization.
///
/// # Safety
/// `ctx` live; `bytes` points to `bytes_len` readable bytes (or NULL with 0); `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_make_bytes(
    ctx: *mut DoodleCallCtx,
    bytes: *const u8,
    bytes_len: usize,
    out: *mut DoodleHandle,
) -> DoodleStatus {
    let Some(slice) = bytes_arg(bytes, bytes_len) else {
        return DoodleStatus::ErrNullPointer;
    };
    make(ctx, |engine| engine.make_bytes(slice), out)
}

/// Constructs an empty list (E§4.6); grow it with [`doodle_call_list_append`].
///
/// # Safety
/// `ctx` live; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_make_list(
    ctx: *mut DoodleCallCtx,
    out: *mut DoodleHandle,
) -> DoodleStatus {
    make(ctx, |engine| engine.make_list(), out)
}

/// Appends the value `value` to the list `list` (E§4.6). `ErrWrongKind` if `list` is not a list.
///
/// # Safety
/// `ctx` the callback's current `DoodleCallCtx`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_list_append(
    ctx: *mut DoodleCallCtx,
    list: DoodleHandle,
    value: DoodleHandle,
) -> DoodleStatus {
    catch(|| {
        let engine = match engine_of(ctx) {
            Ok(engine) => engine,
            Err(status) => return status,
        };
        match engine.list_append(Handle::from_bits(list), Handle::from_bits(value)) {
            Ok(()) => DoodleStatus::Ok,
            Err(err) => abi::value_error(err),
        }
    })
}

/// Constructs a foreign (host) value (E§4.5): an opaque `tag`/`ptr` with an exactly-once
/// `finalizer` (which receives only `ptr` and must not unwind).
///
/// # Safety
/// `ctx` live; `out` writable; `finalizer` (if non-NULL) a valid function pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_make_foreign(
    ctx: *mut DoodleCallCtx,
    tag: u64,
    ptr: u64,
    finalizer: Option<DoodleFinalizer>,
    out: *mut DoodleHandle,
) -> DoodleStatus {
    // Trampoline the C finalizer into the engine's `Finalizer` (captures only the fn pointer +
    // is handed the `ptr`, so it is `Send` and cannot reach the instance).
    let finalizer: Option<Finalizer> =
        finalizer.map(|f| Box::new(move |ptr: u64| f(ptr)) as Finalizer);
    catch(|| match engine_of(ctx) {
        Ok(engine) => emit(engine.make_foreign(tag, ptr, finalizer), out),
        Err(status) => status,
    })
}

/// A fresh **host-owned** handle to the element at `index` of a list (E§4.6). `ErrWrongKind` if
/// not a list; `ErrIndexOutOfBounds` past the end.
///
/// # Safety
/// `ctx` live; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_list_get(
    ctx: *mut DoodleCallCtx,
    list: DoodleHandle,
    index: usize,
    out: *mut DoodleHandle,
) -> DoodleStatus {
    make_fallible(
        ctx,
        |engine| engine.list_get(Handle::from_bits(list), index),
        out,
    )
}
