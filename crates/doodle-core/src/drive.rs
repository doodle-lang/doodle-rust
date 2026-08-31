//! The drive loop: [`Outcome`]s and the [`run`] entry point (engine spec E§7).
//!
//! M2a.2: [`run`] advances the real machine one [`step`](Instance::step) at a
//! time to completion. The demo subset has no capabilities, breakpoints, or safe
//! points yet, so every directive runs straight to `Completed`; the other
//! outcomes' payloads (suspension, raise, fault) are shells the later M2a/M2b
//! chunks fill in.

use crate::machine::{Halt, Handle, Instance, InstanceState, Trace, Value};

mod config;
pub use config::{Config, ConfigError, LimitKind, Limits, ObservationMode};

/// A driving directive: how far to run before returning to the host (E§7.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Directive {
    /// Stop only on Suspended / Raised / Faulted / Completed (a fast run).
    RunToCompletion,
    /// Additionally stop on the next breakpoint or raise-trap.
    Continue,
    /// Stop at the next safe point, in any frame.
    Step,
    /// Step, descending into calls.
    StepInto,
    /// Step, treating a call as a single step.
    StepOver,
    /// Run until the current frame returns.
    StepOut,
}

/// The result of driving an instance (engine spec E§7.2).
#[derive(Clone, Debug)]
pub enum Outcome {
    /// The driven unit finished; the value is present for a `fn` result and
    /// absent for Void (a `to` result, L§6.11).
    Completed(Option<Value>),
    /// A capability must be fulfilled by the host before execution continues.
    Suspended(CapabilityRequest),
    /// An `import` reached a module the engine has not loaded and that is not a registered
    /// native module (E§6, S-60): the host must supply its source (or report it missing)
    /// before execution continues. Resolve with [`resolve_import`]. Modeled as a
    /// suspension so a host that fetches source asynchronously (a browser) and one that
    /// bundles it (resolves immediately) obey the same contract.
    SuspendedImport(ImportRequest),
    /// Execution stopped at a safe point for observation.
    Paused(PauseReason),
    /// An uncaught exception reached the boundary (E§9): the raised **value** (an
    /// `Error` record for an engine raise, or any value a program `raise`d) and its
    /// trace. Describe it for display with [`Instance::describe_raised`].
    Raised(Value, Trace),
    /// A limit, cancellation, nested-suspend, or internal fault stopped execution.
    Faulted(EngineFault),
}

/// Why the engine stopped at a safe point (engine spec E§7.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PauseReason {
    /// The active step directive reached its next safe point.
    Step,
    /// A breakpoint was hit.
    Breakpoint(BreakpointId),
    /// A raise was trapped before propagating (E§8.7).
    RaiseTrap,
    /// The host requested a pause.
    HostPause,
    /// The drive call's **bounded-run fuel** was spent (S-40, E§7.3): a resumable
    /// slice boundary, distinct from a host `HostPause` request — the pump's yield
    /// point. Re-drive (`run`/`run_slice`) to continue; state is intact.
    SliceEnd,
}

/// Identifies an installed breakpoint.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct BreakpointId(pub u32);

/// A non-resumable engine fault (engine spec E§7.2, §10).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EngineFault {
    /// A configured limit was exceeded.
    LimitExceeded(LimitKind),
    /// The host cancelled the drive.
    Cancelled,
    /// A suspending capability was reached **inside a native block-consumer's reentrant
    /// drive** (E§5.4/§5.3, S-15). The nested drive runs on the host's Rust stack, so the
    /// native consumer's in-progress state (a loop index, say) cannot be frozen and
    /// resumed — the engine **forbids** the nested suspend as a terminal, deterministic
    /// fault (Decision #2, "forbid-and-fault"). This is distinct from [`Internal`](Self::Internal):
    /// it reports a well-defined engine limitation reached by legitimate Doodle code, not
    /// a violated invariant. The alternative — *suspending the outer drive* by making
    /// native consumers resumable — is the deferred M7 C-ABI foreign-function-yield
    /// extension (E§5.4/§7.6; machine-design §14).
    NestedSuspend,
    /// An internal invariant was violated.
    Internal,
}

/// Identifies the registered capability a [`CapabilityRequest`] is for (engine spec
/// E§7.5). Its value is the capability's registration index — **stable across runs**
/// (S-43 registration order is replay-identity input), so a host matches a request to
/// the capability it registered and a recording replays deterministically (E§11).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct CapabilityId(pub u32);

