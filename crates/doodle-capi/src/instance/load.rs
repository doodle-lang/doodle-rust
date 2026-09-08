//! Loading a program into a `DoodleInstance` (E§3.1) and freeing it (E§4.5) — the front-end
//! pipeline (normalize → parse → resolve) plus `create` with the host `registry`, and the
//! source/config/error out-param marshalling. Split from `instance.rs` (the drive/resolve
//! surface) to keep that file within the hygiene length limit; a child module, so it reaches
//! the parent's private `DoodleInstance` fields.

use super::DoodleInstance;
use crate::abi::DoodleStatus;
use crate::config::DoodleConfig;
use crate::guard::{catch, catch_or};
use crate::registry::DoodleRegistry;
use crate::value::copy_out;
use doodle_core::diag::Severity;
use doodle_core::drive::Config;
use doodle_core::machine::{Instance, Registry};
use doodle_core::span::ModuleId;

/// The entry-module canonical id (E§3.2) the C ABI loads under. The engine has no magic default
/// (D-M7-17), so the ABI names its own: a C host that does not (yet) pass a path gets `"main"`, the
/// conventional entry name. When the ABI grows a caller-supplied module-path parameter this becomes
/// the fallback for a NULL argument.
const DEFAULT_MODULE_PATH: &str = "main";

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
        load_impl(
            source,
            source_len,
            config,
            Registry::new(),
            out_instance,
            err_buf,
            err_buf_cap,
            out_err_len,
        )
    })
}

/// Like [`doodle_load`] but with host extensions from `registry` (E§5.5) — capabilities and
/// built-ins the program can call. **Consumes** `registry` (moved into the instance): the
/// pointer is invalid after this call, whether it succeeds or fails; do not free or reuse it.
///
/// # Safety
/// As [`doodle_load`], plus `registry` must be a pointer from `doodle_registry_new` not
/// already consumed/freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_load_with_registry(
    source: *const u8,
    source_len: usize,
    config: *const DoodleConfig,
    registry: *mut DoodleRegistry,
    out_instance: *mut *mut DoodleInstance,
    err_buf: *mut u8,
    err_buf_cap: usize,
    out_err_len: *mut usize,
) -> DoodleStatus {
    catch(|| {
        if registry.is_null() {
            return DoodleStatus::ErrNullPointer;
        }
        // SAFETY: non-null (checked); a `Box::into_raw` pointer from `doodle_registry_new`, not
        // consumed/freed, by the caller's contract. Consuming it here is the documented
        // contract (the pointer is invalid afterward).
        let registry = unsafe { Box::from_raw(registry) }.inner;
        load_impl(
            source,
            source_len,
            config,
            registry,
            out_instance,
            err_buf,
            err_buf_cap,
            out_err_len,
        )
    })
}

