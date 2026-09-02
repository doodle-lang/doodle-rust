//! The machine's single transition, `step` (machine-design §8): pop the top
//! frame's top continuation and perform one step of work.
//!
//! **Scope (M2a.5).** Statement sequencing; literal, arithmetic, comparison, and
//! boolean evaluation; `let`/`const`/assignment; `if`/`while`/`loop`; calls of
//! `to`/`fn`/anonymous-`fn` values with keyword arguments and defaults; `is`; and
//! the **raise** path — a failing operation returns `Err(Raise)`, which the drive
//! loop turns into `Raised` (no handlers yet; `try`/`rescue` is M4, the §12 unwind
//! mechanism M2a.6). Blocks, `return`/`break`/`continue`, and PTC are M2a.6/M2a.7;
//! other node kinds reach an `unimplemented!`.

use super::cont::Cont;
use super::error::{ExceptionKind, Raise, Trace};
use super::frame::FrameKind;
use super::modload::LoadState;
use super::{Halt, LoadedModule, Machine, ObservationMode, Value, limits, stmt, unwind};
use crate::ast::{BinaryOp, Node, NodeId};
use crate::drive::{EngineFault, SafePoint, SafePointKind};
use crate::heap::Heap;
use crate::resolve::ResolvedModule;
use crate::span::Span;

mod dispatch;
use dispatch::dispatch;

