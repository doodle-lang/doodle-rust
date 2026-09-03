//! The C ABI observation/debug surface (engine spec E§8, M7.3): the **pull** view of a stopped
//! instance — current/completed source positions, the result-register value, and the call-stack
//! walk. Frames are addressed **innermost-0 index + a pause-generation token** (D-M7-12): the
//! walk returns the current count and generation, and every pause-scoped accessor rejects a
//! **stale** generation (`ErrStale` — benign, re-walk) so a debugger never resolves a frame index
//! against a stack a drive has since replaced. A frame's callable is minted **lazily**
//! (`doodle_frame_callable`, D-M7-13), so a host that only needs positions pays for no handles.
//! Positions carry an **opaque module token** resolved to a canonical id by
//! `doodle_module_canonical_id` (D-M7-14). Bindings/globals, inspection, breakpoints, aux eval,
//! and diagnostics are later M7.3 chunks.

use crate::abi::{
    self, DOODLE_NULL_HANDLE, DoodleFrame, DoodleHandle, DoodlePosition, DoodleStatus,
};
use crate::guard::catch;
use crate::instance::{DoodleInstance, di_mut, di_ref};
use crate::value::{copy_out, write_out};
use doodle_core::machine::Position;
use doodle_core::span::ModuleId;

/// Writes an optional [`Position`] to `out_pos` + `out_has` (a bare struct can't be `Option`):
/// `has = true` and the mapped position when `Some`, `has = false` and a zeroed position when
/// `None` (already terminal / not at a fine safe point).
fn write_position(
    pos: Option<Position>,
    out_pos: *mut DoodlePosition,
    out_has: *mut bool,
) -> DoodleStatus {
    let (has, mapped) = match pos {
        Some(p) => (true, abi::position(p)),
        None => (
            false,
            DoodlePosition {
                span_start: 0,
                span_end: 0,
                module: 0,
            },
        ),
    };
    if write_out(out_pos, mapped) && write_out(out_has, has) {
        DoodleStatus::Ok
    } else {
        DoodleStatus::ErrNullPointer
    }
}

/// The current source position (E§8.1): the span the active frame is about to execute. `has` is
/// `false` (position zeroed) when the instance is terminal (no active frame).
///
/// # Safety
/// `instance` live; `out_position`/`out_has` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_current_position(
    instance: *const DoodleInstance,
    out_position: *mut DoodlePosition,
    out_has: *mut bool,
) -> DoodleStatus {
    catch(|| match di_ref(instance) {
        Some(di) => write_position(di.inner.current_position(), out_position, out_has),
        None => DoodleStatus::ErrNullPointer,
    })
}

/// The position of the subexpression just completed at a **fine** safe point (E§8.4, S-62) —
/// together with `doodle_current_result`, the "watch your expression evaluate" primitive. `has`
/// is `false` at a statement stop or when not stopped at a fine point.
///
/// # Safety
/// `instance` live; `out_position`/`out_has` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_completed_position(
    instance: *const DoodleInstance,
    out_position: *mut DoodlePosition,
    out_has: *mut bool,
) -> DoodleStatus {
    catch(|| match di_ref(instance) {
        Some(di) => write_position(di.inner.completed_position(), out_position, out_has),
        None => DoodleStatus::ErrNullPointer,
    })
}

