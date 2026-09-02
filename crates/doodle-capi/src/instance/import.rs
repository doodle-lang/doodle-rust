//! The module-resolver surface (E§6): a program's `import` of a not-yet-loaded module
//! suspends with `SuspendedImport`; the host reads the requested dotted path and answers with
//! the module's source, "not found", or an exception to raise. Split from `instance.rs` (a
//! child module, so it reaches the parent's private `DoodleInstance` fields + drive helpers).

use super::{DoodleInstance, di_mut, di_ref, fill_outcome};
use crate::abi::{DoodleHandle, DoodleOutcome, DoodleStatus};
use crate::guard::catch;
use crate::value::copy_out;
use doodle_core::drive::{ImportResolution, resolve_import};
use doodle_core::machine::Handle;

/// Copies the `index`-th dotted path segment of a parked `import` request (E§6) into `buf`
/// (copy-out; `out_len` gets the full byte length). E.g. `import shapes.circle` exposes
/// `"shapes"` at index 0 and `"circle"` at index 1 (`DoodleOutcome::request_count` segments).
/// `ErrContract` if the instance is not suspended on an import; `ErrIndexOutOfBounds` past the
/// last segment.
///
/// # Safety
/// `instance` live; `buf` readable for `cap` (or NULL with `cap` 0); `out_len` may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_import_path_segment(
    instance: *const DoodleInstance,
    index: u32,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> DoodleStatus {
    catch(|| {
        let Some(di) = di_ref(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        match &di.pending_import {
            Some(path) => match path.get(index as usize) {
                Some(segment) => copy_out(segment.as_bytes(), buf, cap, out_len),
                None => DoodleStatus::ErrIndexOutOfBounds,
            },
            None => DoodleStatus::ErrContract,
        }
    })
}

/// Resolves a parked `import` (E§6) with the module's `source` (UTF-8) under the host's
/// `canonical_id` (its singleton-cache identity, L§11.3), then drives on — the module's top
/// level runs before the importer continues. `ErrContract` if not suspended on an import;
/// `ErrInvalidUtf8` if either buffer is not UTF-8.
///
/// # Safety
/// `instance` live; `source`/`canonical_id` point to their `*_len` readable bytes;
/// `out_outcome` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_resolve_import(
    instance: *mut DoodleInstance,
    source: *const u8,
    source_len: usize,
    canonical_id: *const u8,
    canonical_id_len: usize,
    out_outcome: *mut DoodleOutcome,
) -> DoodleStatus {
    catch(|| {
        let (Some(text), Some(id)) = (
            str_arg(source, source_len),
            str_arg(canonical_id, canonical_id_len),
        ) else {
            return DoodleStatus::ErrInvalidUtf8;
        };
        resolve_import_and_fill(
            instance,
            out_outcome,
            ImportResolution::Source {
                text,
                canonical_id: id,
            },
        )
    })
}

/// Resolves a parked `import` (E§6) as **not found**: the engine raises `module-not-found` in
/// the importer (a multi-segment path first falls back to a member import, S-7), then drives
/// on. `ErrContract` if not suspended on an import.
///
/// # Safety
/// `instance` live; `out_outcome` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_resolve_import_not_found(
    instance: *mut DoodleInstance,
    out_outcome: *mut DoodleOutcome,
) -> DoodleStatus {
    catch(|| resolve_import_and_fill(instance, out_outcome, ImportResolution::NotFound))
}

/// Resolves a parked `import` (E§6) by **raising** `value` at the `import` site (e.g. a failed
/// fetch), then drives on. `ErrContract` if not suspended on an import.
///
/// # Safety
/// `instance` live; `out_outcome` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_resolve_import_raise(
    instance: *mut DoodleInstance,
    value: DoodleHandle,
    out_outcome: *mut DoodleOutcome,
) -> DoodleStatus {
    catch(|| {
        resolve_import_and_fill(
            instance,
            out_outcome,
            ImportResolution::Raise(Handle::from_bits(value)),
        )
    })
}

/// Reads a `(ptr, len)` UTF-8 argument into an owned `String`, or `None` if NULL (with a
/// non-zero length) or not UTF-8. An empty request (`len == 0`) is the empty string.
fn str_arg(ptr: *const u8, len: usize) -> Option<String> {
    if len == 0 {
        return Some(String::new());
    }
    if ptr.is_null() {
        return None;
    }
    // SAFETY: `ptr` is non-null (checked) and points to `len` readable bytes by the caller's
    // `# Safety` contract; the slice is copied into an owned `String`, not held.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    std::str::from_utf8(bytes).ok().map(str::to_string)
}

/// Shared body of the `doodle_resolve_import*` functions: answer the parked import, then fill
/// the out-outcome. `ErrContract` if the instance is not suspended on an import.
fn resolve_import_and_fill(
    instance: *mut DoodleInstance,
    out_outcome: *mut DoodleOutcome,
    resolution: ImportResolution,
) -> DoodleStatus {
    let Some(di) = di_mut(instance) else {
        return DoodleStatus::ErrNullPointer;
    };
    if out_outcome.is_null() {
        return DoodleStatus::ErrNullPointer;
    }
    // `pending_import` is `Some` exactly while `SuspendedImport` (set by `fill_outcome`); a
    // defined `ErrContract` beats the engine's non-suspended-resolve fault.
    if di.pending_import.is_none() {
        return DoodleStatus::ErrContract;
    }
    let outcome = resolve_import(&mut di.inner, resolution);
    let filled = fill_outcome(di, outcome);
    // SAFETY: `out_outcome` is non-null (checked) and writable/aligned for a `DoodleOutcome`.
    unsafe { *out_outcome = filled };
    DoodleStatus::Ok
}
