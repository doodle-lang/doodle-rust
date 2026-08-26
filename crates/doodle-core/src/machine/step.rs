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
use super::control::{self, Namespace};
use super::error::{ExceptionKind, Raise, Trace};
use super::frame::{Frame, FrameKind};
use super::{
    Halt, Machine, Value, arith, block, call, compare, dict, dynamic, eval, limits, protect,
    record, strop, types, unwind,
};
use crate::ast::{BinaryOp, Node, NodeId, UnaryOp};
use crate::drive::EngineFault;
use crate::heap::Heap;
use crate::resolve::ResolvedModule;

/// Performs one machine transition (machine-design §8), evaluating resource limits
/// at each statement-level safe point (E§7.4, §10.2). Precondition: `machine` has at
/// least one frame (the caller checks `is_halted` first). `Ok(Some(depth))` means the
/// transition crossed a statement-level safe point at that frame depth (where the
/// drive loop may pause a `Step*`); `Ok(None)` means none. `Err` stopped the drive.
pub(crate) fn step(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    namespace: &Namespace,
) -> Result<Option<usize>, Halt> {
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
            Some(depth) => {
                limits::safe_point(heap, machine, namespace)?;
                Ok(Some(depth))
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
    let depth_before = machine.frames.len();
    let dispatched = dispatch(resolved, heap, machine, namespace, cont);
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
    // The frame depth where a safe point fired this transition (for `Step*` anchoring),
    // or `None`. A statement safe point and a call-entry safe point never coincide in
    // one transition (a `Seq`/`ReturnBarrier` step pushes no frame), so at most one fires.
    let mut safe_point_depth = None;
    if stmt_safe_point {
        limits::safe_point(heap, machine, namespace)?;
        // Cancellation (E§10.1): the host stop button, polled at this safe point. Arming
        // the cancel unwind takes over the next transition, so do not also offer this as
        // a `Step*` pause — the drive re-steps straight into the teardown.
        if machine.poll_cancel() {
            return Ok(None);
        }
        safe_point_depth = Some(machine.frames.len());
    }
    // A call or block invocation just pushed a frame — a **non-tail** entry (a tail
    // call reuses a frame in place, §11): the call-entry safe point, and the only
    // place non-tail stack depth grows.
    let depth = machine.frames.len();
    if depth > depth_before {
        limits::safe_point(heap, machine, namespace)?;
        machine.check_stack_depth(depth)?;
        if machine.poll_cancel() {
            return Ok(None);
        }
        safe_point_depth = Some(depth);
    }
    Ok(safe_point_depth)
}

/// Executes one popped continuation — or, when `None`, returns from a drained
/// frame. This is the transition proper; `step` wraps it with the safe-point limit
/// checks. Its error is a plain [`Raise`]; `step`'s `?` lifts it into [`Halt`].
fn dispatch(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    namespace: &Namespace,
    cont: Option<Cont>,
) -> Result<(), Raise> {
    match cont {
        Some(Cont::Seq { block, next }) => {
            seq_step(resolved, machine, block, next);
            Ok(())
        }
        Some(Cont::Eval { node }) => eval::eval(resolved, heap, machine, namespace, node),
        Some(Cont::BinRhs { op, rhs, span }) => eval::bin_rhs(machine, op, rhs, span),
        Some(Cont::BinApply { op, lhs, span }) => {
            let rhs = take_value(machine, span)?;
            let result = match op {
                // `x is T`: the right operand is a type value (L§6.5). The callable
                // trio (`Procedure`/`Function`/`Callable`) needs the resolver and the
                // intrinsic registry to read a callable's `to`/`fn` kind (S-37).
                BinaryOp::Is => types::is_op(lhs, rhs, heap, resolved, &machine.intrinsics, span)?,
                // String `+`/`*` branch off the numeric path (L§4.4, S-59); a pair with no
                // string operand falls through to numeric arithmetic.
                _ if is_arithmetic(op) => {
                    match strop::try_binary(op, lhs, rhs, heap, machine, span)? {
                        Some(v) => v,
                        None => arith::binary(op, lhs, rhs, heap, span)?,
                    }
                }
                // A comparison or equality operator (`== != < > <= >=`).
                _ => compare::binary(op, lhs, rhs, heap, span)?,
            };
            machine.reg = Some(result);
            Ok(())
        }
        Some(Cont::UnaryApply { op, span }) => {
            let operand = take_value(machine, span)?;
            let result = match op {
                UnaryOp::Not => compare::not(operand, span)?,
                UnaryOp::Neg | UnaryOp::Pos => arith::unary(op, operand, heap, span)?,
            };
            machine.reg = Some(result);
            Ok(())
        }
        Some(Cont::AndRhs { rhs, span }) => eval::logical_rhs(machine, rhs, span, true),
        Some(Cont::OrRhs { rhs, span }) => eval::logical_rhs(machine, rhs, span, false),
        Some(Cont::AssertBool { span }) => {
            let v = take_value(machine, span)?;
            machine.reg = Some(Value::Bool(compare::as_bool(v, "and/or", span)?));
            Ok(())
        }
        Some(Cont::BindLet { decl }) => control::bind_let(resolved, heap, machine, namespace, decl),
        Some(Cont::AssignTo { assign }) => {
            control::assign_to(resolved, heap, machine, namespace, assign)
        }
        Some(Cont::AssignPlaceObj { assign }) => {
            control::assign_place_obj(resolved, machine, assign)
        }
        Some(Cont::AssignFieldVal { assign, object }) => {
            record::field_set(resolved, heap, machine, assign, object)
        }
        Some(Cont::AssignIndexKey { assign, object }) => {
            control::assign_index_key(resolved, machine, assign, object)
        }
        Some(Cont::AssignIndexVal {
            assign,
            object,
            key,
        }) => dict::index_set(resolved, heap, machine, assign, object, key),
        Some(Cont::IfChoose { node, index }) => control::if_choose(resolved, machine, node, index),
        Some(Cont::WhileCheck { node }) => control::while_check(resolved, machine, node),
        Some(Cont::LoopReloop { node }) => {
            control::loop_reloop(resolved, machine, node);
            Ok(())
        }
        Some(Cont::CallGotCallee { call }) => {
            call::got_callee(resolved, heap, machine, namespace, call)
        }
        Some(Cont::CallGotArg {
            call,
            callee,
            values,
            index,
        }) => call::got_arg(
            resolved, heap, machine, namespace, call, callee, values, index,
        ),
        Some(Cont::BlockGotArg {
            call,
            values,
            index,
        }) => block::got_block_arg(resolved, heap, machine, call, values, index),
        Some(Cont::ListGotElem {
            list,
            values,
            index,
        }) => eval::list_got_elem(resolved, heap, machine, list, values, index),
        Some(Cont::StrInterp { node, acc, index }) => {
            eval::str_interp(resolved, heap, machine, node, acc, index)
        }
        Some(Cont::DictGotKey {
            dict,
            entries,
            index,
        }) => dict::dict_got_key(resolved, machine, dict, entries, index),
        Some(Cont::DictGotValue {
            dict,
            entries,
            index,
            key,
        }) => dict::dict_got_value(resolved, heap, machine, dict, entries, index, key),
        Some(Cont::IndexGotObject { index, span }) => dict::index_got_object(machine, index, span),
        Some(Cont::IndexApply { object, span }) => dict::index_apply(heap, machine, object, span),
        Some(Cont::BindDefault { slot, default }) => {
            call::bind_default(resolved, heap, machine, slot, default)
        }
        Some(Cont::DefineCallable { decl }) => {
            call::define_callable(resolved, heap, machine, namespace, decl);
            Ok(())
        }
        Some(Cont::DefineRecord { decl }) => {
            record::define(resolved, heap, machine, namespace, decl);
            Ok(())
        }
        Some(Cont::FieldRead { field }) => record::field_read(resolved, heap, machine, field),
        // A `with`'s value is now in the register: open its dynamic binding and run the
        // body under a `WithRestore` (dynamic.rs).
        Some(Cont::WithBind { with }) => {
            dynamic::with_bind(resolved, heap, machine, namespace, with)
        }
        // A `with` body completed normally: restore its dynamic binding (machine-design
        // §13). The body's value stays in the register as the `with`'s value.
        Some(Cont::WithRestore { dyn_mark }) => {
            unwind::restore(machine, heap, dyn_mark);
            Ok(())
        }
        // A `try` body completed normally: its handler is not run, and the body's value
        // is the `try`'s value (already in the register). Discard the handler cont.
        Some(Cont::TryHandler { .. }) => Ok(()),
        // A `raise` throws its operand (or re-raises the handled exception), arming the
        // Raise unwind (protect.rs).
        Some(Cont::RaiseApply { raise }) => protect::raise_apply(resolved, heap, machine, raise),
        // A rescue body finished normally: pop the exception it was handling (L§12.2).
        Some(Cont::PopHandler) => {
            machine.pop_handling();
            Ok(())
        }
        Some(Cont::ReturnBarrier) => call::return_from_callable(resolved, heap, machine),
        Some(Cont::ExitApply { exit }) => unwind::exit_apply(resolved, heap, machine, exit),
        // The frame's work is drained: return from it.
        None => {
            return_from_top_frame(machine);
            Ok(())
        }
    }
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
    dispatch_stmt(resolved, frame, stmt);
}

/// Schedules the work for one statement. A statement's value is discarded at the
/// boundary (only `fn` bodies yield, L§6.11): an expression statement evaluates
/// its expression, whose value the next `Seq` step overwrites.
fn dispatch_stmt(resolved: &ResolvedModule, frame: &mut Frame, stmt: NodeId) {
    match resolved.ast.node(stmt) {
        Node::ExprStmt(expr) => frame.conts.push(Cont::Eval { node: *expr }),
        // Evaluate the initializer, then bind it (LIFO: the `Eval` runs first). A
        // `parameter`'s default seeds its module cell through the same path (§5.5).
        Node::Let { value, .. } | Node::Const { value, .. } => {
            frame.conts.push(Cont::BindLet { decl: stmt });
            frame.conts.push(Cont::Eval { node: *value });
        }
        Node::Parameter { default, .. } => {
            frame.conts.push(Cont::BindLet { decl: stmt });
            frame.conts.push(Cont::Eval { node: *default });
        }
        // A `with`: evaluate the bound value, then open the dynamic binding for the body
        // (WithBind, dynamic.rs), which runs the body under a restoring cleanup cont.
        Node::With { value, .. } => {
            frame.conts.push(Cont::WithBind { with: stmt });
            frame.conts.push(Cont::Eval { node: *value });
        }
        // A name target evaluates only the RHS, then binds. A `Field`/`Index` place
        // target evaluates its object first (left-to-right, L§14): navigate the object
        // as a place (no copy), then finish the store (control.rs / record.rs / dict.rs).
        Node::Assign { target, value } => match resolved.ast.node(*target) {
            Node::Ident(_) => {
                frame.conts.push(Cont::AssignTo { assign: stmt });
                frame.conts.push(Cont::Eval { node: *value });
            }
            _ => {
                let (Node::Field { object, .. } | Node::Index { object, .. }) =
                    resolved.ast.node(*target)
                else {
                    unreachable!("an assignment target is Ident, Field, or Index (L§5.3)");
                };
                frame.conts.push(Cont::AssignPlaceObj { assign: stmt });
                frame.conts.push(Cont::Eval { node: *object });
            }
        },
        Node::If { .. } => control::schedule_if(frame, resolved, stmt),
        Node::While { cond, .. } => {
            frame.conts.push(Cont::WhileCheck { node: stmt });
            frame.conts.push(Cont::Eval { node: *cond });
        }
        Node::Loop { body } => {
            frame.conts.push(Cont::LoopReloop { node: stmt });
            frame.conts.push(Cont::Seq {
                block: *body,
                next: 0,
            });
        }
        // A named `to`/`fn` declaration: intern and bind its callable value when
        // the statement runs (call.rs). Anonymous `fn` never reaches here — it is
        // an expression, wrapped in an `ExprStmt`.
        Node::Callable { .. } => frame.conts.push(Cont::DefineCallable { decl: stmt }),
        // A `record …` declaration binds its type value when the statement runs (L§9);
        // the body is docstring-only, so nothing is evaluated first (record.rs).
        Node::Record { .. } => frame.conts.push(Cont::DefineRecord { decl: stmt }),
        // A non-local exit (§7.10): evaluate its operand (if any), then arm the
        // unwind toward the resolver-annotated target (unwind.rs).
        Node::Return(op) | Node::Break(op) | Node::Continue(op) => {
            frame.conts.push(Cont::ExitApply { exit: stmt });
            if let Some(operand) = op {
                frame.conts.push(Cont::Eval { node: *operand });
            }
        }
        // A `try`: run the protected body under a `TryHandler` (protect.rs).
        Node::Try { .. } => protect::schedule_try(frame, resolved, stmt),
        // A `raise` (L§12.1): evaluate its operand (if any), then throw. A bare `raise`
        // re-raises the exception being handled (RaiseApply, protect.rs).
        Node::Raise(op) => {
            frame.conts.push(Cont::RaiseApply { raise: stmt });
            if let Some(operand) = op {
                frame.conts.push(Cont::Eval { node: *operand });
            }
        }
        other => unimplemented!("statement not yet in the machine (M4+): {other:?}"),
    }
}

/// The top frame's work is drained with no `ReturnBarrier` beneath it: only the
/// module top level ends this way, completing Void (L§6.11) — its final
/// statement's transient value is discarded. A callable frame instead returns
/// through its [`Cont::ReturnBarrier`] ([`call::return_from_callable`]).
fn return_from_top_frame(machine: &mut Machine) {
    let frame = machine.frames.pop().expect("return with no frame");
    match frame.kind {
        FrameKind::ModuleTopLevel => machine.reg = None,
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
    );
    machine.unwind = Some(unwind::Unwind::Raise { value, trace });
}

/// Takes the in-flight Raise unwind's value + trace and clears the transfer — for the
/// drained, uncaught raise reaching the boundary.
fn take_raise(machine: &mut Machine) -> (Value, Trace) {
    match machine.unwind.take() {
        Some(unwind::Unwind::Raise { value, trace }) => (value, trace),
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

/// The statement list of a body node (`Module` or `Block`).
fn stmt_list(node: &Node) -> &[NodeId] {
    match node {
        Node::Module { stmts, .. } => stmts,
        Node::Block(stmts) => stmts,
        other => unreachable!("Seq over a non-body node: {other:?}"),
    }
}