/// A capability request carried by [`Outcome::Suspended`] (engine spec E§7.5): which
/// capability was called and its bound arguments as **host-owned** handles (S-17 — the
/// host reads them and must [`release`](Instance::release) them). The host fulfils the
/// request and continues with [`resolve`].
#[derive(Clone, Debug)]
pub struct CapabilityRequest {
    /// Which registered capability was called.
    pub capability: CapabilityId,
    /// Its bound arguments (positional order), each a fresh host-owned handle.
    pub args: Vec<Handle>,
}

/// How a host resolves a [`Suspended`](Outcome::Suspended) capability (E§7.3): the
/// value the capability produced (which becomes the call's result) or an exception to
/// raise at the call site. The suspend→resolve mechanism is wired at M2b.4; the
/// [`resolve`] entry and its phase guard land now (M2b.3).
#[derive(Clone, Copy, Debug)]
pub enum Resolution {
    /// The capability's result value (E§7.5).
    Value(Handle),
    /// Raise this value at the capability call site (E§7.5).
    Raise(Handle),
}

/// An import request carried by [`Outcome::SuspendedImport`] (engine spec E§6): the
/// module the program asked for, as its dotted-path segments (the request identity), plus
/// the importing module's id for host diagnostics. The host maps the path to source and
/// continues with [`resolve_import`].
#[derive(Clone, Debug)]
pub struct ImportRequest {
    /// The requested dotted module path, one entry per segment (e.g. `["shapes", "circle"]`).
    pub path: Vec<String>,
    /// The importing module's canonical id ([`ModuleId`](crate::span::ModuleId) value) —
    /// for the host's own diagnostics; the engine needs only the path to be resolved.
    pub importer: u32,
}

/// How a host resolves a [`SuspendedImport`](Outcome::SuspendedImport) (engine spec E§6):
/// the module's source, a "not found", or an exception to raise at the `import` site. A
/// bundling host answers with `Source` immediately in its drive loop; a host that fetches
/// over the network answers when the source arrives (or fails). Import resolutions are
/// recorded and replayed like capability resolutions (E§11).
#[derive(Clone, Debug)]
pub enum ImportResolution {
    /// The module's source `text` and the host's `canonical_id` for it (E§6): the engine
    /// parses the text, drives its top level, and caches it under the canonical id
    /// (singleton loading, L§11.3 — two paths the host maps to one canonical id load once).
    Source {
        /// The module's source text (the engine NFC-normalizes it, L§4.4).
        text: String,
        /// The host's canonical identity for the module — the singleton cache key.
        canonical_id: String,
    },
    /// The module does not exist: the engine raises `module-not-found` in the importer
    /// (E§6, S-60).
    NotFound,
    /// The host could not supply the source (e.g. a failed fetch): the engine raises the
    /// given value at the `import` site (E§6).
    Raise(Handle),
}

/// Starts (from `Ready`) or continues (after a `Paused`) driving `instance` under
/// `directive`, returning an [`Outcome`] (E§7.3).
///
/// `RunToCompletion` and `Continue` run without a `Step*` pause (the breakpoints and
/// raise-trap that make `Continue` distinct are M6); a `Step*` directive pauses at the
/// next statement-level safe point selected by frame depth (E§8.5) →
/// [`Paused`](Outcome::Paused)`(`[`PauseReason::Step`]`)`. A host pause requested through a
/// [`PauseToken`](crate::machine::PauseToken) stops **any** directive at the next safe point
/// → [`Paused`](Outcome::Paused)`(`[`PauseReason::HostPause`]`)`, with state resumable
/// (E§8.8) — re-drive to continue. An uncaught raise leaves
/// the instance `Raised`, an engine fault leaves it `Faulted`, and completion leaves
/// it `Completed` (E§3.3 outcome↔state correspondence). Re-driving a terminal
/// instance, or `run`-ing a `Suspended` one (use [`resolve`]), is a host-contract
/// violation — debug-asserted, and a returned `Faulted(Internal)` in release rather
/// than undefined behavior.
pub fn run(instance: &mut Instance, directive: Directive) -> Outcome {
    run_slice(instance, directive, None)
}

