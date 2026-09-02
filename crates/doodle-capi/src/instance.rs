//! The instance lifecycle + drive/resolve surface (E§3.1/§7): load a program to a
//! `DoodleInstance`, drive it, read the drive [`DoodleOutcome`], and resolve a parked
//! capability (E§7.5). All `unsafe` (raw-pointer marshalling) is confined here and in
//! [`crate::value`] (AD1). A program calls host extensions from the [`crate::registry`] it was
//! loaded with; the module resolver and arbitrary host foreign-function callbacks are later
//! M7.2 work.

use crate::abi::{
    self, DoodleDirective, DoodleHandle, DoodleOutcome, DoodleOutcomeKind, DoodleStatus,
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
/// (E§7.3). Unbounded (runs until a capability / pause / raise / fault / completion); use
/// [`doodle_drive_slice`] to bound the run with fuel.
///
/// # Safety
/// `instance` must be a live pointer from [`doodle_load`]; `out_outcome` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_drive(
    instance: *mut DoodleInstance,
    directive: DoodleDirective,
    out_outcome: *mut DoodleOutcome,
) -> DoodleStatus {
    catch(|| {
        drive_and_fill(instance, out_outcome, |inst| {
            run(inst, abi::directive(directive))
        })
    })
}

/// Like [`doodle_drive`] but runs at most `fuel` statement safe points before yielding a
/// resumable `Paused(SliceEnd)` (S-40) — the host's cooperative-yield point. Re-drive to
/// continue.
///
/// # Safety
/// As [`doodle_drive`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_drive_slice(
    instance: *mut DoodleInstance,
    directive: DoodleDirective,
    fuel: u64,
    out_outcome: *mut DoodleOutcome,
) -> DoodleStatus {
    catch(|| {
        drive_and_fill(instance, out_outcome, |inst| {
            run_slice(inst, abi::directive(directive), Some(fuel))
        })
    })
}

/// Requests cancellation (E§10.1): the drive tears down to `Faulted(Cancelled)` at its next
/// safe point. Idempotent; callable from another thread while a drive runs (the cancel flag
/// is atomic). No-op on NULL.
///
/// # Safety
/// `instance` must be a live pointer from [`doodle_load`] (or NULL).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_cancel(instance: *const DoodleInstance) {
    if let Some(di) = di_ref(instance) {
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
        let Some(di) = di_ref(instance) else {
            return DoodleStatus::ErrNullPointer;
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
/// on a capability.
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
/// call site (E§9), then drives on. `ErrContract` if not suspended on a capability.
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
        Some(di) => copy_out(di.inner.output(), buf, cap, out_len),
        None => DoodleStatus::ErrNullPointer,
    })
}

// --- internals ---

/// Borrows the `DoodleInstance` behind a raw pointer, or `None` if NULL — the one documented
/// mutable deref the drive/handle entry points share.
fn di_mut<'a>(instance: *mut DoodleInstance) -> Option<&'a mut DoodleInstance> {
    // SAFETY: `as_mut` returns None for NULL; a non-null `instance` is a live `DoodleInstance`
    // from `doodle_load` (not freed) by the caller's `# Safety` contract, and the host drives
    // one instance from one thread at a time (`!Sync`), so a `&mut` for `'a` is sound.
    unsafe { instance.as_mut() }
}

/// Borrows the `DoodleInstance` behind a raw pointer for a shared read, or `None` if NULL.
fn di_ref<'a>(instance: *const DoodleInstance) -> Option<&'a DoodleInstance> {
    // SAFETY: as `di_mut`, for a shared borrow.
    unsafe { instance.as_ref() }
}

/// Shared body of `doodle_drive`/`doodle_drive_slice`: run the drive, fill the out-outcome.
fn drive_and_fill(
    instance: *mut DoodleInstance,
    out_outcome: *mut DoodleOutcome,
    run: impl FnOnce(&mut Instance) -> Outcome,
) -> DoodleStatus {
    let Some(di) = di_mut(instance) else {
        return DoodleStatus::ErrNullPointer;
    };
    if out_outcome.is_null() {
        return DoodleStatus::ErrNullPointer;
    }
    let outcome = run(&mut di.inner);
    let filled = fill_outcome(di, outcome);
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
    let Some(di) = di_mut(instance) else {
        return DoodleStatus::ErrNullPointer;
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
    let outcome = resolve(&mut di.inner, resolution);
    let filled = fill_outcome(di, outcome);
    // SAFETY: `out_outcome` is non-null (checked) and writable/aligned for a `DoodleOutcome`.
    unsafe { *out_outcome = filled };
    DoodleStatus::Ok
}

/// Fills a [`DoodleOutcome`] from a core [`Outcome`], stashing a raise's described form.
fn fill_outcome(di: &mut DoodleInstance, outcome: Outcome) -> DoodleOutcome {
    let mut out = DoodleOutcome::blank();
    di.last_raised = None;
    di.pending_args = None;
    di.pending_import = None;
    match outcome {
        // A module drive completes Void; a returning `fn`'s result (M2b.5) will populate
        // `value` once an intern-to-handle path exists on the boundary (M7.3).
        Outcome::Completed(_) => out.kind = DoodleOutcomeKind::Completed,
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
        Some(di) => match &di.last_raised {
            Some(pair) => copy_out(select(pair).as_bytes(), buf, cap, out_len),
            None => DoodleStatus::ErrContract,
        },
        None => DoodleStatus::ErrNullPointer,
    })
}
