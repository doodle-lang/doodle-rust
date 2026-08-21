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
    Directive, EngineFault, LimitKind, Outcome, PauseReason, Resolution, resolve_slice, run_slice,
};
use doodle_core::machine::{
    ExceptionKind, Handle, HandleError, Instance, Kind, Registry, ValueError,
    clear_canvas_intrinsic, cos_intrinsic, draw_line_intrinsic, print_intrinsic,
    set_turtle_intrinsic, sin_intrinsic,
};
use doodle_core::parse::parse_program;
use doodle_core::resolve::resolve as resolve_module;
use doodle_core::source::normalize;
use doodle_core::span::{ModuleId, Span};

/// The M3 turtle library, prepended to a turtle program as one module (the real module
/// system is M5). Lives in `doodle-rust`; `doodle-web` reaches it only through the wasm
/// facade's `turtle` constructor.
const TURTLE_LIBRARY: &str = include_str!("../../../doodle/turtle.doodle");

/// A loaded engine session: a `doodle-core` [`Instance`] plus the exact source it was
/// loaded from (so positions map back to source, and the prelude offset is known).
pub struct Session {
    instance: Instance,
    /// The full module source actually parsed (prelude + program), NFC-normalized.
    source: String,
    /// Byte length of the prepended prelude, so a position can be reported relative to the
    /// user's program (0 when nothing was prepended).
    prelude_bytes: u32,
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
    /// An uncaught Doodle exception reached the boundary.
    Raised {
        /// The exception kind, kebab-cased.
        kind: &'static str,
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
    /// Loads `program` with the **demo** registry — only `print`, matching the native
    /// conformance runner (S-43), and nothing prepended. This is the conformance-parity
    /// configuration (identical namespace ⇒ identical resolution, E§11).
    pub fn demo(program: &str) -> Result<Self, LoadError> {
        let mut registry = Registry::new();
        registry
            .register(print_intrinsic())
            .expect("print registers cleanly");
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
            instance: Instance::load_with_intrinsics(resolved.module, registry),
            source,
            prelude_bytes,
        })
    }

    /// Drives the session at most `fuel` statement safe points (`None` = unbounded, S-40),
    /// starting or resuming from a `Paused`; the pump's slice step.
    pub fn drive(&mut self, fuel: Option<u64>) -> DriveOutcome {
        let outcome = run_slice(&mut self.instance, Directive::RunToCompletion, fuel);
        to_drive_outcome(outcome)
    }

    /// Resolves a pending capability with a host value (or a raise), then resumes the
    /// drive for at most `fuel` safe points (E§7.5). The handle is the host's resolution
    /// value; `raise` chooses whether it surfaces as the call's result or is raised at
    /// the call site.
    pub fn resolve(&mut self, value: Handle, raise: bool, fuel: Option<u64>) -> DriveOutcome {
        let resolution = if raise {
            Resolution::Raise(value)
        } else {
            Resolution::Value(value)
        };
        let outcome = resolve_slice(&mut self.instance, resolution, fuel);
        to_drive_outcome(outcome)
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

/// Maps a drive [`Outcome`] to the facade's string-tagged [`DriveOutcome`].
fn to_drive_outcome(outcome: Outcome) -> DriveOutcome {
    match outcome {
        Outcome::Completed(_) => DriveOutcome::Completed,
        Outcome::Suspended(request) => DriveOutcome::Suspended {
            capability: request.capability.0,
            args: request.args,
        },
        Outcome::Paused(reason) => DriveOutcome::Paused(pause_tag(reason)),
        Outcome::Raised(exception, trace) => DriveOutcome::Raised {
            kind: exception_tag(exception.kind),
            message: exception.message,
            span: trace.raised_at,
        },
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
        EngineFault::Cancelled => "cancelled",
        EngineFault::NestedSuspend => "nested-suspend",
        EngineFault::Internal => "internal",
    }
}

fn exception_tag(kind: ExceptionKind) -> &'static str {
    match kind {
        ExceptionKind::TypeMismatch => "type-mismatch",
        ExceptionKind::DivisionByZero => "division-by-zero",
        ExceptionKind::NonFiniteFloat => "non-finite-float",
        ExceptionKind::UndefinedOrdering => "undefined-ordering",
        ExceptionKind::ProcedureInExpression => "procedure-in-expression",
        ExceptionKind::NameNotDefined => "name-not-defined",
        ExceptionKind::UsedBeforeDefined => "used-before-defined",
        ExceptionKind::ExponentTooLarge => "exponent-too-large",
        ExceptionKind::NotCallable => "not-callable",
        ExceptionKind::ArgumentError => "argument-error",
        ExceptionKind::NoValueDestination => "no-value-destination",
        ExceptionKind::FunctionFellOffEnd => "function-fell-off-end",
        ExceptionKind::HostRaised => "host-raised",
    }
}

#[cfg(test)]
mod tests;
