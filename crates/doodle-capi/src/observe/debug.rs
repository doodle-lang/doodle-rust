//! The debug-setup slice of the observation surface (engine spec E§8, M7.3d): breakpoints
//! (E§8.6), the raise-trap (E§8.7), a host **pause** request (E§8.8), the runtime observation
//! mode (E§8.8), the bounded **tail-elided history** (E§8.3), and the **load-diagnostics** pull
//! record (E§3.2/§8, S-63, D-M7-15). Split from the parent `observe` for length.
//!
//! Breakpoints, raise-trap, pause, mode, and diagnostics are instance-level (not frame-indexed),
//! so they are **not** pause-scoped. Only the tail-elided history is pause-scoped (a stack-history
//! read, D-M7-12): its accessors take the `generation` from `doodle_stack_frame_count`.

use super::{checked_mut, checked_ref, write_count, write_value};
use crate::abi::{
    self, DoodleBreakpoint, DoodleDiagnostic, DoodleHandle, DoodleObservationMode, DoodlePosition,
    DoodleStatus,
};
use crate::guard::catch;
use crate::instance::{DoodleInstance, di_mut, di_ref};
use crate::value::{copy_out, write_out};
use doodle_core::drive::BreakpointId;

/// Reads a `(ptr, len)` UTF-8 argument as a borrowed `&str`, or `None` if not UTF-8 (or NULL with
/// a non-zero length). `len == 0` is the empty string.
fn str_arg<'a>(ptr: *const u8, len: usize) -> Option<&'a str> {
    if len == 0 {
        return Some("");
    }
    if ptr.is_null() {
        return None;
    }
    // SAFETY: `len != 0` and `ptr` non-null (checked); by the caller's contract it points to `len`
    // readable bytes; the slice is not held past the call that validates it.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    std::str::from_utf8(bytes).ok()
}

/// Writes an optional source position to `out_position` + `out_has` (a bare struct can't be
/// `Option`).
fn write_optional_position(
    pos: Option<doodle_core::machine::Position>,
    out_position: *mut DoodlePosition,
    out_has: *mut bool,
) -> DoodleStatus {
    match pos {
        Some(p) => {
            if write_out(out_position, abi::position(p)) && write_out(out_has, true) {
                DoodleStatus::Ok
            } else {
                DoodleStatus::ErrNullPointer
            }
        }
        None => {
            if write_out(out_has, false) {
                DoodleStatus::Ok
            } else {
                DoodleStatus::ErrNullPointer
            }
        }
    }
}

// ---- breakpoints ----------------------------------------------------------------------------

/// Sets a breakpoint at (`canonical_id`, `line`) (E§8.6, 1-based line) and writes its id to
/// `out_id`. It resolves if the canonical module is loaded; an unresolved breakpoint re-resolves
/// when that module loads. `ErrInvalidUtf8` if `canonical_id` is not UTF-8.
///
/// # Safety
/// `instance` live; `canonical_id` points to `canonical_len` readable bytes (or NULL with 0);
/// `out_id` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_set_breakpoint(
    instance: *mut DoodleInstance,
    canonical_id: *const u8,
    canonical_len: usize,
    line: u32,
    out_id: *mut u32,
) -> DoodleStatus {
    catch(|| {
        let Some(di) = di_mut(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        let Some(canonical) = str_arg(canonical_id, canonical_len) else {
            return DoodleStatus::ErrInvalidUtf8;
        };
        let id = di.inner.set_breakpoint(canonical, line);
        if write_out(out_id, id.0) {
            DoodleStatus::Ok
        } else {
            DoodleStatus::ErrNullPointer
        }
    })
}

/// Clears the breakpoint with `id` (E§8.6). Clearing an unknown id is a no-op.
///
/// # Safety
/// `instance` live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_clear_breakpoint(
    instance: *mut DoodleInstance,
    id: u32,
) -> DoodleStatus {
    catch(|| match di_mut(instance) {
        Some(di) => {
            di.inner.clear_breakpoint(BreakpointId(id));
            DoodleStatus::Ok
        }
        None => DoodleStatus::ErrNullPointer,
    })
}

/// Writes the number of set breakpoints (E§8.6).
///
/// # Safety
/// `instance` live; `out_count` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_breakpoint_count(
    instance: *const DoodleInstance,
    out_count: *mut u32,
) -> DoodleStatus {
    catch(|| match di_ref(instance) {
        Some(di) => write_count(di.inner.breakpoints().len(), out_count),
        None => DoodleStatus::ErrNullPointer,
    })
}