/// Like [`run`], but **bounded**: runs at most `fuel` statement safe points (S-40, AD6)
/// before returning [`Paused`](Outcome::Paused)`(`[`PauseReason::SliceEnd`]`)`, the
/// pump's yield point. `None` fuel is unbounded (identical to [`run`]). The drive stops
/// earlier for any other reason (completion, raise, suspend, a `Step*` pause, a fault);
/// slice size never changes *what* executes, only where it yields (E§7.7).
pub fn run_slice(instance: &mut Instance, directive: Directive, fuel: Option<u64>) -> Outcome {
    debug_assert!(
        matches!(
            instance.state(),
            InstanceState::Ready | InstanceState::Paused
        ),
        "run() requires a Ready or Paused instance (got {:?}); use resolve() after \
         Suspended, and terminal states are not re-drivable (E§3.3/§7.3)",
        instance.state()
    );
    if !matches!(
        instance.state(),
        InstanceState::Ready | InstanceState::Paused
    ) {
        return Outcome::Faulted(EngineFault::Internal);
    }
    drive(instance, directive, fuel)
}

/// Continues a `Suspended` `instance` with the host's `resolution` (E§7.3/§7.5): the
/// value the capability produced becomes the call's result and the drive resumes under
/// the directive in force; a raise surfaces at the capability call site (leaving the
/// instance `Raised`). `resolve` on an instance with no pending suspension is a
/// host-contract violation (debug-asserted; `Faulted(Internal)` in release).
pub fn resolve(instance: &mut Instance, resolution: Resolution) -> Outcome {
    resolve_slice(instance, resolution, None)
}

/// Like [`resolve`], but **bounded**: the resumed drive runs at most `fuel` statement
/// safe points before `Paused(SliceEnd)` (S-40) — the pump resolves a capability and
/// resumes one slice. `None` is unbounded (identical to [`resolve`]).
pub fn resolve_slice(
    instance: &mut Instance,
    resolution: Resolution,
    fuel: Option<u64>,
) -> Outcome {
    // A capability suspension is `Suspended` **and not** import-suspended: an import-suspended
    // instance is also `Suspended`, so without the second half `resolve()` (the capability
    // entry) would proceed into `take_capability` and hit its `unreachable!`. Use
    // `resolve_import()` for an import — a mismatched call is a host-contract violation
    // (debug-asserted; `Faulted(Internal)` in release, never a panic).
    let capability_suspended =
        matches!(instance.state(), InstanceState::Suspended) && !instance.is_import_suspended();
    debug_assert!(
        capability_suspended,
        "resolve() requires an instance suspended on a capability (got {:?}, import={}); use \
         resolve_import() for an import (E§7.3/§7.5)",
        instance.state(),
        instance.is_import_suspended()
    );
    if !capability_suspended {
        return Outcome::Faulted(EngineFault::Internal);
    }
    match resolution {
        // The capability's value becomes the call's result; resume the drive so the
        // caller's waiting continuation consumes it, under the directive in force. A
        // cancellation requested while suspended is reaped here too: the resumed drive's
        // next safe point observes it and faults `Cancelled` (or the program completes
        // first — a cancel racing completion loses, §10.1).
        Resolution::Value(handle) => match instance.resume_with_value(handle) {
            Ok(()) => drive(instance, instance.resume_directive(), fuel),
            // A stale resolution handle is a host-contract violation (a non-resumable
            // engine fault): the suspension was cleared, so leave the instance terminally
            // `Faulted` — a `Faulted` outcome always implies `state() == Faulted` (E§3.3).
            Err(_) => fault(instance),
        },
        // The host rejected the capability: arm a raise carrying the host's value at the
        // call site and re-drive, so it unwinds through the frames running cleanup and a
        // `try` around the capability call can catch it (L§12), or it drains to the
        // terminal `Raised` state (E§3.3) — **unless a cancellation is pending**: a cancel
        // with program work still ahead wins over a host raise (E§10.1, S-23), so discard
        // the rejection and tear the stack down to `Faulted(Cancelled)` instead.
        Resolution::Raise(handle) => {
            if instance.cancel_requested() {
                instance.discard_pending_and_cancel();
                return drive(instance, instance.resume_directive(), fuel);
            }
            match instance.resume_with_raise(handle) {
                Ok(()) => drive(instance, instance.resume_directive(), fuel),
                Err(_) => fault(instance),
            }
        }
    }
}

/// Continues a `SuspendedImport` `instance` with the host's module `resolution` (E§6): on
/// `Source` the engine parses the module, pushes its top-level frame, and resumes driving
/// (the importer stays parked beneath until the module finishes loading); `NotFound` raises
/// `module-not-found` and `Raise` raises the host's value, both at the `import` site.
/// Calling it on an instance not suspended on an import is a host-contract violation
/// (debug-asserted; `Faulted(Internal)` in release).
pub fn resolve_import(instance: &mut Instance, resolution: ImportResolution) -> Outcome {
    resolve_import_slice(instance, resolution, None)
}

