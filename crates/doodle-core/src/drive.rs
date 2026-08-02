//! The drive loop: [`Outcome`]s and the [`run`] entry point (engine spec E§7).
//!
//! M2a.2: [`run`] advances the real machine one [`step`](Instance::step) at a
//! time to completion. The demo subset has no capabilities, breakpoints, or safe
//! points yet, so every directive runs straight to `Completed`; the other
//! outcomes' payloads (suspension, raise, fault) are shells the later M2a/M2b
//! chunks fill in.

use crate::machine::{Exception, Halt, Handle, Instance, InstanceState, Trace, Value};
use crate::unicode::UnicodeVersion;

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
    /// Execution stopped at a safe point for observation.
    Paused(PauseReason),
    /// An uncaught exception reached the boundary.
    Raised(Exception, Trace),
    /// A limit, cancellation, or internal fault stopped execution.
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
    /// An internal invariant was violated.
    Internal,
}

/// Which limit was exceeded (engine spec E§10.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LimitKind {
    /// The step budget (safe points executed).
    StepBudget,
    /// The heap limit (bytes or objects).
    Heap,
    /// The non-tail-call stack-depth limit.
    StackDepth,
    /// The tail-history bound (E§8.3).
    TailHistory,
}

/// Resource limits for an instance (engine spec E§10.2), enforced by the machine
/// at statement-level safe points (E§7.4). This is the limits **subset** of the
/// `create(config)` surface (E§3.1); the rest — the module-resolver hook, target
/// Unicode version (S-41), observation mode, and host data — lands with the full
/// config surface at **M2a.11**.
///
/// Exceeding any limit yields [`Outcome::Faulted`]`(`[`EngineFault::LimitExceeded`]`)`.
/// Proper tail calls reuse frames (L§8.7), so a tail loop never trips `stack_depth`;
/// a runaway **non-tail** recursion does. The tail-history bound (E§8.3) is a fixed
/// ring capacity that overwrites its oldest entry rather than faulting, so it is not
/// a field here.
///
/// The [`Default`] values are **provisional** engineering ceilings: generous enough
/// that ordinary kid-authored programs never trip them, yet finite so a runaway
/// still faults. E§10.2 leaves the concrete values to host config; the real
/// host-chosen values arrive with the M2a.11 config surface.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// Maximum statement-level safe points executed (E§7.4) before
    /// `LimitExceeded(StepBudget)`. The engine owns no clock, so a host enforces a
    /// wall-clock timeout via this budget or by cancelling (E§10.2).
    pub step_budget: u64,
    /// Maximum heap payload bytes ([`Heap::bytes_allocated`](crate::heap::Heap::bytes_allocated),
    /// which excludes pure caches, MD §5) before `LimitExceeded(Heap)`.
    pub heap_bytes: u64,
    /// Maximum non-tail frame-stack depth before `LimitExceeded(StackDepth)`.
    pub stack_depth: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            // ~1.1e12 safe points, ~1.7e10 payload bytes (16 GiB), 100k non-tail
            // frames — see the type's "provisional" note.
            step_budget: 1 << 40,
            heap_bytes: 1 << 34,
            stack_depth: 100_000,
        }
    }
}

/// Instance configuration (engine spec E§3.1). The resource-limits subset landed at
/// M2a.9; this adds the target Unicode version (S-41). The module-resolver hook,
/// observation mode, and opaque host-data value join with their features (M2b/M5).
#[derive(Clone, Copy, Debug, Default)]
pub struct Config {
    /// Resource limits (E§10.2).
    pub limits: Limits,
    /// The requested target Unicode version (E§3.1, L§4.4). `None` uses the engine's
    /// build-pinned version ([`crate::unicode::UNICODE_VERSION`]); `Some(v)` that is
    /// not that pinned version fails [`create`](crate::machine::Instance::create)
    /// with [`ConfigError::UnsupportedUnicodeVersion`] (S-41). Naming it lets a host
    /// assert the version a recording was made under at create time — a loud failure
    /// instead of a silent grapheme/normalization divergence (determinism, E§11).
    pub unicode_version: Option<UnicodeVersion>,
}

/// Why [`create`](crate::machine::Instance::create) rejected a [`Config`] (E§3.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConfigError {
    /// The config named a Unicode version the engine does not support — at M2a, any
    /// version other than the single build-pinned one (S-41).
    UnsupportedUnicodeVersion {
        /// The version the host requested.
        requested: UnicodeVersion,
        /// The engine's build-pinned version.
        pinned: UnicodeVersion,
    },
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

