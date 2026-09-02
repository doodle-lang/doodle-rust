//! The value/handle boundary (E§4): constructors (`doodle_make_*`), typed readers
//! (`doodle_as_*`, `doodle_kind_of`), and `doodle_release`. Constructors route through the
//! engine's canonicalizing/normalizing paths (`make_float` canonical-NaN, `make_string`
//! UTF-8-validated + NFC), so a host cannot inject a non-canonical value that would diverge
//! across surfaces (E§4.3/§11). Byte/string results **copy out** into a caller buffer
//! (freeze convention 4) — no interior pointer escapes the engine.

use crate::abi::{self, DoodleHandle, DoodleKind, DoodleStatus};
use crate::guard::catch;
use crate::instance::DoodleInstance;
use doodle_core::machine::{Handle, Instance, ValueError};

/// Copies `bytes` into the caller buffer `buf` of capacity `cap`, writing the full byte
/// length to `out_len` (which may be NULL). Returns `Ok` on a full copy, or
/// `ErrBufferTooSmall` (leaving `buf` untouched) when `cap < bytes.len()` — the host reads
/// `out_len`, resizes, and retries. This is the one copy-out primitive every string/byte
/// reader shares (freeze convention 4).
pub(crate) fn copy_out(
    bytes: &[u8],
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> DoodleStatus {
    // SAFETY: `out_len.as_mut()` returns None for a NULL `out_len`; a non-null `out_len` is a
    // writable, aligned `*usize` by the caller's `# Safety` contract.
    if let Some(slot) = unsafe { out_len.as_mut() } {
        *slot = bytes.len();
    }
    if bytes.len() > cap {
        return DoodleStatus::ErrBufferTooSmall;
    }
    if !bytes.is_empty() {
        // SAFETY: `buf` has room for `bytes.len()` (checked against `cap` above), and the
        // caller's contract makes `buf` non-null/writable whenever `cap > 0`; the copy is
        // within `[buf, buf+bytes.len())` and the regions do not overlap (distinct
        // allocations — engine bytes vs the host buffer).
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, bytes.len()) };
    }
    DoodleStatus::Ok
}

/// Borrows the engine instance behind a raw `DoodleInstance` pointer, or `None` if NULL.
fn instance_mut<'a>(p: *mut DoodleInstance) -> Option<&'a mut Instance> {
    // SAFETY: `as_mut` returns None for NULL; a non-null `p` is a live `DoodleInstance` from
    // `doodle_load` (not freed) by the caller's `# Safety` contract, so a `&mut` for `'a` is
    // sound (the host does not drive one instance from two threads at once — `!Sync`).
    unsafe { p.as_mut() }.map(|di| &mut di.inner)
}
fn instance_ref<'a>(p: *const DoodleInstance) -> Option<&'a Instance> {
    // SAFETY: as `instance_mut`, for a shared borrow.
    unsafe { p.as_ref() }.map(|di| &di.inner)
}

/// Writes `value` through the out-pointer `out`, returning false if `out` is NULL. The one
/// documented deref the scalar constructors/readers (and `doodle_capability_arg`) share.
pub(crate) fn write_out<T>(out: *mut T, value: T) -> bool {
    // SAFETY: `as_mut` returns None for NULL; a non-null `out` is writable and aligned for
    // `T` by the caller's `# Safety` contract.
    match unsafe { out.as_mut() } {
        Some(slot) => {
            *slot = value;
            true
        }
        None => false,
    }
}

/// Writes a made value's handle bits into the out-param, or reports NULL.
fn emit(handle: Handle, out: *mut DoodleHandle) -> DoodleStatus {
    if write_out(out, handle.bits()) {
        DoodleStatus::Ok
    } else {
        DoodleStatus::ErrNullPointer
    }
}

/// Makes an `Int` from an `int64_t` (E§4.3). Larger magnitudes use `doodle_make_int_decimal`.
///
/// # Safety
/// `instance` live; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_make_int(
    instance: *mut DoodleInstance,
    value: i64,
    out: *mut DoodleHandle,
) -> DoodleStatus {
    catch(|| match instance_mut(instance) {
        Some(inst) => emit(inst.make_int(value), out),
        None => DoodleStatus::ErrNullPointer,
    })
}