/// Like [`resolve_import`], but **bounded**: the resumed drive runs at most `fuel` statement
/// safe points before `Paused(SliceEnd)` (S-40). `None` is unbounded.
pub fn resolve_import_slice(
    instance: &mut Instance,
    resolution: ImportResolution,
    fuel: Option<u64>,
) -> Outcome {
    debug_assert!(
        matches!(instance.state(), InstanceState::Suspended) && instance.is_import_suspended(),
        "resolve_import() requires an instance suspended on an import (got {:?}); use \
         resolve() for a capability (E§6/§7.5)",
        instance.state()
    );
    if !(matches!(instance.state(), InstanceState::Suspended) && instance.is_import_suspended()) {
        return Outcome::Faulted(EngineFault::Internal);
    }
    // A cancellation requested while suspended wins over the resolution (E§10.1, S-23),
    // exactly as for a capability: discard the parked import and tear down to
    // `Faulted(Cancelled)`.
    if instance.cancel_requested() {
        instance.discard_pending_and_cancel();
        return drive(instance, instance.resume_directive(), fuel);
    }
    match resolution {
        // The module's source: parse + push its top-level frame (or alias a canonical
        // duplicate / arm a compile-failure raise), then resume — the module's top level
        // drives to completion (observable, may itself suspend/raise) before the importer
        // continues.
        ImportResolution::Source { text, canonical_id } => {
            instance.load_import_source(&text, &canonical_id);
            drive(instance, instance.resume_directive(), fuel)
        }
        // The host did not find the requested path. A single-segment path raises
        // `module-not-found`; a multi-segment one falls back to a member import (S-7) and
        // resumes. Either way re-drive under the directive in force.
        ImportResolution::NotFound => {
            instance.resolve_import_not_found();
            drive(instance, instance.resume_directive(), fuel)
        }
        // The host could not supply the source: raise its value at the `import` site.
        ImportResolution::Raise(handle) => match instance.raise_import_value(handle) {
            Ok(()) => drive(instance, instance.resume_directive(), fuel),
            // A stale resolution handle is a host-contract violation: the suspension was
            // cleared, so leave the instance terminally `Faulted` (E§3.3).
            Err(_) => fault(instance),
        },
    }
}

