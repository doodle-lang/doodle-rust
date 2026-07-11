//! The drive loop: [`Outcome`]s and the [`run`] entry point (engine spec E§7).
//!
//! Shell for M0: the [`Outcome`] enum (E§7.2) and a [`run`] that walks a
//! hand-built program's top-level statements to completion. The M0 skeleton
//! has no capabilities, breakpoints, or safe points, so it produces only
//! `Completed`; the other outcomes' payloads are shells the machine core
//! (M2a/M2b) fills in.

use crate::ast::{Ast, Node, NodeId};
use crate::machine::{Instance, InstanceState, Value};

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

/// A capability request carried by [`Outcome::Suspended`] (engine spec E§7.5).
///
/// Shell for M0: the capability identity and its bound argument handles are
/// added when foreign-function registration lands (E§5, M2b).
#[derive(Clone, Copy, Debug)]
pub struct CapabilityRequest;

/// An exception value reaching the boundary (engine spec E§9).
///
/// Shell for M0: filled in when the machine can raise (M2a).
#[derive(Clone, Copy, Debug)]
pub struct Exception;

/// A stack trace accompanying a [`Outcome::Raised`] (engine spec E§8.2).
///
/// Shell for M0: filled in when the call stack exists (M2a).
#[derive(Clone, Copy, Debug)]
pub struct Trace;

/// Drives `instance` under `directive`, returning an [`Outcome`] (E§7.3).
///
/// The M0 skeleton has no stop conditions, so every directive runs the
/// hand-built program straight to `Completed`; the `Step*` directives gain
/// meaning once safe points land (E§7.4, M2a).
pub fn run(instance: &mut Instance, directive: Directive) -> Outcome {
    let _ = directive;
    instance.set_state(InstanceState::Running);
    while let Some(stmt) = instance.next_statement() {
        let value = eval_stmt(instance.program(), stmt);
        instance.set_result(value);
    }
    instance.set_state(InstanceState::Completed);
    // E§7.2 pins `Completed`'s value only for a returning `fn`; what a
    // *top-level* drive carries is unspecified. The skeleton provisionally
    // returns the result register's last value so the acceptance test can
    // observe it; real top-level completion is expected to be Void (`None`).
    // To be pinned in E§7.2 by M2a (see claude-todo spec-delta queue).
    Outcome::Completed(instance.result())
}

/// Evaluates one top-level statement, returning its value (or `None` = Void).
///
/// The M0 skeleton stores an expression statement's value in the result
/// register so the driver can observe it; real statement semantics (values are
/// discarded, only `fn` bodies yield, L§6.11) arrive with the machine core.
fn eval_stmt(program: &Ast, id: NodeId) -> Option<Value> {
    match program.node(id) {
        Node::ExprStmt(expr) => Some(eval_expr(program, *expr)),
        // The M0 driver only builds expression statements; anything else in
        // statement position is Void here (real statement semantics arrive with
        // the machine core, M2a).
        _ => None,
    }
}

/// Evaluates one expression node to a [`Value`].
fn eval_expr(program: &Ast, id: NodeId) -> Value {
    match program.node(id) {
        Node::IntLit(n) => Value::Int(*n),
        // Only literals appear in expression position in the M0 grammar; a
        // node reaching here is a front-end bug, so fail loudly rather than
        // silently yielding a value that could mask it. The front end grows
        // the real expression kinds at M1+.
        other => unreachable!("non-expression node in expression position: {other:?}"),
    }
}
