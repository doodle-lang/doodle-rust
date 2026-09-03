//! The frame-binding + module-global slice of the observation surface (E§8.2, M7.3b): a frame's
//! **local** bindings (parameters + `let`/`const` in scope) and its `with` **dynamic-parameter**
//! bindings, and a module's **globals** — each as a count + per-slot name (copy-out) + lazily
//! minted value handle, the pull model's cheap-names / lazy-value split. Split from the parent
//! `observe` (positions + stack walk) for length.
//!
//! All accessors are **pause-scoped** (D-M7-12 rider): each takes the `generation` from
//! `doodle_stack_frame_count` and rejects a stale one with `ErrStale`. Locals/dynamics are
//! addressed by (frame index, slot); globals by (module token, index) — `slot`/`index` directly
//! index the name list (slot order). A value accessor returns `DOODLE_NULL_HANDLE` for an
//! absent/uninitialized binding (the null handle is never a live handle), so a temporal-dead-zone
//! local reads cleanly as "unbound".

use super::{checked_mut, checked_ref, write_count, write_value};
use crate::abi::{self, DoodleGlobal, DoodleHandle, DoodlePosition, DoodleStatus};
use crate::guard::catch;
use crate::instance::DoodleInstance;
use crate::value::{copy_out, write_out};
use doodle_core::span::Span;

/// A `DoodlePosition` for `span` in the module named by `module_token`.
fn pos_in(span: Span, module_token: u32) -> DoodlePosition {
    DoodlePosition {
        span_start: span.start,
        span_end: span.end,
        module: module_token,
    }
}

/// Writes the number of local bindings in scope in frame `index` (E§8.2). `ErrStale` on a stale
/// `generation`; `ErrIndexOutOfBounds` if `index` is past the top frame.
///
/// # Safety
/// `instance` live; `out_count` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_frame_local_count(
    instance: *const DoodleInstance,
    generation: u32,
    index: u32,
    out_count: *mut u32,
) -> DoodleStatus {
    catch(|| {
        let di = match checked_ref(instance, generation) {
            Ok(di) => di,
            Err(status) => return status,
        };
        if index as usize >= di.inner.frame_count() {
            return DoodleStatus::ErrIndexOutOfBounds;
        }
        write_count(di.inner.frame_local_names(index as usize).len(), out_count)
    })
}

/// Copies the name of frame `index`'s `slot`-th local (E§8.2) into `buf` (copy-out). `ErrStale`
/// on a stale `generation`; `ErrIndexOutOfBounds` if `slot` is past the frame's locals.
///
/// # Safety
/// `instance` live; `buf` readable for `cap` (or NULL with `cap` 0); `out_len` may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_frame_local_name(
    instance: *const DoodleInstance,
    generation: u32,
    index: u32,
    slot: u32,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> DoodleStatus {
    catch(|| {
        let di = match checked_ref(instance, generation) {
            Ok(di) => di,
            Err(status) => return status,
        };
        let names = di.inner.frame_local_names(index as usize);
        match names.get(slot as usize) {
            Some(name) => copy_out(name.as_bytes(), buf, cap, out_len),
            None => DoodleStatus::ErrIndexOutOfBounds,
        }
    })
}

/// A fresh **host-owned** handle to frame `index`'s `slot`-th local's value (E§8.2), or
/// `DOODLE_NULL_HANDLE` if it is not yet initialized (the temporal dead zone) or the slot is
/// absent. `ErrStale` on a stale `generation`. Non-null values are host-owned — release them.
///
/// # Safety
/// `instance` live; `out_handle` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_frame_local_value(
    instance: *mut DoodleInstance,
    generation: u32,
    index: u32,
    slot: u32,
    out_handle: *mut DoodleHandle,
) -> DoodleStatus {
    catch(|| {
        if out_handle.is_null() {
            return DoodleStatus::ErrNullPointer;
        }
        let di = match checked_mut(instance, generation) {
            Ok(di) => di,
            Err(status) => return status,
        };
        write_value(
            di.inner.frame_local_value(index as usize, slot as usize),
            out_handle,
        )
    })
}

/// Writes the number of `with` dynamic-parameter bindings in frame `index` (E§8.2, L§5.5).
/// `ErrStale`/`ErrIndexOutOfBounds` as [`doodle_frame_local_count`].
///
/// # Safety
/// `instance` live; `out_count` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_frame_dynamic_count(
    instance: *const DoodleInstance,
    generation: u32,
    index: u32,
    out_count: *mut u32,
) -> DoodleStatus {
    catch(|| {
        let di = match checked_ref(instance, generation) {
            Ok(di) => di,
            Err(status) => return status,
        };
        if index as usize >= di.inner.frame_count() {
            return DoodleStatus::ErrIndexOutOfBounds;
        }
        write_count(
            di.inner.frame_dynamic_names(index as usize).len(),
            out_count,
        )
    })
}

