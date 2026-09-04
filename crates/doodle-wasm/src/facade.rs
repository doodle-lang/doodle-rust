//! The **native-testable core** of the wasm facade (engine spec E§3–§8): a thin session
//! over a `doodle-core` [`Instance`] that owns the front-end pipeline (source → loaded
//! instance), the fuel-sliced drive/resolve loop, the handle boundary, and the output/
//! position/cancel surfaces. It deals only in plain Rust types (no `JsValue`), so it is
//! exercised by ordinary `cargo test`; the `#[wasm_bindgen]` layer in `lib.rs` is a thin
//! marshaling shell over it (Decision #1 keeps `doodle-rust` free of a JS test harness —
//! the Node smoke lives in `doodle-web`, M3.5).
//!
//! Values cross as **opaque handle ids** (a `Handle`'s `u64` bits), never raw pointers
//! (E§4). Host code participates only through the **suspend/resolve capability** path
//! (the public intrinsic API registers engine-provided natives, not JS callbacks): drive
//! until [`DriveOutcome::Suspended`], read + release the request's argument handles, do
//! the host-side work, then [`resolve`](Session::resolve).

use doodle_core::diag::Severity;
use doodle_core::drive::{
    Directive, EngineFault, LimitKind, Limits, Outcome, PauseReason, Resolution, resolve_slice,
    run_slice,
};
use doodle_core::machine::{
    Handle, HandleError, Instance, Kind, Registry, ValueError, clear_canvas_intrinsic,
    cos_intrinsic, decode_intrinsic, draw_line_intrinsic, each_intrinsic, encode_intrinsic,
    length_intrinsic, print_intrinsic, random_intrinsic, read_line_intrinsic, set_turtle_intrinsic,
    sin_intrinsic, time_intrinsic,
};
use doodle_core::parse::parse_program;
use doodle_core::resolve::resolve as resolve_module;
use doodle_core::source::normalize;
use doodle_core::span::{ModuleId, Span};

/// The debug observation surface (E§8) on the session — breakpoints, raise-trap, observation
/// mode, the stack walk + lazy bindings, the trapped raise, and auxiliary evaluation.
mod debug;
/// The structural value-inspection surface (E§4.4/§8.4) on the session — record/dict/list
/// fields and callable/type/module reflection, mirroring the native `Instance` API 1:1.
mod inspect;

pub use debug::{AuxOutcomeData, CallableInfo, FrameData, GlobalBindingData, StaleGeneration};

/// The M3 turtle library, prepended to a turtle program as one module (the real module
/// system is M5). Lives in `doodle-rust`; `doodle-web` reaches it only through the wasm
/// facade's `turtle` constructor.
const TURTLE_LIBRARY: &str = include_str!("../../../doodle/turtle.doodle");

/// The playground's entry-module canonical id (E§3.2): the single file the user is editing. The
/// engine has no magic default, so the browser host names its entry explicitly (D-M7-17); it
/// surfaces as [`entry_module`](Session::entry_module), which doodle-web feeds to breakpoints. It
/// does not appear in program output, so the conformance-parity `demo`/`turtle` configs stay
/// bit-identical to the native runner regardless of this id (E§11).
const ENTRY_MODULE_PATH: &str = "playground";

/// Resource limits for the public browser demo (E§10.2), one per rail: **space** (a 64 MiB heap,
/// vast headroom for kid turtle programs, KB–MB), **total work** (a large step budget, so a long
/// animation is paced by the stop button and slice fuel, never cut off), and **single-op latency**
/// (a 1 MiB per-operation result cap). The latency cap is the demo's guard against a pathological
/// `**`/`*`/repetition: a bignum whose result fits the heap but would take seconds to compute (an
/// atomic op cannot be interrupted, S-40) faults `LimitExceeded(OpResult)` **before** computing,
/// bounding the worst single-op stall to a fraction of a second rather than freezing the tab.
const DEMO_LIMITS: Limits = Limits {
    step_budget: 1 << 40,
    heap_bytes: 64 << 20,
    stack_depth: 100_000,
    max_op_result_bytes: 1 << 20,
};

