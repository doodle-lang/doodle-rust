//! The instance lifecycle + drive/resolve surface (E§3.1/§7): load a program to a
//! `DoodleInstance`, drive it, read the drive [`DoodleOutcome`], and resolve a parked
//! capability (E§7.5). All `unsafe` (raw-pointer marshalling) is confined here and in
//! [`crate::value`] (AD1). A program calls host extensions from the [`crate::registry`] it was
//! loaded with; the module resolver and arbitrary host foreign-function callbacks are later
//! M7.2 work.

use crate::abi::{
    self, DOODLE_NULL_HANDLE, DoodleHandle, DoodleOutcome, DoodleOutcomeKind, DoodleStatus,
};
use crate::guard::catch;
use crate::value::copy_out;
use doodle_core::drive::{Outcome, Resolution, resolve, run, run_slice};
use doodle_core::machine::{Handle, Instance};

/// A loaded Doodle program: the engine [`Instance`] plus a little host-facing state — the
/// described form of its most recent uncaught raise (for `doodle_raised_kind`/`_message`) and
/// the bound argument handles of a parked capability request (for `doodle_capability_arg`).
/// Opaque to C.
pub struct DoodleInstance {
    pub(crate) inner: Instance,
    /// The **pause generation** (D-M7-12): bumped by every state-advancing drive (`fill_outcome`,
    /// so `drive`/`resolve`/`resolve_import`), **not** by auxiliary evaluation (which restores the
    /// paused stack, S-22). The pause-scoped observation accessors ([`crate::observe`]) take a
    /// generation token from the stack walk and reject a stale one with `ErrStale`, so a debugger
    /// never resolves a frame index against a stack that a drive has since replaced.
    pub(crate) generation: u32,
    /// `(kind, message)` of the last `Raised` outcome (E§9), for the describe accessors.
    last_raised: Option<(String, String)>,
    /// The bound argument handles of a parked capability suspension (E§7.5): `Some` while the
    /// instance is `Suspended` on a capability, `None` otherwise. Each is a **host-owned**
    /// handle (S-17) the host reads via `doodle_capability_arg` and releases.
    pending_args: Option<Vec<Handle>>,
    /// The requested dotted path segments of a parked `import` suspension (E§6): `Some` while
    /// the instance is `SuspendedImport`, `None` otherwise. The host reads them via
    /// `doodle_import_path_segment` and answers with a `doodle_resolve_import*`.
    pending_import: Option<Vec<String>>,
}

mod import;
mod load;
pub use import::{
    doodle_import_path_segment, doodle_resolve_import, doodle_resolve_import_not_found,
    doodle_resolve_import_raise,
};
pub use load::{doodle_free, doodle_load, doodle_load_with_registry};

/// Drives `instance` under `directive` to its next stop, writing the result to `out_outcome`
/// (E§7.3). `directive` is a [`DoodleDirective`](crate::abi::DoodleDirective) value (an
/// out-of-range value is `ErrContract`, never UB). Unbounded (runs until a capability / pause /
/// raise / fault / completion); use [`doodle_drive_slice`] to bound the run with fuel.
///
/// # Safety
/// `instance` must be a live pointer from [`doodle_load`]; `out_outcome` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_drive(
    instance: *mut DoodleInstance,
    directive: u32,
    out_outcome: *mut DoodleOutcome,
) -> DoodleStatus {
    catch(|| {
        // Validate the host-supplied directive before use: an out-of-range value (a host bug or
        // version skew against a newer header) is `ErrContract`, not UB (freeze convention 5).
        let Some(directive) = abi::directive(directive) else {
            return DoodleStatus::ErrContract;
        };
        drive_and_fill(instance, out_outcome, true, |inst| run(inst, directive))
    })
}

/// Like [`doodle_drive`] but runs at most `fuel` statement safe points before yielding a
/// resumable `Paused(SliceEnd)` (S-40) — the host's cooperative-yield point. Re-drive to
/// continue. `directive` is a [`DoodleDirective`](crate::abi::DoodleDirective) value, validated
/// as in [`doodle_drive`] (out-of-range → `ErrContract`).
///
/// # Safety
/// As [`doodle_drive`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_drive_slice(
    instance: *mut DoodleInstance,
    directive: u32,
    fuel: u64,
    out_outcome: *mut DoodleOutcome,
) -> DoodleStatus {
    catch(|| {
        let Some(directive) = abi::directive(directive) else {
            return DoodleStatus::ErrContract;
        };
        // A zero-fuel slice yields `Paused(SliceEnd)` without running anything (it changes no
        // frames), so it must not bump the pause generation and invalidate the host's frame tokens.
        drive_and_fill(instance, out_outcome, fuel != 0, |inst| {
            run_slice(inst, directive, Some(fuel))
        })
    })
}

