//! The drive loop: [`Outcome`]s and the [`run`] entry point (engine spec E§7).
//!
//! M2a.2: [`run`] advances the real machine one [`step`](Instance::step) at a
//! time to completion. The demo subset has no capabilities, breakpoints, or safe
//! points yet, so every directive runs straight to `Completed`; the other
//! outcomes' payloads (suspension, raise, fault) are shells the later M2a/M2b
//! chunks fill in.

use crate::machine::{Exception, Halt, Instance, InstanceState, Trace, Value};

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

/// A capability request carried by [`Outcome::Suspended`] (engine spec E§7.5).
///
/// Shell for M0: the capability identity and its bound argument handles are
/// added when foreign-function registration lands (E§5, M2b).
#[derive(Clone, Copy, Debug)]
pub struct CapabilityRequest;

/// Drives `instance` under `directive`, returning an [`Outcome`] (E§7.3).
///
/// M2a.3a runs the machine to completion or to an **uncaught raise** — a runtime
/// error (type mismatch, division by zero, a nonfinite float result, …) has no
/// handler yet (`try`/`rescue` is M4), so it surfaces as [`Outcome::Raised`]. The
/// `Step*`/`Continue` directives gain meaning once safe points, breakpoints, and
/// the fused counter land (E§7.4, M2a.9); until then every directive runs through.
pub fn run(instance: &mut Instance, directive: Directive) -> Outcome {
    let _ = directive;
    // A terminal instance is not re-drivable at M2a.3a: after a raise, frames are
    // left on the stack, so re-driving would resume them. The full drive-state
    // machine — resuming Paused/Suspended, rejecting terminal states per E§7 — is
    // M2b; until then, guard the single-drive contract.
    debug_assert!(
        matches!(instance.state(), InstanceState::Ready),
        "re-driving a non-Ready instance is not yet supported (M2b)"
    );
    instance.set_state(InstanceState::Running);
    while !instance.is_halted() {
        match instance.step() {
            Ok(()) => {}
            // An uncaught raise (no handlers at M2a.3a). The post-raise instance
            // state (E§3.3 has no distinct "raised" state) is provisionally
            // `Faulted` — tracked as a discovered delta; the outcome carries the
            // real result.
            Err(Halt::Raise(raise)) => {
                instance.set_state(InstanceState::Faulted);
                return Outcome::Raised(raise.exception, raise.trace);
            }
            // A resource limit was exceeded at a safe point (E§10.2): a
            // non-resumable fault.
            Err(Halt::Fault(fault)) => {
                instance.set_state(InstanceState::Faulted);
                return Outcome::Faulted(fault);
            }
        }
    }
    instance.set_state(InstanceState::Completed);
    // E§7.2: a top-level module drive completes with **no value** (Void). A
    // module runs for effect (L§6.11 — statements yield no value); `Completed`'s
    // value is present only for a returning `fn` (a reentrant callable return,
    // E§7.6), which arrives at M2a.5. This replaces the M0.3 provisional (which
    // returned the last expression's value).
    Outcome::Completed(None)
}