/// Copies the name of frame `index`'s `slot`-th dynamic-parameter binding (E§8.2) into `buf`
/// (copy-out). `ErrStale`/`ErrIndexOutOfBounds` as [`doodle_frame_local_name`].
///
/// # Safety
/// `instance` live; `buf` readable for `cap` (or NULL with `cap` 0); `out_len` may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_frame_dynamic_name(
    instance: *const DoodleInstance,
    generation: u32,
    index: u32,
    slot: u32,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> DoodleStatus {
    catch(|| {
        let di = match checked_ref(instance, generation) {
            Ok(di) => di,
            Err(status) => return status,
        };
        let names = di.inner.frame_dynamic_names(index as usize);
        match names.get(slot as usize) {
            Some(name) => copy_out(name.as_bytes(), buf, cap, out_len),
            None => DoodleStatus::ErrIndexOutOfBounds,
        }
    })
}

/// A fresh **host-owned** handle to frame `index`'s `slot`-th dynamic-parameter value (E§8.2),
/// or `DOODLE_NULL_HANDLE` if the cell is unbound/absent. `ErrStale` on a stale `generation`.
///
/// # Safety
/// `instance` live; `out_handle` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_frame_dynamic_value(
    instance: *mut DoodleInstance,
    generation: u32,
    index: u32,
    slot: u32,
    out_handle: *mut DoodleHandle,
) -> DoodleStatus {
    catch(|| {
        if out_handle.is_null() {
            return DoodleStatus::ErrNullPointer;
        }
        let di = match checked_mut(instance, generation) {
            Ok(di) => di,
            Err(status) => return status,
        };
        write_value(
            di.inner.frame_dynamic_value(index as usize, slot as usize),
            out_handle,
        )
    })
}

/// Writes the number of module-level globals of the module named by `module_token` (E§8.2, L§5).
/// `ErrStale` on a stale `generation`. (Globals are module-scoped, but share the pause token so a
/// host re-walks after any drive — the values change with execution.)
///
/// # Safety
/// `instance` live; `out_count` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_module_global_count(
    instance: *const DoodleInstance,
    generation: u32,
    module_token: u32,
    out_count: *mut u32,
) -> DoodleStatus {
    catch(|| {
        let di = match checked_ref(instance, generation) {
            Ok(di) => di,
            Err(status) => return status,
        };
        write_count(
            di.inner.module_global_names(module_token as usize).len(),
            out_count,
        )
    })
}

/// Fills `out_global` with the `index`-th global of `module_token` (E§8.2): its `kind` and
/// declaration span. Its name copies out via [`doodle_module_global_name`] and its value via
/// [`doodle_module_global_value`] (same `index`). `ErrStale`/`ErrIndexOutOfBounds`.
///
/// # Safety
/// `instance` live; `out_global` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_module_global(
    instance: *const DoodleInstance,
    generation: u32,
    module_token: u32,
    index: u32,
    out_global: *mut DoodleGlobal,
) -> DoodleStatus {
    catch(|| {
        let di = match checked_ref(instance, generation) {
            Ok(di) => di,
            Err(status) => return status,
        };
        let globals = di.inner.module_global_names(module_token as usize);
        match globals.get(index as usize) {
            Some(global) => {
                let filled = DoodleGlobal {
                    kind: abi::global_kind(global.kind),
                    decl_span: pos_in(global.decl_span, module_token),
                    reserved: [0; 2],
                };
                if write_out(out_global, filled) {
                    DoodleStatus::Ok
                } else {
                    DoodleStatus::ErrNullPointer
                }
            }
            None => DoodleStatus::ErrIndexOutOfBounds,
        }
    })
}

/// Copies the name of `module_token`'s `index`-th global (E§8.2) into `buf` (copy-out).
/// `ErrStale`/`ErrIndexOutOfBounds`.
///
/// # Safety
/// `instance` live; `buf` readable for `cap` (or NULL with `cap` 0); `out_len` may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_module_global_name(
    instance: *const DoodleInstance,
    generation: u32,
    module_token: u32,
    index: u32,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> DoodleStatus {
    catch(|| {
        let di = match checked_ref(instance, generation) {
            Ok(di) => di,
            Err(status) => return status,
        };
        let globals = di.inner.module_global_names(module_token as usize);
        match globals.get(index as usize) {
            Some(global) => copy_out(global.name.as_bytes(), buf, cap, out_len),
            None => DoodleStatus::ErrIndexOutOfBounds,
        }
    })
}

/// A fresh **host-owned** handle to the current value of `module_token`'s `index`-th global
/// (E§8.2), or `DOODLE_NULL_HANDLE` if it is not yet defined (its declaration has not executed)
/// or the index is absent. `ErrStale` on a stale `generation`. Non-null values are host-owned.
///
/// # Safety
/// `instance` live; `out_handle` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_module_global_value(
    instance: *mut DoodleInstance,
    generation: u32,
    module_token: u32,
    index: u32,
    out_handle: *mut DoodleHandle,
) -> DoodleStatus {
    catch(|| {
        if out_handle.is_null() {
            return DoodleStatus::ErrNullPointer;
        }
        let di = match checked_mut(instance, generation) {
            Ok(di) => di,
            Err(status) => return status,
        };
        write_value(
            di.inner
                .module_global_value(module_token as usize, index as usize),
            out_handle,
        )
    })
}
