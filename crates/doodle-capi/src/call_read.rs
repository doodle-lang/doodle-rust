//! The value **readers** and `release` inside a foreign callback (E§4.3/§4.4/§4.5, M7.2b): the
//! `doodle_call_as_*`/`doodle_call_kind_of`/`doodle_call_release` a host uses to inspect its
//! argument handles and free the handles it minted — the counterparts of the instance-based
//! `doodle_as_*`, routed through the callback's [`DoodleCallCtx`]. Split from `call_value.rs` (the
//! constructors) for length; shares its `engine_of` innermost-ctx gate.

use crate::abi::{self, DoodleHandle, DoodleKind, DoodleStatus};
use crate::call::{DoodleCallCtx, engine_of};
use crate::guard::catch;
use crate::value::{copy_out, write_out};
use doodle_core::machine::{Handle, IntrinsicCtx, ValueError};

/// Reads a scalar from the callback's engine ctx and writes it to `out`, mapping a
/// [`ValueError`] to its status. Shared by the scalar readers.
fn read<T: Copy>(
    ctx: *mut DoodleCallCtx,
    reader: impl FnOnce(&IntrinsicCtx) -> Result<T, ValueError>,
    out: *mut T,
) -> DoodleStatus {
    catch(|| {
        let engine = match engine_of(ctx) {
            Ok(engine) => engine,
            Err(status) => return status,
        };
        match reader(engine) {
            Ok(value) if write_out(out, value) => DoodleStatus::Ok,
            Ok(_) => DoodleStatus::ErrNullPointer,
            Err(err) => abi::value_error(err),
        }
    })
}

/// Writes the value's kind (E§4.4). `ErrStaleHandle` if the handle is freed.
///
/// # Safety
/// `ctx` live; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_kind_of(
    ctx: *mut DoodleCallCtx,
    handle: DoodleHandle,
    out: *mut DoodleKind,
) -> DoodleStatus {
    catch(|| {
        let engine = match engine_of(ctx) {
            Ok(engine) => engine,
            Err(status) => return status,
        };
        match engine.kind_of(Handle::from_bits(handle)) {
            Ok(k) if write_out(out, abi::kind(k)) => DoodleStatus::Ok,
            Ok(_) => DoodleStatus::ErrNullPointer,
            Err(err) => abi::value_error(err),
        }
    })
}

/// Reads an `Int` as `int64_t` (E§4.3). `ErrIntOutOfRange` for a bignum; `ErrWrongKind` otherwise.
///
/// # Safety
/// `ctx` live; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_as_int(
    ctx: *mut DoodleCallCtx,
    handle: DoodleHandle,
    out: *mut i64,
) -> DoodleStatus {
    read(ctx, |engine| engine.as_int(Handle::from_bits(handle)), out)
}

/// Reads a `Float` as `double` (E§4.3).
///
/// # Safety
/// `ctx` live; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_as_float(
    ctx: *mut DoodleCallCtx,
    handle: DoodleHandle,
    out: *mut f64,
) -> DoodleStatus {
    read(
        ctx,
        |engine| engine.as_float(Handle::from_bits(handle)),
        out,
    )
}

/// Reads a `Bool` (E§4.1).
///
/// # Safety
/// `ctx` live; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_as_bool(
    ctx: *mut DoodleCallCtx,
    handle: DoodleHandle,
    out: *mut bool,
) -> DoodleStatus {
    read(ctx, |engine| engine.as_bool(Handle::from_bits(handle)), out)
}

/// Writes whether the value is `nil` (E§4.9) — `ErrStaleHandle` if freed, never `ErrWrongKind`.
///
/// # Safety
/// `ctx` live; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_is_nil(
    ctx: *mut DoodleCallCtx,
    handle: DoodleHandle,
    out: *mut bool,
) -> DoodleStatus {
    read(ctx, |engine| engine.is_nil(Handle::from_bits(handle)), out)
}

