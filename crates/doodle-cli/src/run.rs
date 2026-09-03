//! `doodle run <file>` (engine spec E§3.1/§7): load a single Doodle program and drive it to a
//! terminal outcome, acting as its host.
//!
//! The CLI is a real host (M3.2): it registers the demo intrinsics + the ambient capabilities and
//! **resolves** each request from the outside world — `read_line` from stdin, `time` from the
//! wall clock, `random` from a seeded PRNG (D-M7-16). Those readings enter only as capability
//! resolutions, so the engine stays deterministic and a recorded run replays bit-for-bit (E§11).
//! `print` output streams to stdout as it is produced; an uncaught raise or an engine fault renders
//! to stderr with a source snippet, and the process exit code reflects the outcome (0 completed,
//! nonzero otherwise). An `import` resolves against the filesystem ([`resolve_import_request`]):
//! the dotted path maps to a `.doodle` file beside the importing module (E§6, S-7).

use crate::rng::Rng;
use doodle_core::diag::render::{SourceView, render_diagnostics, render_raise};
use doodle_core::diag::{Diagnostic, Severity};
use doodle_core::drive::{
    self, CapabilityRequest, Directive, EngineFault, ImportRequest, ImportResolution, LimitKind,
    Limits, Outcome, Resolution,
};
use doodle_core::machine::{
    Instance, Registry, print_intrinsic, random_intrinsic, read_line_intrinsic, time_intrinsic,
};
use doodle_core::parse::parse_program;
use doodle_core::resolve::resolve;
use doodle_core::source::normalize;
use doodle_core::span::ModuleId;
use std::io::{BufRead, Write};
use std::path::Path;

/// The capability ids the CLI resolves, fixed by the registration order in [`build_registry`]:
/// `print` is index 0 (a synchronous intrinsic that never suspends), then the three ambient
/// capabilities. Stable across runs, so a recording replays each resolution against a stable
/// identity (E§11, S-43).
const CAP_READ_LINE: u32 = 1;
const CAP_TIME: u32 = 2;
const CAP_RANDOM: u32 = 3;

/// Options for `doodle run`, parsed by the argument front end (`main.rs`).
pub struct RunOptions {
    /// The program file to run (its path is the entry module's display name + canonical id).
    pub file: String,
    /// The `--seed N` value for `random`, or `None` to seed from the wall clock (D-M7-16).
    pub seed: Option<u64>,
}

/// Runs the program at `options.file`, returning the process exit code: `0` on `Completed`, `1` on
/// an uncaught raise / engine fault / load error, `2` on a file-read error.
pub fn run(options: RunOptions) -> u8 {
    let source = match std::fs::read_to_string(&options.file) {
        Ok(source) => source,
        Err(e) => {
            eprintln!("doodle: cannot read {}: {e}", options.file);
            return 2;
        }
    };
    let nfc = normalize(&source);
    let view = SourceView {
        name: &options.file,
        source: nfc.as_ref(),
    };

    // Front end: any error-severity diagnostic fails the load (E§3.1), rendered to stderr.
    let parsed = parse_program(nfc.as_ref(), ModuleId(0));
    if let Some(rendered) = render_errors(&parsed.diagnostics, &view) {
        eprint!("{rendered}");
        return 1;
    }
    let resolved = resolve(parsed.ast, parsed.root, ModuleId(0));
    if let Some(rendered) = render_errors(&resolved.diagnostics, &view) {
        eprint!("{rendered}");
        return 1;
    }

    // The entry module's canonical id (E§3.2, D-M7-17): the normalized absolute path, so an import
    // later resolves relative to it (M7.4d) and breakpoints/traces address it stably. Falls back to
    // the raw path if the filesystem cannot canonicalize it.
    let entry_id = std::fs::canonicalize(&options.file)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| options.file.clone());
    let mut inst = Instance::load(
        resolved.module,
        Limits::default(),
        build_registry(),
        &entry_id,
    );

    // S-63 load diagnostics (e.g. prelude-shadowing warnings): shown, never blocking (E§3.1).
    let load_warnings = render_diagnostics(inst.load_diagnostics(0), &view);
    if !load_warnings.trim().is_empty() {
        eprint!("{load_warnings}");
    }

    drive_to_terminal(&mut inst, &view, options.seed)
}