/// A fresh **host-owned** handle to the current result-register value (E§8.4), or
/// `DOODLE_NULL_HANDLE` when the register is empty (Void). At a fine safe point this is the
/// subexpression's value (S-62). Non-null results are host-owned — `doodle_release` them.
///
/// # Safety
/// `instance` live; `out_handle` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_current_result(
    instance: *mut DoodleInstance,
    out_handle: *mut DoodleHandle,
) -> DoodleStatus {
    catch(|| {
        let Some(di) = di_mut(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        let bits = di
            .inner
            .result_handle()
            .map_or(DOODLE_NULL_HANDLE, |h| h.bits());
        if write_out(out_handle, bits) {
            DoodleStatus::Ok
        } else {
            DoodleStatus::ErrNullPointer
        }
    })
}

/// Writes the number of live stack frames (E§8.2) to `out_count` and the current **pause
/// generation** to `out_generation` — the token every per-frame accessor validates (D-M7-12).
/// Frames are addressed innermost-0. Re-read (and re-walk) after any drive/resolve.
///
/// # Safety
/// `instance` live; `out_count`/`out_generation` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_stack_frame_count(
    instance: *const DoodleInstance,
    out_count: *mut u32,
    out_generation: *mut u32,
) -> DoodleStatus {
    catch(|| {
        let Some(di) = di_ref(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        let count = u32::try_from(di.inner.frame_count()).unwrap_or(u32::MAX);
        if write_out(out_count, count) && write_out(out_generation, di.generation) {
            DoodleStatus::Ok
        } else {
            DoodleStatus::ErrNullPointer
        }
    })
}

/// Fills `out_frame` with innermost-first frame `index` (E§8.2), pure data (the callable is minted
/// separately by `doodle_frame_callable`). `generation` must be the token from
/// `doodle_stack_frame_count`; a stale one → `ErrStale` (checked **before** bounds, D-M7-12), a
/// live one with `index` past the top → `ErrIndexOutOfBounds`.
///
/// # Safety
/// `instance` live; `out_frame` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_frame_at(
    instance: *const DoodleInstance,
    generation: u32,
    index: u32,
    out_frame: *mut DoodleFrame,
) -> DoodleStatus {
    catch(|| {
        let Some(di) = di_ref(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        if generation != di.generation {
            return DoodleStatus::ErrStale;
        }
        let Some(info) = di.inner.frame_info(index as usize) else {
            return DoodleStatus::ErrIndexOutOfBounds;
        };
        let frame = DoodleFrame {
            has_callable: info.has_callable,
            has_call_site: info.call_site.is_some(),
            call_site: info.call_site.map_or(
                DoodlePosition {
                    span_start: 0,
                    span_end: 0,
                    module: 0,
                },
                |span| DoodlePosition {
                    span_start: span.start,
                    span_end: span.end,
                    module: info.module.0,
                },
            ),
            tail_count: info.tail_count,
            module: info.module.0,
            reserved: [0; 4],
        };
        if write_out(out_frame, frame) {
            DoodleStatus::Ok
        } else {
            DoodleStatus::ErrNullPointer
        }
    })
}

/// A fresh **host-owned** handle to frame `index`'s callable (E§8.2, D-M7-13), or
/// `DOODLE_NULL_HANDLE` for a module-top / block frame (no callable — `has_callable` said so).
/// `generation` gates the `index` addressing (stale → `ErrStale`), **not** the returned handle:
/// once minted it is an ordinary host-owned handle (valid across resumes) — `doodle_release` it.
///
/// # Safety
/// `instance` live; `out_handle` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_frame_callable(
    instance: *mut DoodleInstance,
    generation: u32,
    index: u32,
    out_handle: *mut DoodleHandle,
) -> DoodleStatus {
    catch(|| {
        let Some(di) = di_mut(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        if generation != di.generation {
            return DoodleStatus::ErrStale;
        }
        if index as usize >= di.inner.frame_count() {
            return DoodleStatus::ErrIndexOutOfBounds;
        }
        let bits = di
            .inner
            .frame_callable(index as usize)
            .map_or(DOODLE_NULL_HANDLE, |h| h.bits());
        if write_out(out_handle, bits) {
            DoodleStatus::Ok
        } else {
            DoodleStatus::ErrNullPointer
        }
    })
}

/// Copies the host **canonical id** a position's opaque module `token` was loaded under (E§6,
/// D-M7-14) into `buf` (copy-out; `out_len` gets the full byte length) — the host resolves a
/// `DoodlePosition::module` to the source it holds. `ErrContract` if `token` names no loaded
/// module (a fabricated token; tokens come from positions/frames).
///
/// # Safety
/// `instance` live; `buf` readable for `cap` (or NULL with `cap` 0); `out_len` may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_module_canonical_id(
    instance: *const DoodleInstance,
    token: u32,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> DoodleStatus {
    catch(|| {
        let Some(di) = di_ref(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        match di.inner.module_canonical_id(ModuleId(token)) {
            Some(canonical) => copy_out(canonical.as_bytes(), buf, cap, out_len),
            None => DoodleStatus::ErrContract,
        }
    })
}

/// The frame-binding + module-global accessors (M7.3b), split out for length.
mod bindings;
pub use bindings::*;