/// Makes an `Int` of any magnitude from a base-10 decimal string (E§4.3). `ErrMalformedInt`
/// if the text is not a base-10 integer literal.
///
/// # Safety
/// `instance` live; `decimal` points to `decimal_len` readable bytes; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_make_int_decimal(
    instance: *mut DoodleInstance,
    decimal: *const u8,
    decimal_len: usize,
    out: *mut DoodleHandle,
) -> DoodleStatus {
    catch(|| {
        let Some(inst) = instance_mut(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        if decimal.is_null() {
            return DoodleStatus::ErrNullPointer;
        }
        // SAFETY: `decimal` is non-null (checked) and points to `decimal_len` readable bytes
        // by the caller's `# Safety` contract; the slice is not held past this call.
        let bytes = unsafe { std::slice::from_raw_parts(decimal, decimal_len) };
        let Ok(text) = std::str::from_utf8(bytes) else {
            return DoodleStatus::ErrMalformedInt;
        };
        match inst.make_int_decimal(text) {
            Ok(handle) => emit(handle, out),
            Err(err) => abi::value_error(err),
        }
    })
}

/// Makes a `Float` from a `double` (E§4.3); any NaN is canonicalized (E§11).
///
/// # Safety
/// `instance` live; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_make_float(
    instance: *mut DoodleInstance,
    value: f64,
    out: *mut DoodleHandle,
) -> DoodleStatus {
    catch(|| match instance_mut(instance) {
        Some(inst) => emit(inst.make_float(value), out),
        None => DoodleStatus::ErrNullPointer,
    })
}

/// Makes a `Bool` (E§4.1).
///
/// # Safety
/// `instance` live; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_make_bool(
    instance: *mut DoodleInstance,
    value: bool,
    out: *mut DoodleHandle,
) -> DoodleStatus {
    catch(|| match instance_mut(instance) {
        Some(inst) => emit(inst.make_bool(value), out),
        None => DoodleStatus::ErrNullPointer,
    })
}

/// Makes `nil` (E§4.9).
///
/// # Safety
/// `instance` live; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_make_nil(
    instance: *mut DoodleInstance,
    out: *mut DoodleHandle,
) -> DoodleStatus {
    catch(|| match instance_mut(instance) {
        Some(inst) => emit(inst.make_nil(), out),
        None => DoodleStatus::ErrNullPointer,
    })
}

/// Makes a `String` from UTF-8 bytes (E§4.4); the engine validates UTF-8 and normalizes to
/// NFC. `ErrInvalidUtf8` if the bytes are not well-formed UTF-8.
///
/// # Safety
/// `instance` live; `bytes` points to `bytes_len` readable bytes; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_make_string(
    instance: *mut DoodleInstance,
    bytes: *const u8,
    bytes_len: usize,
    out: *mut DoodleHandle,
) -> DoodleStatus {
    catch(|| {
        let Some(inst) = instance_mut(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        if bytes.is_null() && bytes_len != 0 {
            return DoodleStatus::ErrNullPointer;
        }
        let slice = if bytes_len == 0 {
            &[][..]
        } else {
            // SAFETY: `bytes_len != 0` here and `bytes` was null-checked above, so by the
            // caller's `# Safety` contract it points to `bytes_len` readable bytes; the slice
            // is not held past this call.
            unsafe { std::slice::from_raw_parts(bytes, bytes_len) }
        };
        match inst.make_string(slice) {
            Ok(handle) => emit(handle, out),
            Err(err) => abi::value_error(err),
        }
    })
}

/// Reads an `Int` as `int64_t` (E§4.3). `ErrIntOutOfRange` for a bignum (read it with
/// `doodle_as_int_decimal`); `ErrWrongKind` for a non-int; `ErrStaleHandle` if freed.
///
/// # Safety
/// `instance` live; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_as_int(
    instance: *const DoodleInstance,
    handle: DoodleHandle,
    out: *mut i64,
) -> DoodleStatus {
    read(instance, |inst| inst.as_int(Handle::from_bits(handle)), out)
}

/// Reads a `Float` as `double` (E§4.3).
///
/// # Safety
/// `instance` live; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_as_float(
    instance: *const DoodleInstance,
    handle: DoodleHandle,
    out: *mut f64,
) -> DoodleStatus {
    read(
        instance,
        |inst| inst.as_float(Handle::from_bits(handle)),
        out,
    )
}