/// Performs one machine transition (machine-design §8), evaluating resource limits
/// at each statement-level safe point (E§7.4, §10.2). Precondition: `machine` has at
/// least one frame (the caller checks `is_halted` first). `Ok(Some(sp))` means the
/// transition crossed a safe point (its frame `depth` and `kind` steer the `Step*`
/// pause decision, E§8.5); `Ok(None)` means none. `Err` stopped the drive.
pub(crate) fn step(
    resolved: &ResolvedModule,
    modules: &mut [LoadedModule],
    heap: &mut Heap,
    machine: &mut Machine,
) -> Result<Option<SafePoint>, Halt> {
    // Fresh each transition (E§7.4/§8.4): only a fine safe point below sets it, so it reflects
    // whether *this* step completed a non-leaf subexpression — `None` during an unwind, at a
    // statement/call-entry safe point, and in coarse mode.
    machine.fine_span = None;
    // A non-local transfer in flight takes over the transition (§12): unwind toward
    // the exit's target instead of running continuations normally. Intervening cleanup
    // steps hit no safe point, but the **settling** transition — where the exit pops
    // its target frame and returns control to a shallower frame (a `return` reaching
    // its home callable, a `break` reaching its consumer) — is a return safe point at
    // the post-pop depth, exactly like a fall-through `ReturnBarrier`. Reporting it
    // lets `StepOut` stop the instant the frame returns and keeps the limit checks
    // consistent across the two return paths (E§7.4).
    if machine.unwind.is_some() {
        let cancelling = matches!(machine.unwind, Some(unwind::Unwind::Cancel));
        // An unwind arm may itself raise (a bare `return` off a `fn`'s end): that becomes
        // a Raise unwind, running cleanup on its way out rather than propagating straight
        // to the boundary.
        let settle = match unwind::step(resolved, heap, machine) {
            Ok(settle) => settle,
            Err(raise) => {
                arm_raise(resolved, machine, heap, raise);
                return Ok(None);
            }
        };
        // Cancellation teardown (E§10.1, §12): once the cancel unwind empties the stack,
        // the whole program is torn down — fault `Cancelled`, a non-resumable stop that
        // Doodle code cannot catch.
        if cancelling && machine.frames.is_empty() {
            machine.unwind = None;
            return Err(Halt::Fault(EngineFault::Cancelled));
        }
        // An uncaught raise drained the whole stack (no `TryHandler` cleared it): it
        // reaches the outermost boundary as the terminal `Raised` outcome (E§9).
        if machine.frames.is_empty() && matches!(machine.unwind, Some(unwind::Unwind::Raise { .. }))
        {
            let (value, trace) = take_raise(machine);
            return Err(Halt::Raise(value, trace));
        }
        return match settle {
            // The settling transition hands control back to a shallower frame — a return
            // safe point (E§8.5): `StepOver` treats a return *into* the stepped-over frame
            // (equal depth) as mid-statement, not a stop (see `should_pause`).
            Some(depth) => {
                limits::safe_point(heap, machine)?;
                Ok(Some(SafePoint {
                    depth,
                    kind: SafePointKind::Return,
                }))
            }
            None => Ok(None),
        };
    }
    // Pop the top frame's top continuation; the borrow ends before we dispatch,
    // so a transition is free to push work back onto the same (or a new) frame.
    let cont = machine
        .frames
        .last_mut()
        .expect("step with no frame")
        .conts
        .pop();
    // Statement-level safe points (E§7.4): between statements (`Seq`) and at return
    // (a callable's `ReturnBarrier`, or `None` = the module top level draining). The
    // third — call/block entry — is detected after dispatch by the frame stack
    // growing, which is also the only place non-tail depth grows.
    let stmt_safe_point = matches!(
        cont,
        Some(Cont::Seq { .. }) | Some(Cont::ReturnBarrier) | None
    );
    // Classify that safe point for `Step*` (E§8.5): a `Seq` is a forward statement boundary; a
    // `ReturnBarrier` (a callable falling off its end) or a drained `None` (the module top
    // returning) hands control back — a return safe point, like the unwind-settle above.
    let stmt_kind = if matches!(cont, Some(Cont::Seq { .. })) {
        SafePointKind::Boundary
    } else {
        SafePointKind::Return
    };
    // Record the statement about to run for breakpoint matching (E§8.6): the `Seq` case is
    // about to dispatch `stmts[next]`. Only at the outer drive — a reentrant native-consumer
    // drive's steps are not the outer drive's safe points, so `reentry_depth > 0` records
    // `None` rather than clobbering the outer statement with an inner one. A return safe point
    // (`ReturnBarrier`/`None`) or a call-entry safe point (a non-`Seq` cont) has no statement.
    machine.safe_point_stmt = match &cont {
        Some(Cont::Seq { block, next }) if machine.reentry_depth == 0 => {
            stmt_list(resolved.ast.node(*block))
                .get(*next as usize)
                .copied()
        }
        _ => None,
    };
    // In fine mode (E§7.4, S-62), classify the popped cont: does executing it *complete* a
    // non-leaf subexpression? Computed before `dispatch` consumes `cont`. `and`/`or` that
    // short-circuits completes inside `dispatch` instead and sets `fine_span` there.
    let fine_completion = if machine.observation_mode == ObservationMode::Subexpression {
        cont.as_ref()
            .and_then(|c| fine_completion_span(c, resolved))
    } else {
        None
    };
    let depth_before = machine.frames.len();
    let dispatched = dispatch(resolved, modules, heap, machine, cont);
    // A reentrant nested drive (a native block-consumer running its block) faulted —
    // a limit tripped, or the S-15 `NestedSuspend` (a suspending capability reached
    // inside the native consumer, forbidden — Decision #2). It parks the fault because
    // the Raise-typed `apply` chain cannot carry an `EngineFault`; surface it here as
    // this transition's fault (MD §14). A fault takes priority over a raise.
    if let Some(fault) = machine.take_pending_fault() {
        return Err(Halt::Fault(fault));
    }
    // A raise from the transition begins a **Raise unwind** (machine-design §12): it
    // unwinds through the frames running `WithRestore` cleanup and seeking a handler,
    // rather than propagating straight to the boundary. An uncaught raise drains the
    // stack and surfaces as the terminal `Raised` in the unwind branch above.
    if let Err(raise) = dispatched {
        arm_raise(resolved, machine, heap, raise);
        return Ok(None);
    }
    // The safe point that fired this transition (frame depth + kind, for `Step*` anchoring),
    // or `None`. A statement safe point and a call-entry safe point never coincide in
    // one transition (a `Seq`/`ReturnBarrier` step pushes no frame), so at most one fires.
    let mut safe_point = None;
    if stmt_safe_point {
        limits::safe_point(heap, machine)?;
        // Cancellation (E§10.1): the host stop button, polled at this safe point. Arming
        // the cancel unwind takes over the next transition, so do not also offer this as
        // a `Step*` pause — the drive re-steps straight into the teardown.
        if machine.poll_cancel() {
            return Ok(None);
        }
        safe_point = Some(SafePoint {
            depth: machine.frames.len(),
            kind: stmt_kind,
        });
    }
    // A call or block invocation just pushed a frame — a **non-tail** entry (a tail
    // call reuses a frame in place, §11): the call-entry safe point, and the only
    // place non-tail stack depth grows. A forward boundary (descending into a call), not a
    // return.
    let depth = machine.frames.len();
    if depth > depth_before {
        limits::safe_point(heap, machine)?;
        machine.check_stack_depth(depth)?;
        if machine.poll_cancel() {
            return Ok(None);
        }
        safe_point = Some(SafePoint {
            depth,
            kind: SafePointKind::Boundary,
        });
    }
    // A **fine** (per-subexpression) safe point (E§7.4, S-62), only when no statement/call-entry
    // safe point already fired this step: the popped cont completed a non-leaf subexpression
    // (`fine_completion`), or an `and`/`or` short-circuited (`fine_span` set in `dispatch`).
    // **Observation-only** — no `limits::safe_point`/`poll_cancel` runs here, so the step budget,
    // slice fuel, GC, and cancellation observation stay at statement safe points and a fault
    // lands at the same instant in either mode. `fine_span` carries the completed span to
    // `completed_position`; cleared when this is not a fine stop. A within-statement stop, so a
    // forward boundary (a `StepOver` at the anchor depth still stops for it).
    if safe_point.is_none()
        && let Some(span) = fine_completion.or(machine.fine_span)
    {
        machine.fine_span = Some(span);
        safe_point = Some(SafePoint {
            depth: machine.frames.len(),
            kind: SafePointKind::Boundary,
        });
    } else {
        machine.fine_span = None;
    }
    Ok(safe_point)
}

