//! The instance lifecycle + drive surface (E§3.1/§7): load a program to a `DoodleInstance`,
//! drive it, and read the drive [`DoodleOutcome`]. All `unsafe` (raw-pointer marshalling) is
//! confined here and in [`crate::value`] (AD1). Host capabilities, the module resolver, and
//! foreign functions are M7.2 — an M7.1 program loads with no host extensions, so a drive
//! reaches `Completed`/`Paused`/`Raised`/`Faulted` (never `Suspended`).

use crate::abi::{self, DoodleDirective, DoodleOutcome, DoodleOutcomeKind, DoodleStatus};
use crate::config::DoodleConfig;
use crate::guard::catch;
use crate::value::copy_out;
use doodle_core::diag::Severity;
use doodle_core::drive::{Config, Outcome, run, run_slice};
use doodle_core::machine::Instance;
use doodle_core::span::ModuleId;

/// A loaded Doodle program: the engine [`Instance`] plus the described form of its most
/// recent uncaught raise (so `doodle_raised_kind`/`_message` can copy it out). Opaque to C.
pub struct DoodleInstance {
    pub(crate) inner: Instance,
    /// `(kind, message)` of the last `Raised` outcome (E§9), for the describe accessors.
    last_raised: Option<(String, String)>,
}

/// Loads a program from UTF-8 `source` under `config`, writing a new instance to
/// `out_instance` on success (`DoodleStatus_Ok`). The host owns the returned instance and
/// frees it with [`doodle_free`]; `config` is not consumed (free it separately).
///
/// On a lex/parse/resolve error the status is `DoodleStatus_ErrLoad` and the human-readable
/// error text is copied into `err_buf` (up to `err_buf_cap` bytes, always NUL-free UTF-8),
/// with the full byte length written to `out_err_len` (copy-out; a too-small buffer still
/// reports the needed length). `config` may be NULL (engine defaults).
///
/// # Safety
/// `source` must point to `source_len` readable bytes; `out_instance` must be a writable
/// `*DoodleInstance`; `err_buf` may be NULL only if `err_buf_cap` is 0; `out_err_len` and
/// `config` may be NULL. Pointers must not alias in ways C forbids.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_load(
    source: *const u8,
    source_len: usize,
    config: *const DoodleConfig,
    out_instance: *mut *mut DoodleInstance,
    err_buf: *mut u8,
    err_buf_cap: usize,
    out_err_len: *mut usize,
) -> DoodleStatus {
    catch(|| {
        if source.is_null() || out_instance.is_null() {
            return DoodleStatus::ErrNullPointer;
        }
        // SAFETY: `source` is non-null (checked) and points to `source_len` readable bytes by
        // the caller's `# Safety` contract; the slice is not held past this call.
        let bytes = unsafe { std::slice::from_raw_parts(source, source_len) };
        let Ok(text) = std::str::from_utf8(bytes) else {
            return DoodleStatus::ErrInvalidUtf8;
        };
        // SAFETY: `as_ref` returns None for NULL; a non-null `config` is a live `DoodleConfig`
        // from `doodle_config_new` by the caller's contract. `Config` is `Copy` — nothing is
        // aliased or consumed.
        let cfg = unsafe { config.as_ref() }.map_or_else(Config::default, |c| c.inner);

        match load_program(text, cfg) {
            Ok(instance) => {
                let boxed = Box::new(DoodleInstance {
                    inner: instance,
                    last_raised: None,
                });
                // SAFETY: `out_instance` was null-checked above and is a writable
                // `*mut *mut DoodleInstance` by the caller's contract.
                unsafe { *out_instance = Box::into_raw(boxed) };
                DoodleStatus::Ok
            }
            Err(LoadFailure::Diagnostics(message)) => {
                copy_out(message.as_bytes(), err_buf, err_buf_cap, out_err_len);
                DoodleStatus::ErrLoad
            }
            Err(LoadFailure::UnsupportedUnicode) => DoodleStatus::ErrUnsupportedUnicode,
        }
    })
}

/// Frees an instance from [`doodle_load`], running the finalizer of every live foreign value
/// (E§3.1/§4.5). NULL is a no-op. After this the pointer is invalid.
///
/// # Safety
/// `instance` must be a pointer from `doodle_load` that has not already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_free(instance: *mut DoodleInstance) {
    if !instance.is_null() {
        // SAFETY: `instance` is non-null (checked) and, by the caller's `# Safety` contract, a
        // pointer from `doodle_load` (a `Box::into_raw`) not already freed. `Instance`'s `Drop`
        // finalizes every live foreign value (E§4.5).
        drop(unsafe { Box::from_raw(instance) });
    }
}

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

/// Fills a [`DoodleOutcome`] from a core [`Outcome`], stashing a raise's described form.
fn fill_outcome(di: &mut DoodleInstance, outcome: Outcome) -> DoodleOutcome {
    let mut out = DoodleOutcome::blank();
    di.last_raised = None;
    match outcome {
        // A module drive completes Void; a returning `fn`'s result (M2b.5) will populate
        // `value` once an intern-to-handle path exists on the boundary (M7.3).
        Outcome::Completed(_) => out.kind = DoodleOutcomeKind::Completed,
        Outcome::Suspended(request) => {
            out.kind = DoodleOutcomeKind::Suspended;
            out.capability = request.capability.0;
            out.request_count = request.args.len() as u32;
            // The arg handles are host-owned (S-17); M7.2 exposes them via accessors. Until
            // then (unreachable — no capability is registered in M7.1) release them so a
            // stray suspend cannot leak handles.
            for handle in request.args {
                let _ = di.inner.release(handle);
            }
        }
        Outcome::SuspendedImport(request) => {
            out.kind = DoodleOutcomeKind::SuspendedImport;
            out.capability = request.importer;
            out.request_count = request.path.len() as u32;
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

/// A load failure: front-end diagnostics (with a rendered message) or a rejected config.
enum LoadFailure {
    Diagnostics(String),
    UnsupportedUnicode,
}

/// Runs the front-end pipeline (normalize → parse → resolve, E§3.1) then `create`s the
/// instance under `config`. Any `Severity::Error` diagnostic fails the load with a rendered
/// message (span-prefixed, one per line), matching the wasm facade's `errors_of`.
fn load_program(source: &str, config: Config) -> Result<Instance, LoadFailure> {
    let normalized = doodle_core::source::normalize(source);
    let parsed = doodle_core::parse::parse_program(normalized.as_ref(), ModuleId(0));
    if let Some(message) = errors_of(&parsed.diagnostics) {
        return Err(LoadFailure::Diagnostics(message));
    }
    let resolved = doodle_core::resolve::resolve(parsed.ast, parsed.root, ModuleId(0));
    if let Some(message) = errors_of(&resolved.diagnostics) {
        return Err(LoadFailure::Diagnostics(message));
    }
    Instance::create(resolved.module, config).map_err(|_| LoadFailure::UnsupportedUnicode)
}

/// Renders the `Severity::Error` diagnostics as one span-prefixed message per line, or
/// `None` if the unit loaded clean (warnings do not block a load, E§3.1).
fn errors_of(diagnostics: &[doodle_core::diag::Diagnostic]) -> Option<String> {
    let lines: Vec<String> = diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| match d.span {
            Some(span) => format!("[{}..{}] {}", span.start, span.end, d.message),
            None => d.message.clone(),
        })
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}