/// Shared body of the load entry points: front-end + `create` with `registry`, marshalling
/// the source/config/error out-params. Runs inside the callers' `catch`.
#[allow(clippy::too_many_arguments)]
fn load_impl(
    source: *const u8,
    source_len: usize,
    config: *const DoodleConfig,
    registry: Registry,
    out_instance: *mut *mut DoodleInstance,
    err_buf: *mut u8,
    err_buf_cap: usize,
    out_err_len: *mut usize,
) -> DoodleStatus {
    if source.is_null() || out_instance.is_null() {
        return DoodleStatus::ErrNullPointer;
    }
    // SAFETY: `source` is non-null (checked) and points to `source_len` readable bytes by the
    // caller's `# Safety` contract; the slice is not held past this call.
    let bytes = unsafe { std::slice::from_raw_parts(source, source_len) };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return DoodleStatus::ErrInvalidUtf8;
    };
    // SAFETY: `as_ref` returns None for NULL; a non-null `config` is a live `DoodleConfig` from
    // `doodle_config_new` by the caller's contract. `Config` is `Copy` — nothing is aliased.
    let cfg = unsafe { config.as_ref() }.map_or_else(Config::default, |c| c.inner);

    match load_program(text, cfg, registry) {
        Ok(instance) => {
            let boxed = Box::new(DoodleInstance {
                inner: instance,
                generation: 0,
                last_raised: None,
                pending_args: None,
                pending_import: None,
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
}

/// Frees an instance from [`doodle_load`], running the finalizer of every live foreign value
/// (E§3.1/§4.5). NULL is a no-op. After this the pointer is invalid.
///
/// # Safety
/// `instance` must be a pointer from `doodle_load` that has not already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_free(instance: *mut DoodleInstance) {
    // The drop runs foreign finalizers (host `extern "C"` callbacks) via `Instance`'s `Drop`;
    // convention 5 forbids a panic crossing the boundary, and this entry point has no
    // `DoodleStatus` channel, so `catch_or` swallows a finalizer panic (the instance is being
    // discarded anyway) rather than let it unwind into the C caller (UB).
    catch_or((), || {
        if !instance.is_null() {
            // SAFETY: `instance` is non-null (checked) and, by the caller's `# Safety` contract, a
            // pointer from `doodle_load` (a `Box::into_raw`) not already freed. `Instance`'s `Drop`
            // finalizes every live foreign value (E§4.5).
            drop(unsafe { Box::from_raw(instance) });
        }
    });
}

/// A load failure: front-end diagnostics (with a rendered message) or a rejected config.
enum LoadFailure {
    Diagnostics(String),
    UnsupportedUnicode,
}

/// Runs the front-end pipeline (normalize → parse → resolve, E§3.1) then builds the instance
/// under `config` with `registry`'s host extensions (E§5.5). Any `Severity::Error` diagnostic
/// fails the load with a rendered message (span-prefixed, one per line), matching the wasm
/// facade's `errors_of`. Composed from the public APIs (rather than `Instance::create`, which
/// takes no registry): validate the target Unicode version (S-41), load with the registry +
/// limits, then apply the observation mode — the same steps `create` runs.
fn load_program(source: &str, config: Config, registry: Registry) -> Result<Instance, LoadFailure> {
    // S-41: validate the pure config field FIRST (before parse/resolve), so a mismatched target
    // Unicode version is reported as `ErrUnsupportedUnicode` even when the source ALSO has a parse
    // error (the config mismatch is independent of the source; a parse error must not mask it). A
    // recording asserts its version at create rather than diverging silently (E§11).
    if let Some(requested) = config.unicode_version
        && requested != Instance::unicode_version()
    {
        return Err(LoadFailure::UnsupportedUnicode);
    }
    let normalized = doodle_core::source::normalize(source);
    let parsed = doodle_core::parse::parse_program(normalized.as_ref(), ModuleId(0));
    if let Some(message) = errors_of(&parsed.diagnostics) {
        return Err(LoadFailure::Diagnostics(message));
    }
    let resolved = doodle_core::resolve::resolve(parsed.ast, parsed.root, ModuleId(0));
    if let Some(message) = errors_of(&resolved.diagnostics) {
        return Err(LoadFailure::Diagnostics(message));
    }
    let mut instance = Instance::load(
        resolved.module,
        config.limits,
        registry,
        DEFAULT_MODULE_PATH,
    );
    instance.set_observation_mode(config.observation_mode);
    // The `DOODLE_GC_STRESS` certification hook (M7.6, D-M7-11): a host-side test toggle that makes
    // every safe point collect, so a determinism gate can drive the corpus through this C ABI under
    // GC pressure and confirm the trace matches the un-stressed oracle — exercising what in-crate
    // GC tests cannot (host-held handles as roots, finalizers firing at GC time across the C
    // trampoline). Latched pre-first-drive, so a freshly loaded instance always accepts it. NOT
    // part of the frozen ABI: it adds no `doodle.h` symbol.
    //
    // **The ambient read is behind the `gc-stress` feature** (off by default): the shipped library
    // must not silently vary with an environment variable, so only the gate build reads it. Only
    // this env read is gated: the explicit `Instance::enable_gc_stress` path stays available in
    // every build (that is the sanctioned input; the engine itself never reads the environment).
    #[cfg(feature = "gc-stress")]
    if std::env::var_os("DOODLE_GC_STRESS").is_some_and(|v| !v.is_empty()) {
        instance
            .enable_gc_stress()
            .expect("GC-stress latched on a freshly loaded instance (pre-first-drive)");
    }
    Ok(instance)
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