/// Starts (from `Ready`) or continues (after a `Paused`) driving `instance` under
/// `directive`, returning an [`Outcome`] (E§7.3).
///
/// `RunToCompletion` and `Continue` run without pausing (breakpoints and the
/// raise-trap that make `Continue` distinct are M6); a `Step*` directive pauses at
/// the next statement-level safe point selected by frame depth (E§8.5) →
/// [`Paused`](Outcome::Paused)`(`[`PauseReason::Step`]`)`. An uncaught raise leaves
/// the instance `Raised`, an engine fault leaves it `Faulted`, and completion leaves
/// it `Completed` (E§3.3 outcome↔state correspondence). Re-driving a terminal
/// instance, or `run`-ing a `Suspended` one (use [`resolve`]), is a host-contract
/// violation — debug-asserted, and a returned `Faulted(Internal)` in release rather
/// than undefined behavior.
pub fn run(instance: &mut Instance, directive: Directive) -> Outcome {
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
    drive(instance, directive)
}

/// Continues a `Suspended` `instance` with the host's `resolution` (E§7.3/§7.5): the
/// value the capability produced becomes the call's result and the drive resumes under
/// the directive in force; a raise surfaces at the capability call site (leaving the
/// instance `Raised`). `resolve` on an instance with no pending suspension is a
/// host-contract violation (debug-asserted; `Faulted(Internal)` in release).
pub fn resolve(instance: &mut Instance, resolution: Resolution) -> Outcome {
    debug_assert!(
        matches!(instance.state(), InstanceState::Suspended),
        "resolve() requires a Suspended instance (got {:?}); it continues a pending \
         capability request (E§7.3/§7.5)",
        instance.state()
    );
    if !matches!(instance.state(), InstanceState::Suspended) {
        return Outcome::Faulted(EngineFault::Internal);
    }
    match resolution {
        // The capability's value becomes the call's result; resume the drive so the
        // caller's waiting continuation consumes it, under the directive in force.
        Resolution::Value(handle) => match instance.resume_with_value(handle) {
            Ok(()) => drive(instance, instance.resume_directive()),
            // A stale resolution handle is a host-contract violation (a non-resumable
            // engine fault): the suspension was cleared, so leave the instance terminally
            // `Faulted` — a `Faulted` outcome always implies `state() == Faulted` (E§3.3).
            Err(_) => fault(instance),
        },
        // The host rejected the capability: raise at its call site. No handlers yet
        // (`try`/`rescue` is M4), so it reaches the boundary → the terminal `Raised`
        // state (E§3.3), like any uncaught raise.
        Resolution::Raise(handle) => match instance.resume_with_raise(handle) {
            Ok((exception, trace)) => {
                instance.set_state(InstanceState::Raised);
                Outcome::Raised(exception, trace)
            }
            Err(_) => fault(instance),
        },
    }
}

/// The core drive loop: steps `instance` to a stopping [`Outcome`] under `directive`,
/// pausing a `Step*` at the next matching safe point and suspending at a capability
/// call. Shared by [`run`] and the resume side of [`resolve`].
fn drive(instance: &mut Instance, directive: Directive) -> Outcome {
    // Anchor `Step*` depth judgments at the frame depth we resume from (E§8.5), and
    // remember the directive so a `resolve` after a suspend resumes under it (E§7.3).
    let anchor_depth = instance.frame_depth();
    instance.set_directive(directive);
    instance.set_state(InstanceState::Running);
    loop {
        if instance.is_halted() {
            instance.set_state(InstanceState::Completed);
            // E§7.2: a top-level module drive completes with **no value** (Void) — a
            // module runs for effect (L§6.11). `Completed`'s value is present only for
            // a returning `fn` (a reentrant callable return, E§7.6, M2b.5).
            return Outcome::Completed(None);
        }
        match instance.step() {
            Ok(safe_point) => {
                // A capability call parked a request (E§7.5, MD §14): suspend, no state
                // torn down. Checked before the pause decision (they cannot coincide —
                // a capability call is not a statement-level safe point).
                if instance.is_suspended() {
                    instance.set_state(InstanceState::Suspended);
                    return Outcome::Suspended(instance.capability_request());
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
            Err(Halt::Raise(raise)) => {
                instance.set_state(InstanceState::Raised);
                return Outcome::Raised(raise.exception, raise.trace);
            }
            // A resource limit (or, later, cancellation) at a safe point (E§10.2): a
            // non-resumable fault.
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