/// Fills `out_breakpoint` with the `index`-th breakpoint (E§8.6): its id, line, and resolved flag.
/// Its canonical id copies out via [`doodle_breakpoint_canonical_id`]. `ErrIndexOutOfBounds` past
/// the last breakpoint.
///
/// # Safety
/// `instance` live; `out_breakpoint` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_breakpoint_at(
    instance: *const DoodleInstance,
    index: u32,
    out_breakpoint: *mut DoodleBreakpoint,
) -> DoodleStatus {
    catch(|| {
        let Some(di) = di_ref(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        let breakpoints = di.inner.breakpoints();
        match breakpoints.get(index as usize) {
            Some(info) => {
                let filled = DoodleBreakpoint {
                    id: info.id.0,
                    line: info.line,
                    resolved: info.resolved,
                    reserved: [0; 2],
                };
                if write_out(out_breakpoint, filled) {
                    DoodleStatus::Ok
                } else {
                    DoodleStatus::ErrNullPointer
                }
            }
            None => DoodleStatus::ErrIndexOutOfBounds,
        }
    })
}

/// Copies the `index`-th breakpoint's canonical module id (E§8.6) into `buf` (copy-out).
/// `ErrIndexOutOfBounds` past the last breakpoint.
///
/// # Safety
/// `instance` live; `buf` readable for `cap` (or NULL with `cap` 0); `out_len` may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_breakpoint_canonical_id(
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
        let breakpoints = di.inner.breakpoints();
        match breakpoints.get(index as usize) {
            Some(info) => copy_out(info.canonical_id.as_bytes(), buf, cap, out_len),
            None => DoodleStatus::ErrIndexOutOfBounds,
        }
    })
}

// ---- raise-trap -----------------------------------------------------------------------------

/// Enables or disables the raise-trap (E§8.7): when on, a drive under `Continue` pauses
/// `Paused(RaiseTrap)` at each armed raise before it propagates.
///
/// # Safety
/// `instance` live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_set_raise_trapping(
    instance: *mut DoodleInstance,
    enabled: bool,
) -> DoodleStatus {
    catch(|| match di_mut(instance) {
        Some(di) => {
            di.inner.set_raise_trapping(enabled);
            DoodleStatus::Ok
        }
        None => DoodleStatus::ErrNullPointer,
    })
}

/// Writes whether the raise-trap is enabled (E§8.7).
///
/// # Safety
/// `instance` live; `out_enabled` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_raise_trapping(
    instance: *const DoodleInstance,
    out_enabled: *mut bool,
) -> DoodleStatus {
    catch(|| match di_ref(instance) {
        Some(di) if write_out(out_enabled, di.inner.raise_trapping()) => DoodleStatus::Ok,
        Some(_) => DoodleStatus::ErrNullPointer,
        None => DoodleStatus::ErrNullPointer,
    })
}

/// A fresh **host-owned** handle to the value of the currently trapped raise (E§8.7), or
/// `DOODLE_NULL_HANDLE` if not paused on a raise-trap. A non-null value is host-owned.
///
/// # Safety
/// `instance` live; `out_handle` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_trapped_raise(
    instance: *mut DoodleInstance,
    out_handle: *mut DoodleHandle,
) -> DoodleStatus {
    catch(|| {
        // Validate the out-param before minting, so a NULL out never orphans the trapped value.
        if out_handle.is_null() {
            return DoodleStatus::ErrNullPointer;
        }
        match di_mut(instance) {
            Some(di) => write_value(di.inner.trapped_raise(), out_handle),
            None => DoodleStatus::ErrNullPointer,
        }
    })
}

/// Writes the position of the currently trapped raise (E§8.7) to `out_position` + `out_has`;
/// `out_has` is `false` if not paused on a raise-trap.
///
/// # Safety
/// `instance` live; `out_position`/`out_has` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_trapped_raise_position(
    instance: *const DoodleInstance,
    out_position: *mut DoodlePosition,
    out_has: *mut bool,
) -> DoodleStatus {
    catch(|| match di_ref(instance) {
        Some(di) => {
            write_optional_position(di.inner.trapped_raise_position(), out_position, out_has)
        }
        None => DoodleStatus::ErrNullPointer,
    })
}

// ---- host pause + observation mode ----------------------------------------------------------

/// Requests a pause (E§8.8): the drive stops `Paused(HostPause)` at its next safe point.
/// Idempotent; callable from another thread while a drive runs (the flag is atomic, like
/// `doodle_cancel`). No-op on NULL.
///
/// # Safety
/// `instance` must be a live pointer from `doodle_load` (or NULL).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_pause(instance: *const DoodleInstance) {
    if let Some(di) = di_ref(instance) {
        di.inner.pause_token().pause();
    }
}

/// Sets the observation-mode granularity at runtime (E§8.8, S-62), between drives — the
/// runtime counterpart of `doodle_config_set_observation_mode`.
///
/// # Safety
/// `instance` live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_set_observation_mode(
    instance: *mut DoodleInstance,
    mode: DoodleObservationMode,
) -> DoodleStatus {
    catch(|| match di_mut(instance) {
        Some(di) => {
            di.inner.set_observation_mode(abi::observation_mode(mode));
            DoodleStatus::Ok
        }
        None => DoodleStatus::ErrNullPointer,
    })
}