/// Requests cancellation (E§10.1): the drive tears down to `Faulted(Cancelled)` at its next safe
/// point. Idempotent; no-op on NULL. **Call from the thread that owns `instance`.** To cancel from
/// ANOTHER thread while a drive is running, obtain a `doodle_control` first and use
/// `doodle_control_cancel` — calling this cross-thread forms `&instance`, which would alias the
/// drive thread's `&mut Instance` (D-M7-5).
///
/// # Safety
/// `instance` must be a live pointer from [`doodle_load`] (or NULL), and not concurrently driven on
/// another thread (use `doodle_control` for that).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_cancel(instance: *const DoodleInstance) {
    if let Ok(di) = di_ref(instance) {
        di.inner.cancel_token().cancel();
    }
}

/// Writes the `index`-th bound argument handle of the parked capability request (E§7.5) to
/// `out_handle`. The handle is **host-owned** (S-17): read its value (`doodle_as_*` etc.) and
/// `doodle_release` it. `ErrContract` if the instance is not suspended on a capability;
/// `ErrIndexOutOfBounds` if `index >= DoodleOutcome::request_count`.
///
/// # Safety
/// `instance` must be a live pointer from [`doodle_load`]; `out_handle` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_capability_arg(
    instance: *const DoodleInstance,
    index: u32,
    out_handle: *mut DoodleHandle,
) -> DoodleStatus {
    catch(|| {
        let di = match di_ref(instance) {
            Ok(di) => di,
            Err(status) => return status,
        };
        match &di.pending_args {
            Some(args) => match args.get(index as usize) {
                Some(handle) if crate::value::write_out(out_handle, handle.bits()) => {
                    DoodleStatus::Ok
                }
                Some(_) => DoodleStatus::ErrNullPointer,
                None => DoodleStatus::ErrIndexOutOfBounds,
            },
            None => DoodleStatus::ErrContract,
        }
    })
}

/// Resolves a parked capability suspension (E§7.5) with `value` as its result, then drives on
/// and writes the next stop to `out_outcome`. `value` becomes the capability call's result (a
/// `to` capability ignores it, yielding Void). `ErrContract` if the instance is not suspended
/// on a capability. `value` is **borrowed, not consumed**: its value is copied into the resume and
/// the host still owns the handle, so `doodle_release` it when done (unlike the consuming
/// call-context setters, which take ownership of the handles passed to them, S-17).
///
/// # Safety
/// `instance` must be a live pointer from [`doodle_load`]; `out_outcome` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_resolve(
    instance: *mut DoodleInstance,
    value: DoodleHandle,
    out_outcome: *mut DoodleOutcome,
) -> DoodleStatus {
    catch(|| {
        resolve_and_fill(
            instance,
            out_outcome,
            Resolution::Value(Handle::from_bits(value)),
        )
    })
}

/// Resolves a parked capability suspension (E§7.5) by **raising** `value` at the capability
/// call site (E§9), then drives on. `ErrContract` if not suspended on a capability. `value` is
/// **borrowed, not consumed** — `doodle_release` it when done, as [`doodle_resolve`].
///
/// # Safety
/// As [`doodle_resolve`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_resolve_raise(
    instance: *mut DoodleInstance,
    value: DoodleHandle,
    out_outcome: *mut DoodleOutcome,
) -> DoodleStatus {
    catch(|| {
        resolve_and_fill(
            instance,
            out_outcome,
            Resolution::Raise(Handle::from_bits(value)),
        )
    })
}

/// Copies the described **kind** slug of the instance's most recent `Raised` outcome (E§9)
/// into `buf` (copy-out; `out_len` gets the full length). `DoodleStatus_ErrContract` if the
/// last drive did not raise.
///
/// # Safety
/// `instance` live; `buf` readable for `cap` (or NULL with `cap` 0); `out_len` may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_raised_kind(
    instance: *const DoodleInstance,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> DoodleStatus {
    describe_field(instance, buf, cap, out_len, |(kind, _)| kind)
}

/// Copies the **message** of the instance's most recent `Raised` outcome (E§9) into `buf`
/// (copy-out). `DoodleStatus_ErrContract` if the last drive did not raise.
///
/// # Safety
/// As [`doodle_raised_kind`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_raised_message(
    instance: *const DoodleInstance,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> DoodleStatus {
    describe_field(instance, buf, cap, out_len, |(_, message)| message)
}