/// The CLI's registry (E§5.5, S-43): the demo `print` plus the three ambient capabilities, in a
/// fixed order so the [`CAP_*`](CAP_READ_LINE) ids and any recording stay stable (E§11).
fn build_registry() -> Registry {
    let mut registry = Registry::new();
    for intrinsic in [
        print_intrinsic(),
        read_line_intrinsic(),
        time_intrinsic(),
        random_intrinsic(),
    ] {
        registry
            .register(intrinsic)
            .expect("a CLI intrinsic registers cleanly into a fresh registry");
    }
    registry
}

/// Drives `inst` to a terminal outcome, streaming `print` output to stdout, resolving each
/// capability/import request, and rendering a raise/fault to stderr. Returns the exit code.
fn drive_to_terminal(inst: &mut Instance, view: &SourceView<'_>, seed: Option<u64>) -> u8 {
    let mut rng = Rng::new(seed.unwrap_or_else(entropy_seed));
    let mut flushed = 0usize;
    let mut outcome = drive::run(inst, Directive::RunToCompletion);
    loop {
        // Stream whatever `print` produced in the last step before we act — so a prompt shows
        // before we block on stdin, and program output precedes any error we are about to render.
        flush_output(inst, &mut flushed);
        match outcome {
            Outcome::Completed(_) => return 0,
            Outcome::Suspended(request) => {
                let resolution = resolve_capability(inst, &request, &mut rng);
                outcome = drive::resolve(inst, resolution);
            }
            Outcome::SuspendedImport(request) => {
                let resolution = resolve_import_request(inst, &request);
                outcome = drive::resolve_import(inst, resolution);
            }
            Outcome::Raised(value, trace) => {
                let (kind, message) = inst.describe_raised(value);
                eprint!("{}", render_raise(&kind, &message, trace.raised_at, view));
                return 1;
            }
            Outcome::Faulted(fault) => {
                eprintln!("doodle: {}", fault_message(fault));
                return 1;
            }
            // `RunToCompletion` with no breakpoints/host-pause never pauses; a pause here means a
            // violated engine invariant, reported rather than silently looped.
            Outcome::Paused(reason) => {
                eprintln!("doodle: internal error: unexpected pause ({reason:?})");
                return 1;
            }
        }
    }
}

/// Resolves one `import` request against the filesystem (E§6, S-7): the dotted `path` maps to a
/// `.doodle` file beside the **importing** module — its segments joined as directories under the
/// importer's own directory (the parent of its canonical id,
/// [`module_canonical_id`](Instance::module_canonical_id)), with `.doodle` on the last, so a module
/// in a subdirectory resolves its own siblings. A missing file is `NotFound`, which the
/// engine uses to drive the module-vs-member fallback (S-7: `import a.b` tries the module `a/b`
/// first, then member `b` of module `a`); a present-but-unreadable file raises at the `import`.
/// The resolved module's `canonical_id` is its normalized absolute path — the singleton-load key
/// (L§11.3), so two import paths reaching one file load it once.
fn resolve_import_request(inst: &mut Instance, request: &ImportRequest) -> ImportResolution {
    let Some(importer_id) = inst.module_canonical_id(ModuleId(request.importer)) else {
        // An importer with no recorded canonical id should not occur; treat it as unresolvable
        // rather than guessing a directory.
        return ImportResolution::NotFound;
    };
    let importer_id = importer_id.to_string();
    let dir = Path::new(&importer_id)
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let mut file = dir.to_path_buf();
    for segment in &request.path {
        file.push(segment);
    }
    file.set_extension("doodle");

    match std::fs::read_to_string(&file) {
        Ok(text) => {
            let canonical_id = std::fs::canonicalize(&file)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| file.to_string_lossy().into_owned());
            ImportResolution::Source { text, canonical_id }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ImportResolution::NotFound,
        Err(e) => raise_import_read_error(inst, &file, &e),
    }
}

/// A read error on a module file that *does* exist (permissions, not-a-file): raise it at the
/// `import` site rather than reporting `NotFound`, which would wrongly trigger the member fallback.
fn raise_import_read_error(
    inst: &mut Instance,
    file: &Path,
    error: &std::io::Error,
) -> ImportResolution {
    ImportResolution::Raise(raise_string(
        inst,
        &format!("cannot read module `{}`: {error}", file.display()),
    ))
}