/// A loaded engine session: a `doodle-core` [`Instance`] plus the exact source it was
/// loaded from (so positions map back to source, and the prelude offset is known).
pub struct Session {
    instance: Instance,
    /// The full module source actually parsed (prelude + program), NFC-normalized.
    source: String,
    /// Byte length of the prepended prelude, so a position can be reported relative to the
    /// user's program (0 when nothing was prepended).
    prelude_bytes: u32,
    /// The **pause generation** (E§8, debug surface): bumped on every [`drive`](Self::drive)/
    /// [`resolve`](Self::resolve) so a [`stack_walk`](Self::stack_walk)'s frame indices are only
    /// valid until the next state advance. A lazy [`frame_local`](Self::frame_local)/
    /// [`frame_dynamic`](Self::frame_dynamic) read carrying a stale generation is a clean error,
    /// never a wrong answer read against a different stack. Auxiliary evaluation
    /// ([`eval_to_string`](Self::eval_to_string)) does **not** bump it — it restores the pause,
    /// leaving the walk valid.
    pause_gen: u32,
    /// The entry module's canonical id (E§3.2) — how the host addresses it for breakpoints
    /// (E§8.6). A single editor buffer has no filename, so this is the engine default (`main`);
    /// a host that loads named files would thread the name through here.
    module_path: String,
}

/// Why loading a program failed before it could run (a front-end error, E§3.1). Carries
/// the rendered diagnostics for the host to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    /// One human-readable line per error diagnostic.
    pub message: String,
}