/// Copies the instance's accumulated `print` output (raw UTF-8 bytes) into `buf` (copy-out;
/// `out_len` gets the full length). The output only grows; a host reads incrementally by
/// tracking how many bytes it has already consumed.
///
/// # Safety
/// `instance` live; `buf` readable for `cap` (or NULL with `cap` 0); `out_len` may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_output(
    instance: *const DoodleInstance,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> DoodleStatus {
    catch(|| match di_ref(instance) {
        Ok(di) => copy_out(di.inner.output(), buf, cap, out_len),
        Err(status) => status,
    })
}

// --- internals ---

thread_local! {
    /// True while a drive/resolve holds `&mut Instance` on this thread (set by [`DriveScope`]
    /// around the engine `run`/`resolve`). A foreign callback — or a finalizer fired at GC —
    /// runs *inside* that borrow; if it re-enters through any instance-pointer entry, forming a
    /// second `&mut`/`&Instance` would alias the live `&mut` (instantaneous UB). The accessors
    /// below check this and refuse (`ErrContract`) rather than form the aliasing reference. It is
    /// drive-scoped, **not** ctx-scoped (`CURRENT_CTX` is null during a finalizer), and the C ABI
    /// is single-threaded per instance (`!Sync`), so a thread-local is the whole story.
    static IN_DRIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// RAII marker for "a drive/resolve is in progress on this thread"; set around the engine
/// `run`/`resolve` (the window in which a callback can re-enter). Restores the previous value on
/// drop, so it is panic-safe and robust to any nesting.
struct DriveScope(bool);

impl DriveScope {
    fn enter() -> Self {
        DriveScope(IN_DRIVE.replace(true))
    }
}

impl Drop for DriveScope {
    fn drop(&mut self) {
        IN_DRIVE.set(self.0);
    }
}

/// Borrows the `DoodleInstance` behind a raw pointer — the one documented mutable deref the
/// drive/handle entry points share (also the observation surface, [`crate::observe`]). Returns
/// `ErrNullPointer` for NULL, or `ErrContract` if a drive is already in progress on this thread
/// (a reentrant instance-pointer call from inside a foreign callback/finalizer; see [`IN_DRIVE`]).
pub(crate) fn di_mut<'a>(
    instance: *mut DoodleInstance,
) -> Result<&'a mut DoodleInstance, DoodleStatus> {
    if IN_DRIVE.get() {
        return Err(DoodleStatus::ErrContract);
    }
    // SAFETY: `as_mut` returns None for NULL; a non-null `instance` is a live `DoodleInstance`
    // from `doodle_load` (not freed) by the caller's `# Safety` contract; the host drives one
    // instance from one thread at a time (`!Sync`); and the `IN_DRIVE` guard above rules out a
    // reentrant alias of a drive's live `&mut Instance`, so a `&mut` for `'a` is sound.
    unsafe { instance.as_mut() }.ok_or(DoodleStatus::ErrNullPointer)
}

/// Borrows the `DoodleInstance` behind a raw pointer for a shared read; `ErrNullPointer` for NULL,
/// `ErrContract` on a reentrant call (see [`di_mut`]).
pub(crate) fn di_ref<'a>(
    instance: *const DoodleInstance,
) -> Result<&'a DoodleInstance, DoodleStatus> {
    if IN_DRIVE.get() {
        return Err(DoodleStatus::ErrContract);
    }
    // SAFETY: as `di_mut`, for a shared borrow.
    unsafe { instance.as_ref() }.ok_or(DoodleStatus::ErrNullPointer)
}

/// Shared body of `doodle_drive`/`doodle_drive_slice`: run the drive, fill the out-outcome.
fn drive_and_fill(
    instance: *mut DoodleInstance,
    out_outcome: *mut DoodleOutcome,
    advanced: bool,
    run: impl FnOnce(&mut Instance) -> Outcome,
) -> DoodleStatus {
    let di = match di_mut(instance) {
        Ok(di) => di,
        Err(status) => return status,
    };
    if out_outcome.is_null() {
        return DoodleStatus::ErrNullPointer;
    }
    // Mark the drive in progress so a reentrant instance-pointer call from a foreign callback is
    // refused (`ErrContract`) instead of aliasing this `&mut Instance`.
    let outcome = {
        let _drive = DriveScope::enter();
        run(&mut di.inner)
    };
    let filled = fill_outcome(di, outcome, advanced);
    // SAFETY: `out_outcome` is non-null (checked) and writable/aligned for a `DoodleOutcome`
    // by the caller's `# Safety` contract.
    unsafe { *out_outcome = filled };
    DoodleStatus::Ok
}