/// Writes the current observation-mode granularity (E§8.8).
///
/// # Safety
/// `instance` live; `out_mode` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_observation_mode(
    instance: *const DoodleInstance,
    out_mode: *mut DoodleObservationMode,
) -> DoodleStatus {
    catch(|| match di_ref(instance) {
        Some(di)
            if write_out(
                out_mode,
                abi::observation_mode_of(di.inner.observation_mode()),
            ) =>
        {
            DoodleStatus::Ok
        }
        Some(_) => DoodleStatus::ErrNullPointer,
        None => DoodleStatus::ErrNullPointer,
    })
}

// ---- tail-elided history (pause-scoped) ------------------------------------------------------

/// Writes the number of tail-elided history entries (E§8.3). `ErrStale` on a stale `generation`.
///
/// # Safety
/// `instance` live; `out_count` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_tail_history_count(
    instance: *const DoodleInstance,
    generation: u32,
    out_count: *mut u32,
) -> DoodleStatus {
    catch(|| match checked_ref(instance, generation) {
        Ok(di) => write_count(di.inner.tail_history_count(), out_count),
        Err(status) => status,
    })
}

/// Writes the `index`-th tail-elided entry (most recent first, E§8.3): its declaration position to
/// `out_decl` and a fresh **host-owned** handle to the elided callable to `out_callable`
/// (release it). `ErrStale` on a stale `generation`; `ErrIndexOutOfBounds` past the last entry.
///
/// # Safety
/// `instance` live; `out_decl`/`out_callable` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_tail_frame_at(
    instance: *mut DoodleInstance,
    generation: u32,
    index: u32,
    out_decl: *mut DoodlePosition,
    out_callable: *mut DoodleHandle,
) -> DoodleStatus {
    catch(|| {
        // Validate both out-params before minting: `tail_history_entry` mints the callable handle,
        // so a NULL out (either one) would orphan it.
        if out_decl.is_null() || out_callable.is_null() {
            return DoodleStatus::ErrNullPointer;
        }
        let di = match checked_mut(instance, generation) {
            Ok(di) => di,
            Err(status) => return status,
        };
        match di.inner.tail_history_entry(index as usize) {
            Some((handle, position)) => {
                if write_out(out_decl, abi::position(position))
                    && write_out(out_callable, handle.bits())
                {
                    DoodleStatus::Ok
                } else {
                    DoodleStatus::ErrNullPointer
                }
            }
            None => DoodleStatus::ErrIndexOutOfBounds,
        }
    })
}

// ---- load diagnostics (S-63, D-M7-15) -------------------------------------------------------

/// Writes the number of load/exec diagnostics from cursor `since` onward (E§3.2/§8, S-63). A host
/// tracks its cursor as `since + count` to poll incrementally.
///
/// # Safety
/// `instance` live; `out_count` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_diagnostic_count(
    instance: *const DoodleInstance,
    since: usize,
    out_count: *mut u32,
) -> DoodleStatus {
    catch(|| match di_ref(instance) {
        Some(di) => write_count(di.inner.load_diagnostics(since).len(), out_count),
        None => DoodleStatus::ErrNullPointer,
    })
}

/// Fills `out_diagnostic` with the `index`-th diagnostic from cursor `since` (E§3.2/§8): its
/// severity and byte span. Its message copies out via [`doodle_diagnostic_message`].
/// `ErrIndexOutOfBounds` past the last diagnostic.
///
/// # Safety
/// `instance` live; `out_diagnostic` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_diagnostic_at(
    instance: *const DoodleInstance,
    since: usize,
    index: u32,
    out_diagnostic: *mut DoodleDiagnostic,
) -> DoodleStatus {
    catch(|| {
        let Some(di) = di_ref(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        let diagnostics = di.inner.load_diagnostics(since);
        match diagnostics.get(index as usize) {
            Some(diagnostic) => {
                let (has_span, span_start, span_end) = match diagnostic.span {
                    Some(span) => (true, span.start, span.end),
                    None => (false, 0, 0),
                };
                let filled = DoodleDiagnostic {
                    severity: abi::severity(diagnostic.severity),
                    has_span,
                    span_start,
                    span_end,
                    reserved: [0; 2],
                };
                if write_out(out_diagnostic, filled) {
                    DoodleStatus::Ok
                } else {
                    DoodleStatus::ErrNullPointer
                }
            }
            None => DoodleStatus::ErrIndexOutOfBounds,
        }
    })
}

/// Copies the `index`-th diagnostic's message (from cursor `since`) into `buf` (copy-out).
/// `ErrIndexOutOfBounds` past the last diagnostic.
///
/// # Safety
/// `instance` live; `buf` readable for `cap` (or NULL with `cap` 0); `out_len` may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_diagnostic_message(
    instance: *const DoodleInstance,
    since: usize,
    index: u32,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> DoodleStatus {
    catch(|| {
        let Some(di) = di_ref(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        let diagnostics = di.inner.load_diagnostics(since);
        match diagnostics.get(index as usize) {
            Some(diagnostic) => copy_out(diagnostic.message.as_bytes(), buf, cap, out_len),
            None => DoodleStatus::ErrIndexOutOfBounds,
        }
    })
}