/// Runs the statement at `next` in `block`, and re-arms the sequence for the
/// statement after it. When the body is exhausted, nothing is pushed and the
/// frame returns on the following `step`.
fn seq_step(resolved: &ResolvedModule, machine: &mut Machine, block: NodeId, next: u32) {
    let stmts = stmt_list(resolved.ast.node(block));
    let Some(&stmt) = stmts.get(next as usize) else {
        return;
    };
    // Clear the register at each statement boundary, so a body's value is the value
    // of its *last* statement — Void when that statement is value-less (an
    // assignment, a `while`/`loop`, an unmatched `if`) or the body is empty. Without
    // this, a value-less-tailed or empty **block** would leak the previous
    // statement's transient value as its yield (§8.5). (Resolves the statement-
    // boundary register question carried from M2a.2 for the cases blocks make
    // observable; the final `Seq` step — past the last statement — does not clear,
    // preserving that last value for a `fn` body / block yield.)
    machine.reg = None;
    let frame = machine.frames.last_mut().expect("seq_step with no frame");
    frame.conts.push(Cont::Seq {
        block,
        next: next + 1,
    });
    stmt::dispatch_stmt(resolved, frame, stmt);
}

/// The top frame's work is drained with no `ReturnBarrier` beneath it: only the
/// module top level ends this way, completing Void (L§6.11) — its final
/// statement's transient value is discarded. A callable frame instead returns
/// through its [`Cont::ReturnBarrier`] ([`call::return_from_callable`]).
fn return_from_top_frame(machine: &mut Machine) {
    let frame = machine.frames.pop().expect("return with no frame");
    match frame.kind {
        FrameKind::ModuleTopLevel => {
            machine.reg = None;
            // A sub-module's top level completing (an importer frame remains beneath, E§6):
            // the module is now fully loaded (L§11.3 singleton), so a later import of it
            // binds against the loaded instance instead of reloading. The main module
            // draining instead empties the stack (program completion) — nothing to record.
            if !machine.frames.is_empty() {
                machine.load.set_state(frame.module, LoadState::Loaded);
            }
        }
        FrameKind::Callable { .. } | FrameKind::Block { .. } => {
            unreachable!(
                "a callable/block frame returns via its ReturnBarrier, not an empty cont stack"
            )
        }
    }
}