/// Shared body of `doodle_resolve`/`doodle_resolve_raise`: resolve the parked capability, then
/// fill the out-outcome. `ErrContract` if the instance is not suspended on a capability.
fn resolve_and_fill(
    instance: *mut DoodleInstance,
    out_outcome: *mut DoodleOutcome,
    resolution: Resolution,
) -> DoodleStatus {
    let di = match di_mut(instance) {
        Ok(di) => di,
        Err(status) => return status,
    };
    if out_outcome.is_null() {
        return DoodleStatus::ErrNullPointer;
    }
    // `pending_args` is `Some` exactly while suspended on a capability (set by `fill_outcome`
    // on `Suspended`, cleared on every other outcome) — the engine's own resolve would fault a
    // non-suspended instance, but reporting a defined `ErrContract` is friendlier than the
    // `Faulted(Internal)`/debug-panic that misuse would otherwise produce.
    if di.pending_args.is_none() {
        return DoodleStatus::ErrContract;
    }
    let outcome = {
        let _drive = DriveScope::enter();
        resolve(&mut di.inner, resolution)
    };
    // A resolve always advances (it injects the host value and continues the drive).
    let filled = fill_outcome(di, outcome, true);
    // SAFETY: `out_outcome` is non-null (checked) and writable/aligned for a `DoodleOutcome`.
    unsafe { *out_outcome = filled };
    DoodleStatus::Ok
}

/// Fills a [`DoodleOutcome`] from a core [`Outcome`], stashing a raise's described form.
/// `advanced` is `false` only for a zero-fuel slice (`doodle_drive_slice(fuel=0)`), which yields
/// `Paused(SliceEnd)` before running anything, so it must not invalidate the host's frame tokens.
fn fill_outcome(di: &mut DoodleInstance, outcome: Outcome, advanced: bool) -> DoodleOutcome {
    let mut out = DoodleOutcome::blank();
    // A state-advancing drive may have replaced the stack: bump the pause generation (D-M7-12), so
    // a frame index a debugger obtained before this drive is now stale (`ErrStale`). A zero-fuel
    // slice ran nothing (`advanced == false`), so its tokens stay valid. Auxiliary evaluation
    // restores the paused stack (S-22) and does not route here, so it never bumps.
    if advanced {
        di.generation = di.generation.wrapping_add(1);
    }
    di.last_raised = None;
    di.pending_args = None;
    di.pending_import = None;
    match outcome {
        // A module drive completes Void; a reentrant `fn`'s result (M2b.5) rides in the result
        // register — intern it to a host-owned handle for `value` (`0`/Void otherwise, D-M7-15).
        Outcome::Completed(_) => {
            out.kind = DoodleOutcomeKind::Completed;
            out.value = di
                .inner
                .result_handle()
                .map_or(DOODLE_NULL_HANDLE, |h| h.bits());
        }
        Outcome::Suspended(request) => {
            out.kind = DoodleOutcomeKind::Suspended;
            out.capability = request.capability.0;
            out.request_count = request.args.len() as u32;
            // Stash the bound argument handles (host-owned, S-17) for `doodle_capability_arg`;
            // the host reads and releases them, then resolves.
            di.pending_args = Some(request.args);
        }
        Outcome::SuspendedImport(request) => {
            out.kind = DoodleOutcomeKind::SuspendedImport;
            out.capability = request.importer;
            out.request_count = request.path.len() as u32;
            di.pending_import = Some(request.path);
        }
        Outcome::Paused(reason) => {
            out.kind = DoodleOutcomeKind::Paused;
            let (pause_reason, breakpoint_id) = abi::pause_reason(reason);
            out.pause_reason = pause_reason;
            out.breakpoint_id = breakpoint_id;
        }
        Outcome::Raised(value, trace) => {
            out.kind = DoodleOutcomeKind::Raised;
            di.last_raised = Some(di.inner.describe_raised(value));
            // A **host-owned** handle to the raised exception (E§3.3/§8.4): the host inspects it
            // structurally (its `Error` `kind`/`message`/`details`, or any value) and releases it.
            // `0` (`DOODLE_NULL_HANDLE`) never occurs here (a raise always carries a value).
            out.value = di
                .inner
                .raised_value_handle()
                .map_or(DOODLE_NULL_HANDLE, |h| h.bits());
            if let Some(span) = trace.raised_at {
                out.has_span = true;
                out.span_start = span.start;
                out.span_end = span.end;
            }
        }
        Outcome::Faulted(fault) => {
            out.kind = DoodleOutcomeKind::Faulted;
            out.fault = abi::fault(fault);
        }
    }
    out
}

/// Shared body of the raise-describe accessors: copies out one field of `last_raised`.
fn describe_field(
    instance: *const DoodleInstance,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
    select: impl Fn(&(String, String)) -> &String,
) -> DoodleStatus {
    catch(|| match di_ref(instance) {
        Ok(di) => match &di.last_raised {
            Some(pair) => copy_out(select(pair).as_bytes(), buf, cap, out_len),
            None => DoodleStatus::ErrContract,
        },
        Err(status) => status,
    })
}
