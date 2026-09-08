//! The C ABI **retained-trace** surface (engine spec E§9, M7.7 R6): the post-mortem view of a
//! terminal uncaught raise — its live frames and bounded tail-elided history (§8.3), each as a
//! **position and a callable**. The engine retains this trace on the terminal `Raised` state
//! (§3.3), so — unlike the live [`stack_walk`](super) surface — these accessors carry **no
//! pause-generation token**: a terminal instance runs no more program work, so the trace and its
//! innermost-0 frame indices are stable for the instance's life. They complement the described
//! `kind`/`message` + raise span already on [`DoodleOutcome`](crate::abi::DoodleOutcome); the
//! trace is the part the outcome does not carry.
//!
//! **Contract: call on the terminal `Raised` instance.** A callable is minted **lazily**
//! (`doodle_raised_trace_frame_callable`/`_tail_at`), so a host reading only positions pays for no
//! handles. `*_frame_count`/`*_tail_count` write `0` when the last drive did **not** end `Raised`
//! (a real trace always has at least one frame).

use crate::abi::{
    self, DOODLE_NULL_HANDLE, DoodleFrame, DoodleHandle, DoodlePosition, DoodleStatus,
};
use crate::guard::catch;
use crate::instance::{DoodleInstance, di_mut, di_ref};
use crate::value::write_out;

/// The zeroed position written where a frame has no call site (E§8.2).
const NO_POSITION: DoodlePosition = DoodlePosition {
    span_start: 0,
    span_end: 0,
    module: 0,
};

/// Writes the number of **live frames** in the retained trace of the last terminal raise (E§9)
/// to `out_count`: innermost-0, the index space of `doodle_raised_trace_frame_at` /
/// `_frame_callable`. `0` when the last drive did not end `Raised`.
///
/// # Safety
/// `instance` live; `out_count` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_raised_trace_frame_count(
    instance: *const DoodleInstance,
    out_count: *mut u32,
) -> DoodleStatus {
    catch(|| {
        let di = match di_ref(instance) {
            Ok(di) => di,
            Err(status) => return status,
        };
        let count = di.inner.raised_trace_frame_count().unwrap_or(0);
        if write_out(out_count, u32::try_from(count).unwrap_or(u32::MAX)) {
            DoodleStatus::Ok
        } else {
            DoodleStatus::ErrNullPointer
        }
    })
}

/// Fills `out_frame` with innermost-first frame `index` of the retained trace (E§9), pure data
/// (its callable is minted separately by `doodle_raised_trace_frame_callable`). Past the frame
/// count, or no retained trace, is `ErrIndexOutOfBounds`. The frame `module` token is `0`;
/// per-frame module resolution for a trace lands with cross-module call-site spans (M5.1).
///
/// # Safety
/// `instance` live; `out_frame` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_raised_trace_frame_at(
    instance: *const DoodleInstance,
    index: u32,
    out_frame: *mut DoodleFrame,
) -> DoodleStatus {
    catch(|| {
        let di = match di_ref(instance) {
            Ok(di) => di,
            Err(status) => return status,
        };
        let Some(info) = di.inner.raised_trace_frame(index as usize) else {
            return DoodleStatus::ErrIndexOutOfBounds;
        };
        let frame = DoodleFrame {
            has_callable: info.has_callable,
            has_call_site: info.call_site.is_some(),
            call_site: info.call_site.map_or(NO_POSITION, |span| DoodlePosition {
                span_start: span.start,
                span_end: span.end,
                module: 0,
            }),
            tail_count: info.tail_count,
            module: 0,
            reserved: [0; 4],
        };
        if write_out(out_frame, frame) {
            DoodleStatus::Ok
        } else {
            DoodleStatus::ErrNullPointer
        }
    })
}

/// A fresh **host-owned** handle to frame `index`'s callable in the retained trace (E§9), or
/// `DOODLE_NULL_HANDLE` for a module-top / block frame (no callable — `has_callable` said so).
/// `index` past the frame count, or no retained trace, is `ErrIndexOutOfBounds`. The handle is
/// ordinary and host-owned — `doodle_release` it. The retained trace roots the callable, so it is
/// valid for the terminal state's life though the live frame has unwound.
///
/// # Safety
/// `instance` live; `out_handle` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_raised_trace_frame_callable(
    instance: *mut DoodleInstance,
    index: u32,
    out_handle: *mut DoodleHandle,
) -> DoodleStatus {
    catch(|| {
        // Validate the out-param before minting, so a NULL out never orphans the callable handle.
        if out_handle.is_null() {
            return DoodleStatus::ErrNullPointer;
        }
        let di = match di_mut(instance) {
            Ok(di) => di,
            Err(status) => return status,
        };
        if index as usize >= di.inner.raised_trace_frame_count().unwrap_or(0) {
            return DoodleStatus::ErrIndexOutOfBounds;
        }
        let bits = di
            .inner
            .raised_trace_frame_callable(index as usize)
            .map_or(DOODLE_NULL_HANDLE, |h| h.bits());
        if write_out(out_handle, bits) {
            DoodleStatus::Ok
        } else {
            DoodleStatus::ErrNullPointer
        }
    })
}

/// Writes the number of **tail-elided** entries in the retained trace (E§8.3) to `out_count`,
/// most-recent-first, the index space of `doodle_raised_trace_tail_at`. `0` when the last drive did
/// not end `Raised`.
///
/// # Safety
/// `instance` live; `out_count` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_raised_trace_tail_count(
    instance: *const DoodleInstance,
    out_count: *mut u32,
) -> DoodleStatus {
    catch(|| {
        let di = match di_ref(instance) {
            Ok(di) => di,
            Err(status) => return status,
        };
        let count = di.inner.raised_trace_tail_count().unwrap_or(0);
        if write_out(out_count, u32::try_from(count).unwrap_or(u32::MAX)) {
            DoodleStatus::Ok
        } else {
            DoodleStatus::ErrNullPointer
        }
    })
}

/// The `index`-th tail-elided entry of the retained trace (most recent first, E§8.3): a fresh
/// **host-owned** handle to the elided callable in `out_handle`, and its declaration position in
/// `out_position` (a real module token: the callable's home module). Past the count, or no
/// retained trace, is `ErrIndexOutOfBounds`. `doodle_release` the handle.
///
/// # Safety
/// `instance` live; `out_handle`/`out_position` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_raised_trace_tail_at(
    instance: *mut DoodleInstance,
    index: u32,
    out_handle: *mut DoodleHandle,
    out_position: *mut DoodlePosition,
) -> DoodleStatus {
    catch(|| {
        // Validate the out-params before minting, so a NULL out never orphans the callable handle.
        if out_handle.is_null() || out_position.is_null() {
            return DoodleStatus::ErrNullPointer;
        }
        let di = match di_mut(instance) {
            Ok(di) => di,
            Err(status) => return status,
        };
        if index as usize >= di.inner.raised_trace_tail_count().unwrap_or(0) {
            return DoodleStatus::ErrIndexOutOfBounds;
        }
        let (handle, position) = match di.inner.raised_trace_tail_entry(index as usize) {
            Some(entry) => entry,
            None => return DoodleStatus::ErrIndexOutOfBounds,
        };
        if write_out(out_handle, handle.bits()) && write_out(out_position, abi::position(position))
        {
            DoodleStatus::Ok
        } else {
            DoodleStatus::ErrNullPointer
        }
    })
}