/// Writes the `print` output produced since the last flush to stdout, advancing the cursor. The
/// engine's output sink is the full accumulated buffer; this streams only the new tail.
fn flush_output(inst: &Instance, flushed: &mut usize) {
    let out = inst.output();
    if out.len() > *flushed {
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(&out[*flushed..]);
        let _ = stdout.flush();
        *flushed = out.len();
    }
}

/// Resolves one capability request (E§7.5): `read_line` from a line of stdin, `time` from the wall
/// clock, `random` from `rng`. The request's argument handles are host-owned and released here
/// (S-17); these capabilities take none, but a stray one is freed rather than leaked.
fn resolve_capability(
    inst: &mut Instance,
    request: &CapabilityRequest,
    rng: &mut Rng,
) -> Resolution {
    for &handle in &request.args {
        let _ = inst.release(handle);
    }
    match request.capability.0 {
        CAP_READ_LINE => resolve_read_line(inst),
        CAP_TIME => Resolution::Value(inst.make_float(wall_clock_seconds())),
        CAP_RANDOM => Resolution::Value(inst.make_float(rng.next_f64())),
        // Only the three ambient capabilities suspend (print is synchronous), so this is
        // unreachable in practice; a raise keeps a mis-registration from wedging the drive.
        other => Resolution::Raise(raise_string(
            inst,
            &format!("unresolved capability {other}"),
        )),
    }
}

/// Resolves `read_line` (E§5.3): one line of stdin as a string with its trailing newline stripped;
/// end-of-input raises `"end of input"` so the program can handle it, and a read error raises its
/// message.
fn resolve_read_line(inst: &mut Instance) -> Resolution {
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) => Resolution::Raise(raise_string(inst, "end of input")),
        Ok(_) => {
            let trimmed = line.strip_suffix('\n').unwrap_or(&line);
            let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
            Resolution::Value(
                inst.make_string(trimmed.as_bytes())
                    .expect("a stdin line is valid UTF-8 text"),
            )
        }
        Err(e) => Resolution::Raise(raise_string(inst, &format!("read error: {e}"))),
    }
}

/// A host-owned string handle for a [`Resolution::Raise`] message.
fn raise_string(inst: &mut Instance, message: &str) -> doodle_core::machine::Handle {
    inst.make_string(message.as_bytes())
        .expect("a diagnostic message is valid UTF-8 text")
}

/// The current wall-clock time as seconds since the Unix epoch (the `time` capability's value,
/// D-M7-16). A clock reading before the epoch (a mis-set clock) reads as `0.0`.
fn wall_clock_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The default `random` seed when `--seed` is absent: nanoseconds since the epoch (entropy-ish, and
/// non-reproducible by design — a recorded run replays from its resolutions, not by re-seeding).
fn entropy_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// A human message for an engine fault (E§9/§10) shown on stderr.
fn fault_message(fault: EngineFault) -> String {
    match fault {
        EngineFault::LimitExceeded(kind) => {
            let what = match kind {
                LimitKind::StepBudget => "step budget",
                LimitKind::Heap => "memory limit",
                LimitKind::StackDepth => "call-stack depth limit",
                LimitKind::TailHistory => "tail-call history limit",
                LimitKind::OpResult => "single-operation size limit",
            };
            format!("the program exceeded its {what}")
        }
        EngineFault::Cancelled => "the program was cancelled".to_string(),
        EngineFault::NestedSuspend => {
            "a capability (input, time, random, drawing) was used inside a native block, which is \
             not supported"
                .to_string()
        }
        EngineFault::Internal => "internal engine error".to_string(),
    }
}

/// The rendered error-severity diagnostics (E§3.1), or `None` if `diagnostics` has no error. A
/// warning alone does not fail a load, so it is not rendered here.
fn render_errors(diagnostics: &[Diagnostic], view: &SourceView<'_>) -> Option<String> {
    let errors: Vec<Diagnostic> = diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .cloned()
        .collect();
    (!errors.is_empty()).then(|| render_diagnostics(&errors, view))
}