/// The facade's plain-Rust view of a drive [`Outcome`] (engine spec E§7.2), string-tagged
/// so the marshaling layer forwards it without pulling in a serializer (size, §6.5). A
/// module drive `Completed` carries no value (Void); a returning `fn`'s value is reached
/// through the result register (not surfaced here at M3).
#[derive(Debug, Clone, PartialEq)]
pub enum DriveOutcome {
    /// The driven unit finished (module drive ⇒ Void).
    Completed,
    /// A capability must be fulfilled: its registry id and the bound-argument handles
    /// (**host-owned** — the caller reads and must [`release`](Session::release) each).
    Suspended { capability: u32, args: Vec<Handle> },
    /// Stopped at a safe point (`"slice-end"`, `"step"`, `"breakpoint"`, `"raise-trap"`,
    /// `"host-pause"`); `"slice-end"` is the pump's resumable yield.
    Paused(&'static str),
    /// An uncaught Doodle exception reached the boundary (E§9): the raised value's
    /// described `kind`/`message` (an `Error` record's own fields, or `"raised"` for any
    /// other value).
    Raised {
        /// The exception kind, kebab-cased (the `Error`'s `kind`, or `"raised"`).
        kind: String,
        /// The exception message.
        message: String,
        /// The raising site's byte span in the module source, if known.
        span: Option<Span>,
    },
    /// A non-resumable engine fault, kebab-cased (`"limit:step-budget"`, `"cancelled"`,
    /// `"nested-suspend"`, `"internal"`, …).
    Faulted(&'static str),
}

impl Session {
    /// Loads `program` with the **demo** registry — the portable conformance manifest (D-M7-21,
    /// M7.5b), nothing prepended. This is the conformance-parity configuration (identical namespace
    /// and registration order ⇒ identical resolution + capability ids across surfaces, E§11).
    pub fn demo(program: &str) -> Result<Self, LoadError> {
        let mut registry = Registry::new();
        // The portable conformance manifest, in the fixed registration order the native runner
        // (`conformance-runner`'s `capability` module) and the C host also install. Registration
        // order is a capability's replay identity (E§5.5/§11), so all three surfaces must match:
        // print(0), length(1), each(2), encode(3), decode(4), read_line(5), time(6), random(7).
        for intrinsic in [
            print_intrinsic(),
            length_intrinsic(),
            each_intrinsic(),
            encode_intrinsic(),
            decode_intrinsic(),
            read_line_intrinsic(),
            time_intrinsic(),
            random_intrinsic(),
        ] {
            registry
                .register(intrinsic)
                .expect("a demo intrinsic registers cleanly");
        }
        Self::load(program, "", registry)
    }

    /// Loads `program` with the **turtle** registry (`print`/`sin`/`cos` + the three
    /// platform primitives `draw_line`/`set_turtle`/`clear_canvas`, in that fixed
    /// registration order so capability ids are stable, E§11) and the turtle library
    /// prepended as one module. The M3 demo configuration.
    pub fn turtle(program: &str) -> Result<Self, LoadError> {
        let mut registry = Registry::new();
        for intrinsic in [
            print_intrinsic(),
            sin_intrinsic(),
            cos_intrinsic(),
            draw_line_intrinsic(),
            set_turtle_intrinsic(),
            clear_canvas_intrinsic(),
        ] {
            registry
                .register(intrinsic)
                .expect("turtle natives register cleanly");
        }
        Self::load(program, TURTLE_LIBRARY, registry)
    }

    /// Runs the front-end pipeline (normalize → parse → resolve, E§3.1) over `prelude`
    /// concatenated with `program`, failing on any `Severity::Error` diagnostic, then
    /// loads the resolved module with `registry`.
    fn load(program: &str, prelude: &str, registry: Registry) -> Result<Self, LoadError> {
        // The prelude and program join as one module; a newline guarantees the program's
        // first line is not glued onto the prelude's last.
        let combined = if prelude.is_empty() {
            program.to_string()
        } else {
            format!("{prelude}\n{program}")
        };
        let source = normalize(&combined).into_owned();
        let prelude_bytes = if prelude.is_empty() {
            0
        } else {
            // The prelude's normalized byte length, plus the joining newline.
            (source.len() - normalize(program).len()) as u32
        };

        let parsed = parse_program(&source, ModuleId(0));
        if let Some(err) = errors_of(&parsed.diagnostics) {
            return Err(err);
        }
        let resolved = resolve_module(parsed.ast, parsed.root, ModuleId(0));
        if let Some(err) = errors_of(&resolved.diagnostics) {
            return Err(err);
        }
        Ok(Session {
            instance: Instance::load(resolved.module, DEMO_LIMITS, registry, ENTRY_MODULE_PATH),
            source,
            prelude_bytes,
            pause_gen: 0,
            module_path: ENTRY_MODULE_PATH.to_string(),
        })
    }

    /// The entry module's canonical id (E§3.2): the string a host passes to
    /// [`set_breakpoint`](Self::set_breakpoint) to address the program the user is editing.
    pub fn entry_module(&self) -> &str {
        &self.module_path
    }

    /// The current pause generation (see [`pause_gen`](Session::pause_gen)) — stamped into a
    /// [`stack_walk`](Self::stack_walk) so a later [`frame_local`](Self::frame_local)/
    /// [`frame_dynamic`](Self::frame_dynamic) can prove it reads the same stopped state.
    pub fn pause_generation(&self) -> u32 {
        self.pause_gen
    }

    /// Drives the session under `directive` (E§7.3) for at most `fuel` statement safe points
    /// (`None` = unbounded, S-40), starting or resuming from a `Ready`/`Paused` state. The
    /// pump passes [`RunToCompletion`](Directive::RunToCompletion) and slices on fuel; the
    /// debugger passes `Continue`/`Step*` and stops at breakpoints/raise-traps/steps. Bumps the
    /// pause generation, invalidating any prior [`stack_walk`](Self::stack_walk)'s frame indices.
    pub fn drive(&mut self, directive: Directive, fuel: Option<u64>) -> DriveOutcome {
        self.pause_gen = self.pause_gen.wrapping_add(1);
        let outcome = run_slice(&mut self.instance, directive, fuel);
        to_drive_outcome(&self.instance, outcome)
    }

    /// Resolves a pending capability with a host value (or a raise), then resumes the drive for
    /// at most `fuel` safe points (E§7.5). The handle is the host's resolution value; `raise`
    /// chooses whether it surfaces as the call's result or is raised at the call site. The
    /// resume runs under the **directive in force** when the instance suspended (E§7.3) — so a
    /// step across a suspending capability keeps stepping — not a fresh one. Bumps the pause
    /// generation like [`drive`](Self::drive).
    pub fn resolve(&mut self, value: Handle, raise: bool, fuel: Option<u64>) -> DriveOutcome {
        self.pause_gen = self.pause_gen.wrapping_add(1);
        let resolution = if raise {
            Resolution::Raise(value)
        } else {
            Resolution::Value(value)
        };
        let outcome = resolve_slice(&mut self.instance, resolution, fuel);
        to_drive_outcome(&self.instance, outcome)
    }

    /// Requests cancellation (E§10.1); the next safe point faults `Cancelled`. In the
    /// single-threaded pump this is the stop button, checked between slices.
    pub fn cancel(&self) {
        self.instance.cancel_token().cancel();
    }

    /// The captured `print` output so far (E§5.2), deterministic.
    pub fn output(&self) -> &[u8] {
        self.instance.output()
    }

    /// The currently-executing byte span in the module source (E§8.1), or `None` at a
    /// boundary. The span indexes the full module (prelude + program); subtract
    /// [`prelude_bytes`](Self::prelude_bytes) for a program-relative offset.
    pub fn current_position(&self) -> Option<Span> {
        self.instance.current_position().map(|p| p.span)
    }

    /// The currently-executing span **in the user's program** (E§8.2): the innermost active
    /// call site at or past the prelude — the user line whose turtle command is running,
    /// even while the prepended library executes on top of it — else the top frame's own
    /// position when it is itself in the user program (a top-level statement between
    /// commands). `None` when nothing on the stack is in the user program. Drives a live
    /// line highlight; still module-relative, so subtract [`prelude_bytes`](Self::prelude_bytes).
    pub fn current_user_position(&self) -> Option<Span> {
        self.instance
            .call_site_spans()
            .into_iter()
            .find(|span| span.start >= self.prelude_bytes)
            .or_else(|| {
                self.instance
                    .current_position()
                    .map(|p| p.span)
                    .filter(|span| span.start >= self.prelude_bytes)
            })
    }

    /// Byte length of the prepended prelude (0 for the demo config), so a host can map a
    /// module-relative position back to the user's program.
    pub fn prelude_bytes(&self) -> u32 {
        self.prelude_bytes
    }

    /// The module source actually loaded (prelude + program), for host-side span→line
    /// mapping.
    pub fn source(&self) -> &str {
        &self.source
    }

    // --- the handle boundary (E§4): make / read / retain / release ---

    pub fn make_int(&mut self, value: i64) -> Handle {
        self.instance.make_int(value)
    }
    pub fn make_int_decimal(&mut self, decimal: &str) -> Result<Handle, ValueError> {
        self.instance.make_int_decimal(decimal)
    }
    pub fn make_float(&mut self, value: f64) -> Handle {
        self.instance.make_float(value)
    }
    pub fn make_bool(&mut self, value: bool) -> Handle {
        self.instance.make_bool(value)
    }
    pub fn make_nil(&mut self) -> Handle {
        self.instance.make_nil()
    }
    pub fn make_string(&mut self, bytes: &[u8]) -> Result<Handle, ValueError> {
        self.instance.make_string(bytes)
    }
    pub fn as_int(&self, handle: Handle) -> Result<i64, ValueError> {
        self.instance.as_int(handle)
    }
    pub fn as_int_decimal(&self, handle: Handle) -> Result<String, ValueError> {
        self.instance.as_int_decimal(handle)
    }
    pub fn as_float(&self, handle: Handle) -> Result<f64, ValueError> {
        self.instance.as_float(handle)
    }
    pub fn as_bool(&self, handle: Handle) -> Result<bool, ValueError> {
        self.instance.as_bool(handle)
    }
    pub fn is_nil(&self, handle: Handle) -> Result<bool, ValueError> {
        self.instance.is_nil(handle)
    }
    pub fn string_bytes(&self, handle: Handle) -> Result<&[u8], ValueError> {
        self.instance.string_bytes(handle)
    }
    pub fn kind_of(&self, handle: Handle) -> Result<Kind, ValueError> {
        self.instance.kind_of(handle)
    }
    pub fn release(&mut self, handle: Handle) -> Result<(), HandleError> {
        self.instance.release(handle)
    }
}

/// Collects the `Severity::Error` diagnostics into a [`LoadError`], or `None` if the unit
/// loaded clean (warnings do not block a load, E§3.1).
fn errors_of(diagnostics: &[doodle_core::diag::Diagnostic]) -> Option<LoadError> {
    let lines: Vec<String> = diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| match d.span {
            Some(span) => format!("[{}..{}] {}", span.start, span.end, d.message),
            None => d.message.clone(),
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(LoadError {
        message: lines.join("\n"),
    })
}

/// Maps a drive [`Outcome`] to the facade's string-tagged [`DriveOutcome`]. Takes the
/// `instance` so a `Raised` value can be described (its `Error` `kind`/`message`, E§9).
fn to_drive_outcome(instance: &Instance, outcome: Outcome) -> DriveOutcome {
    match outcome {
        Outcome::Completed(_) => DriveOutcome::Completed,
        Outcome::Suspended(request) => DriveOutcome::Suspended {
            capability: request.capability.0,
            args: request.args,
        },
        // The browser host does not load modules dynamically yet (the demo bundles all
        // source): a program that imports an unloaded module has no resolver here. Surfacing
        // it as a fault keeps the JS API unchanged; the first-class `SuspendedImport` outcome
        // + a JS `resolve_import` binding + demo fetch are M5-web work (E§6).
        Outcome::SuspendedImport(_) => DriveOutcome::Faulted("import-unsupported"),
        Outcome::Paused(reason) => DriveOutcome::Paused(pause_tag(reason)),
        Outcome::Raised(value, trace) => {
            let (kind, message) = instance.describe_raised(value);
            DriveOutcome::Raised {
                kind,
                message,
                span: trace.raised_at,
            }
        }
        Outcome::Faulted(fault) => DriveOutcome::Faulted(fault_tag(fault)),
    }
}

fn pause_tag(reason: PauseReason) -> &'static str {
    match reason {
        PauseReason::Step => "step",
        PauseReason::Breakpoint(_) => "breakpoint",
        PauseReason::RaiseTrap => "raise-trap",
        PauseReason::HostPause => "host-pause",
        PauseReason::SliceEnd => "slice-end",
    }
}

fn fault_tag(fault: EngineFault) -> &'static str {
    match fault {
        EngineFault::LimitExceeded(LimitKind::StepBudget) => "limit:step-budget",
        EngineFault::LimitExceeded(LimitKind::Heap) => "limit:heap",
        EngineFault::LimitExceeded(LimitKind::StackDepth) => "limit:stack-depth",
        EngineFault::LimitExceeded(LimitKind::TailHistory) => "limit:tail-history",
        EngineFault::LimitExceeded(LimitKind::OpResult) => "limit:op-result",
        EngineFault::Cancelled => "cancelled",
        EngineFault::NestedSuspend => "nested-suspend",
        EngineFault::Internal => "internal",
    }
}

#[cfg(test)]
mod tests;