/// Writes the number of elements in a list (E§4.6). `ErrWrongKind` if not a list.
///
/// # Safety
/// `ctx` live; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_list_length(
    ctx: *mut DoodleCallCtx,
    handle: DoodleHandle,
    out: *mut usize,
) -> DoodleStatus {
    read(
        ctx,
        |engine| engine.list_length(Handle::from_bits(handle)),
        out,
    )
}

/// Writes a foreign value's host `tag` (E§4.5). `ErrWrongKind` if not a foreign value.
///
/// # Safety
/// `ctx` live; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_foreign_tag(
    ctx: *mut DoodleCallCtx,
    handle: DoodleHandle,
    out: *mut u64,
) -> DoodleStatus {
    read(
        ctx,
        |engine| engine.foreign_tag(Handle::from_bits(handle)),
        out,
    )
}

/// Writes a foreign value's opaque host `ptr` (E§4.5), verbatim. `ErrWrongKind` if not a foreign.
///
/// # Safety
/// `ctx` live; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_foreign_ptr(
    ctx: *mut DoodleCallCtx,
    handle: DoodleHandle,
    out: *mut u64,
) -> DoodleStatus {
    read(
        ctx,
        |engine| engine.foreign_ptr(Handle::from_bits(handle)),
        out,
    )
}

/// Copies an `Int`'s value as base-10 decimal into `buf` (copy-out; `out_len` gets the full
/// length) — the arbitrary-precision counterpart of [`doodle_call_as_int`].
///
/// # Safety
/// `ctx` live; `buf` readable for `cap` (or NULL with `cap` 0); `out_len` may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_as_int_decimal(
    ctx: *mut DoodleCallCtx,
    handle: DoodleHandle,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> DoodleStatus {
    catch(|| {
        let engine = match engine_of(ctx) {
            Ok(engine) => engine,
            Err(status) => return status,
        };
        match engine.as_int_decimal(Handle::from_bits(handle)) {
            Ok(text) => copy_out(text.as_bytes(), buf, cap, out_len),
            Err(err) => abi::value_error(err),
        }
    })
}

/// Copies a `String`'s NFC UTF-8 bytes into `buf` (copy-out; `out_len` gets the full byte length).
///
/// # Safety
/// `ctx` live; `buf` readable for `cap` (or NULL with `cap` 0); `out_len` may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_string_bytes(
    ctx: *mut DoodleCallCtx,
    handle: DoodleHandle,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> DoodleStatus {
    catch(|| {
        let engine = match engine_of(ctx) {
            Ok(engine) => engine,
            Err(status) => return status,
        };
        match engine.string_bytes(Handle::from_bits(handle)) {
            Ok(bytes) => copy_out(bytes, buf, cap, out_len),
            Err(err) => abi::value_error(err),
        }
    })
}

/// Copies a byte string's raw bytes into `buf` (copy-out; `out_len` gets the full length).
///
/// # Safety
/// `ctx` live; `buf` readable for `cap` (or NULL with `cap` 0); `out_len` may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_as_bytes(
    ctx: *mut DoodleCallCtx,
    handle: DoodleHandle,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> DoodleStatus {
    catch(|| {
        let engine = match engine_of(ctx) {
            Ok(engine) => engine,
            Err(status) => return status,
        };
        match engine.as_bytes(Handle::from_bits(handle)) {
            Ok(bytes) => copy_out(bytes, buf, cap, out_len),
            Err(err) => abi::value_error(err),
        }
    })
}

/// Releases a handle the callback obtained (E§4.2): an arg handle from `doodle_call_arg` or a
/// value it constructed. `ErrStaleHandle` if already freed. Release each exactly once.
///
/// # Safety
/// `ctx` the callback's current `DoodleCallCtx`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_call_release(
    ctx: *mut DoodleCallCtx,
    handle: DoodleHandle,
) -> DoodleStatus {
    catch(|| {
        let engine = match engine_of(ctx) {
            Ok(engine) => engine,
            Err(status) => return status,
        };
        match engine.release(Handle::from_bits(handle)) {
            Ok(()) => DoodleStatus::Ok,
            Err(_) => DoodleStatus::ErrStaleHandle,
        }
    })
}