/// Arms an in-flight **Raise unwind** (machine-design §12) from an engine raise that
/// surfaced during a transition, replacing any current transfer: the raise's kind +
/// message **materialize** an `Error` record value (L§12.1), and the unwinder then walks
/// the frames running `WithRestore` cleanup and seeking a `TryHandler`.
pub(crate) fn arm_raise(
    resolved: &ResolvedModule,
    machine: &mut Machine,
    heap: &mut Heap,
    raise: Raise,
) {
    // Capture the trace from the raise-site frames before materializing anything (L§12.1:
    // captured at the point of raise). The Rust `?` that surfaced the raise did not touch
    // the CESK frames, so they still reflect the raise site.
    let trace = super::observe::capture_trace(resolved, heap, machine, raise.trace.raised_at);
    let value = super::exception::make_error(
        heap,
        machine.error_type,
        raise.exception.kind.slug(),
        &raise.exception.message,
        &raise.details,
    );
    machine.unwind = Some(unwind::Unwind::Raise {
        value,
        trace,
        trapped: false,
    });
}

/// Takes the in-flight Raise unwind's value + trace and clears the transfer — for the
/// drained, uncaught raise reaching the boundary.
fn take_raise(machine: &mut Machine) -> (Value, Trace) {
    match machine.unwind.take() {
        Some(unwind::Unwind::Raise { value, trace, .. }) => (value, trace),
        _ => unreachable!("take_raise with no in-flight raise"),
    }
}

/// Takes the register's value, raising if it is Void (L§6.11): a procedure result
/// used where a value is required. (Structural backstop for the resolver's static
/// S-6 check; reachable dynamically once calls can return Void, M2a.5.) Shared
/// with [`super::control`].
pub(crate) fn take_value(machine: &mut Machine, span: crate::span::Span) -> Result<Value, Raise> {
    machine.reg.take().ok_or_else(|| {
        Raise::new(
            ExceptionKind::ProcedureInExpression,
            "this spot needs a value, but a procedure gives none",
            span,
        )
    })
}

fn is_arithmetic(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Add
            | BinaryOp::Sub
            | BinaryOp::Mul
            | BinaryOp::Div
            | BinaryOp::FloorDiv
            | BinaryOp::Rem
            | BinaryOp::Pow
    )
}

/// The span of the non-leaf subexpression a cont **completes** (E§7.4, S-62), for a fine
/// safe point, or `None` if executing it does not complete one. The completing conts are
/// the syntactic forms E§7.4 enumerates: operator applications (`BinApply`/`UnaryApply`, and
/// an `and`/`or`'s full-eval result at `AssertBool`), field access (`FieldRead`), and index
/// steps (`IndexApply`/`IndexReadHashed`), plus each interpolation piece (`StrInterp`/
/// `StrInterpRendered`). Leaves (literals, name reads) and mid-evaluation plumbing
/// (`BinRhs`, `CallGotArg`, list/dict building — deliberately not in the set) return `None`.
/// A short-circuited `and`/`or` completes inside `dispatch` and records its span there. An
/// `if`-expression branch result completes at the branch's final statement safe point (its
/// arms are blocks), so it needs no entry here. The span is what the cont carries: the whole
/// construct for index/field/interpolation, the operator for the arithmetic/boolean ops.
fn fine_completion_span(cont: &Cont, resolved: &ResolvedModule) -> Option<Span> {
    match cont {
        Cont::BinApply { span, .. }
        | Cont::UnaryApply { span, .. }
        | Cont::AssertBool { span }
        | Cont::IndexApply { span, .. }
        | Cont::IndexReadHashed { span, .. } => Some(*span),
        Cont::FieldRead { field } => Some(resolved.ast.span(*field)),
        Cont::StrInterp { node, .. } | Cont::StrInterpRendered { node, .. } => {
            Some(resolved.ast.span(*node))
        }
        _ => None,
    }
}

/// The statement list of a body node (`Module` or `Block`).
fn stmt_list(node: &Node) -> &[NodeId] {
    match node {
        Node::Module { stmts, .. } => stmts,
        Node::Block(stmts) => stmts,
        other => unreachable!("Seq over a non-body node: {other:?}"),
    }
}