/// Reads a `Bool` (E§4.1).
///
/// # Safety
/// `instance` live; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_as_bool(
    instance: *const DoodleInstance,
    handle: DoodleHandle,
    out: *mut bool,
) -> DoodleStatus {
    read(
        instance,
        |inst| inst.as_bool(Handle::from_bits(handle)),
        out,
    )
}

/// Writes whether the value is `nil` (E§4.9) — `ErrStaleHandle` if freed, never `ErrWrongKind`.
///
/// # Safety
/// `instance` live; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_is_nil(
    instance: *const DoodleInstance,
    handle: DoodleHandle,
    out: *mut bool,
) -> DoodleStatus {
    read(instance, |inst| inst.is_nil(Handle::from_bits(handle)), out)
}

/// Writes the value's kind (E§4.4). `ErrStaleHandle` if the handle is freed.
///
/// # Safety
/// `instance` live; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_kind_of(
    instance: *const DoodleInstance,
    handle: DoodleHandle,
    out: *mut DoodleKind,
) -> DoodleStatus {
    catch(|| {
        let Some(inst) = instance_ref(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        match inst.kind_of(Handle::from_bits(handle)) {
            Ok(k) if write_out(out, abi::kind(k)) => DoodleStatus::Ok,
            Ok(_) => DoodleStatus::ErrNullPointer,
            Err(err) => abi::value_error(err),
        }
    })
}

/// Copies an `Int`'s value as a base-10 decimal string into `buf` (copy-out; `out_len` gets
/// the full length) — the arbitrary-precision counterpart of `doodle_as_int`.
///
/// # Safety
/// `instance` live; `buf` readable for `cap` (or NULL with `cap` 0); `out_len` may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_as_int_decimal(
    instance: *const DoodleInstance,
    handle: DoodleHandle,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> DoodleStatus {
    catch(|| {
        let Some(inst) = instance_ref(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        match inst.as_int_decimal(Handle::from_bits(handle)) {
            Ok(text) => copy_out(text.as_bytes(), buf, cap, out_len),
            Err(err) => abi::value_error(err),
        }
    })
}

/// Copies a `String`'s raw UTF-8 bytes into `buf` (copy-out; `out_len` gets the full byte
/// length). The engine keeps strings NFC, so these bytes are NFC UTF-8.
///
/// # Safety
/// `instance` live; `buf` readable for `cap` (or NULL with `cap` 0); `out_len` may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_string_bytes(
    instance: *const DoodleInstance,
    handle: DoodleHandle,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> DoodleStatus {
    catch(|| {
        let Some(inst) = instance_ref(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        match inst.string_bytes(Handle::from_bits(handle)) {
            Ok(bytes) => copy_out(bytes, buf, cap, out_len),
            Err(err) => abi::value_error(err),
        }
    })
}

/// Releases a host-owned handle (E§4.2): decrements its reference count, freeing the slot at
/// zero. `ErrStaleHandle` if already freed. A handle must be released exactly as many times
/// as it was obtained.
///
/// # Safety
/// `instance` must be a live pointer from `doodle_load`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_release(
    instance: *mut DoodleInstance,
    handle: DoodleHandle,
) -> DoodleStatus {
    catch(|| {
        let Some(inst) = instance_mut(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        match inst.release(Handle::from_bits(handle)) {
            Ok(()) => DoodleStatus::Ok,
            Err(_) => DoodleStatus::ErrStaleHandle,
        }
    })
}

/// Shared body of the scalar readers: run `reader` and write its value to `out`, mapping a
/// [`ValueError`] to its status.
fn read<T: Copy>(
    instance: *const DoodleInstance,
    reader: impl FnOnce(&Instance) -> Result<T, ValueError>,
    out: *mut T,
) -> DoodleStatus {
    catch(|| {
        let Some(inst) = instance_ref(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        match reader(inst) {
            Ok(value) if write_out(out, value) => DoodleStatus::Ok,
            Ok(_) => DoodleStatus::ErrNullPointer,
            Err(err) => abi::value_error(err),
        }
    })
}