/// The core drive loop: steps `instance` to a stopping [`Outcome`] under `directive`,
/// pausing a `Step*` at the next matching safe point and suspending at a capability
/// call. Shared by [`run`] and the resume side of [`resolve`].
fn drive(instance: &mut Instance, directive: Directive, fuel: Option<u64>) -> Outcome {
    // Anchor `Step*` depth judgments at the frame depth we resume from (E§8.5), remember
    // the directive so a `resolve` after a suspend resumes under it (E§7.3), and arm this
    // call's bounded-run fuel (S-40; `None` = unbounded).
    let anchor_depth = instance.frame_depth();
    instance.set_directive(directive);
    instance.arm_slice(fuel);
    instance.set_state(InstanceState::Running);
    loop {
        if instance.is_halted() {
            instance.set_state(InstanceState::Completed);
            // E§7.2: a top-level module drive completes with **no value** (Void) — a
            // module runs for effect (L§6.11). `Completed`'s value is present only for
            // a returning `fn` (a reentrant callable return, E§7.6, M2b.5).
            return Outcome::Completed(None);
        }
        // The drive call's slice fuel is spent (S-40): yield here, resumable — including
        // the `fuel == Some(0)` case (yield before running anything) and mid-drive
        // exhaustion (the previous step's safe point flagged it). Checked **after**
        // `is_halted`, so a program that finishes on the same safe point that spends the
        // last fuel still wins `Completed` at the exact boundary (completion is a fact
        // about past work; a slice end is a bound on future work — there is none left).
        if instance.sliced_out() {
            instance.set_state(InstanceState::Paused);
            return Outcome::Paused(PauseReason::SliceEnd);
        }
        // Raise-trap (E§8.7, S-18): a freshly-armed raise pauses here — **before** the next
        // `step` runs any unwind — so the debugger sees the raising frame with the stack
        // intact. Checked before `step` because a raise armed by the previous step (or by a
        // host `resolve(Raise)` before this drive) unwinds inside `step`; `take_raise_trap`
        // marks it one-shot so the resumed drive steps into the unwind instead of re-trapping.
        // Ignored under `RunToCompletion` (a host directive, like breakpoints — the M6 matrix).
        if directive != Directive::RunToCompletion && instance.take_raise_trap() {
            instance.set_state(InstanceState::Paused);
            return Outcome::Paused(PauseReason::RaiseTrap);
        }
        match instance.step() {
            Ok(safe_point) => {
                // A capability call parked a request (E§7.5, MD §14): suspend, no state
                // torn down. Checked before the pause decision (they cannot coincide —
                // a capability call is not a statement-level safe point). A slice
                // exhausted on this same step is honored on the next loop turn (a suspend
                // wins — the instance is `Suspended`, resumed via `resolve`).
                if instance.is_suspended() {
                    instance.set_state(InstanceState::Suspended);
                    // An `import` of an unloaded module (E§6) suspends the same way but is
                    // resolved through `resolve_import`, so it surfaces as its own outcome
                    // that routes the host there.
                    return if instance.is_import_suspended() {
                        Outcome::SuspendedImport(instance.import_request())
                    } else {
                        Outcome::Suspended(instance.capability_request())
                    };
                }
                // A host-requested pause (E§8.8): stops at the next safe point **regardless
                // of directive** — a host control like cancel, not a `Step*` decision — so it
                // is checked before `should_pause` and wins over it. Only at an actual safe
                // point (`safe_point.is_some()`), never mid-expression; consumed here
                // (`take_host_pause` reads-and-clears) so the request is one-shot and the
                // re-drive continues. State stays intact and resumable — a pause is not a
                // fault, so no unwind is armed.
                if safe_point.is_some() && instance.take_host_pause() {
                    instance.set_state(InstanceState::Paused);
                    return Outcome::Paused(PauseReason::HostPause);
                }
                // A breakpoint at this safe point (E§8.6): stop under `Continue` or a `Step*`
                // directive, never under `RunToCompletion` (which ignores breakpoints).
                // `breakpoint_hit` matches the statement about to run, so a loop-body
                // breakpoint re-fires each iteration. Checked before the `Step*` decision so a
                // breakpoint that coincides with a step reports `Breakpoint`, the specific
                // reason.
                if safe_point.is_some()
                    && directive != Directive::RunToCompletion
                    && let Some(id) = instance.breakpoint_hit()
                {
                    instance.set_state(InstanceState::Paused);
                    return Outcome::Paused(PauseReason::Breakpoint(id));
                }
                if let Some(depth) = safe_point
                    && should_pause(directive, anchor_depth, depth)
                {
                    instance.set_state(InstanceState::Paused);
                    return Outcome::Paused(PauseReason::Step);
                }
            }
            // An uncaught raise reached the outermost boundary (no handlers yet;
            // `try`/`rescue` is M4). The instance enters the terminal `Raised` state
            // (E§3.3/§9), distinct from `Faulted`; the outcome carries exception + trace.
            Err(Halt::Raise(value, trace)) => {
                instance.set_state(InstanceState::Raised);
                return Outcome::Raised(value, trace);
            }
            // A resource limit (E§10.2), host cancellation (E§10.1), or the S-15
            // `NestedSuspend` relayed from a native consumer's reentrant drive (§7.6): a
            // non-resumable fault (`state()` becomes terminal `Faulted`).
            Err(Halt::Fault(fault)) => {
                instance.set_state(InstanceState::Faulted);
                return Outcome::Faulted(fault);
            }
        }
    }
}

/// Terminally faults `instance` on a host-contract violation (e.g. a stale resolution
/// handle): sets the state to `Faulted` so a returned `Faulted` outcome always implies
/// `state() == Faulted` (E§3.3 outcome↔state correspondence).
fn fault(instance: &mut Instance) -> Outcome {
    instance.set_state(InstanceState::Faulted);
    Outcome::Faulted(EngineFault::Internal)
}

/// Whether a safe point at frame `depth` stops the given `directive`, anchored at the
/// depth the drive resumed from (E§8.5, basic granularity — breakpoints and
/// tail-aware refinements are M6).
fn should_pause(directive: Directive, anchor_depth: usize, depth: usize) -> bool {
    match directive {
        // A fast run and a "resume the program" both run to the next capability /
        // fault / completion; the breakpoints and raise-trap that make `Continue`
        // distinct are M6.
        Directive::RunToCompletion | Directive::Continue => false,
        // Stop at the very next safe point, in any frame.
        Directive::Step | Directive::StepInto => true,
        // Treat a call as one step: stop at the next safe point at the anchor depth or
        // shallower (a call's deeper entry safe point is skipped).
        Directive::StepOver => depth <= anchor_depth,
        // Run until the current frame returns: stop only strictly shallower.
        Directive::StepOut => depth < anchor_depth,
    }
}
