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
use super::error::{ExceptionKind, Raise};
use super::frame::{Frame, FrameKind};
use super::{Halt, Machine, Value, arith, block, call, compare, limits, types, unwind};
use crate::ast::{BinaryOp, Node, NodeId, UnaryOp};
use crate::heap::Heap;
use crate::resolve::ResolvedModule;
use num_bigint::BigInt;

/// Performs one machine transition (machine-design §8), evaluating resource limits
/// at each statement-level safe point (E§7.4, §10.2). Precondition: `machine` has at
/// least one frame (the caller checks `is_halted` first). Returns `Err` if the
/// transition stopped the drive — an uncaught raise or an engine fault.
pub(crate) fn step(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    namespace: &Namespace,
) -> Result<(), Halt> {
    // A non-local transfer in flight takes over the transition (§12): unwind toward
    // the exit's target instead of running continuations normally. Unwinding is
    // finite cleanup toward a target (bounded by the stack it already paid for on
    // the way in), so it does not itself hit safe points — the next normal step,
    // once the unwind settles, does.
    if machine.unwind.is_some() {
        unwind::step(resolved, heap, machine)?;
        return Ok(());
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
    dispatch(resolved, heap, machine, namespace, cont)?;
    if stmt_safe_point {
        limits::safe_point(heap, machine, namespace)?;
    }
    // A call or block invocation just pushed a frame — a **non-tail** entry (a tail
    // call reuses a frame in place, §11): the call-entry safe point, and the only
    // place non-tail stack depth grows.
    let depth = machine.frames.len();
    if depth > depth_before {
        limits::safe_point(heap, machine, namespace)?;
        machine.check_stack_depth(depth)?;
    }
    Ok(())
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
        Some(Cont::Eval { node }) => eval(resolved, heap, machine, namespace, node),
        Some(Cont::BinRhs { op, rhs, span }) => bin_rhs(machine, op, rhs, span),
        Some(Cont::BinApply { op, lhs, span }) => {
            let rhs = take_value(machine, span)?;
            let result = match op {
                // `x is T`: the right operand is a type value (L§6.5).
                BinaryOp::Is => types::is_op(lhs, rhs, heap, span)?,
                _ if is_arithmetic(op) => arith::binary(op, lhs, rhs, heap, span)?,
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
        Some(Cont::AndRhs { rhs, span }) => logical_rhs(machine, rhs, span, true),
        Some(Cont::OrRhs { rhs, span }) => logical_rhs(machine, rhs, span, false),
        Some(Cont::AssertBool { span }) => {
            let v = take_value(machine, span)?;
            machine.reg = Some(Value::Bool(compare::as_bool(v, "and/or", span)?));
            Ok(())
        }
        Some(Cont::BindLet { decl }) => control::bind_let(resolved, heap, machine, namespace, decl),
        Some(Cont::AssignTo { assign }) => {
            control::assign_to(resolved, heap, machine, namespace, assign)
        }
        Some(Cont::IfChoose { node, index }) => control::if_choose(resolved, machine, node, index),
        Some(Cont::WhileCheck { node }) => control::while_check(resolved, machine, node),
        Some(Cont::LoopReloop { node }) => {
            control::loop_reloop(resolved, machine, node);
            Ok(())
        }
        Some(Cont::CallGotCallee { call }) => call::got_callee(resolved, heap, machine, call),
        Some(Cont::CallGotArg {
            call,
            callee,
            values,
            index,
        }) => call::got_arg(resolved, heap, machine, call, callee, values, index),
        Some(Cont::BlockGotArg {
            call,
            values,
            index,
        }) => block::got_block_arg(resolved, heap, machine, call, values, index),
        Some(Cont::BindDefault { slot, default }) => {
            call::bind_default(resolved, heap, machine, slot, default)
        }
        Some(Cont::DefineCallable { decl }) => {
            call::define_callable(resolved, heap, machine, namespace, decl);
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

/// The `and`/`or` short-circuit transition, with the left operand in the register.
/// `and` (is_and) short-circuits to `false` when the left is `false`; `or`
/// short-circuits to `true` when the left is `true`. Otherwise the right operand
/// is evaluated and must itself be a `Bool` (L§6.6), enforced by `AssertBool`.
fn logical_rhs(
    machine: &mut Machine,
    rhs: NodeId,
    span: crate::span::Span,
    is_and: bool,
) -> Result<(), Raise> {
    let op = if is_and { "and" } else { "or" };
    let lhs = compare::as_bool(take_value(machine, span)?, op, span)?;
    if lhs == is_and {
        // `and` with a true left, or `or` with a false left: the result is rhs.
        let frame = machine
            .frames
            .last_mut()
            .expect("logical_rhs with no frame");
        frame.conts.push(Cont::AssertBool { span });
        frame.conts.push(Cont::Eval { node: rhs });
    } else {
        // Short-circuit: `and` false → false; `or` true → true.
        machine.reg = Some(Value::Bool(!is_and));
    }
    Ok(())
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
        // Evaluate the initializer, then bind it (LIFO: the `Eval` runs first).
        Node::Let { value, .. } | Node::Const { value, .. } => {
            frame.conts.push(Cont::BindLet { decl: stmt });
            frame.conts.push(Cont::Eval { node: *value });
        }
        Node::Assign { value, .. } => {
            frame.conts.push(Cont::AssignTo { assign: stmt });
            frame.conts.push(Cont::Eval { node: *value });
        }
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
        // A non-local exit (§7.10): evaluate its operand (if any), then arm the
        // unwind toward the resolver-annotated target (unwind.rs).
        Node::Return(op) | Node::Break(op) | Node::Continue(op) => {
            frame.conts.push(Cont::ExitApply { exit: stmt });
            if let Some(operand) = op {
                frame.conts.push(Cont::Eval { node: *operand });
            }
        }
        other => unimplemented!("statement not yet in the machine (M4+): {other:?}"),
    }
}

/// Evaluates one expression, either producing a value into the register (a leaf)
/// or scheduling continuations that will (a compound operator). Returns `Err` if
/// reading a name raised.
fn eval(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    namespace: &Namespace,
    node: NodeId,
) -> Result<(), Raise> {
    let value = match resolved.ast.node(node) {
        Node::IntLit(n) => Value::Int(*n),
        Node::FloatLit(x) => Value::Float(*x),
        Node::BoolLit(b) => Value::Bool(*b),
        Node::NilLit => Value::Nil,
        Node::BytesLit(bytes) => Value::Bytes(heap.alloc_bytes(bytes.as_slice().into())),
        Node::BigIntLit { radix, digits } => {
            let n = BigInt::parse_bytes(digits.as_bytes(), u32::from(*radix))
                .expect("lexer-validated bignum digits");
            arith::int_value(n, heap)
        }
        Node::Ident(_) => control::read_ref(resolved, heap, machine, namespace, node)?,
        // `if` in expression position: same machinery as the statement form; the
        // selected branch's value stays in the register for the consumer (L§6.8).
        Node::If { .. } => {
            let frame = machine.frames.last_mut().expect("eval with no frame");
            control::schedule_if(frame, resolved, node);
            return Ok(());
        }
        Node::Binary { op, lhs, rhs } => {
            let op = *op;
            let (lhs, rhs) = (*lhs, *rhs);
            let span = resolved.ast.span(node);
            let frame = machine.frames.last_mut().expect("eval with no frame");
            match op {
                // `and`/`or` short-circuit: after lhs, decide whether to run rhs.
                BinaryOp::And => frame.conts.push(Cont::AndRhs { rhs, span }),
                BinaryOp::Or => frame.conts.push(Cont::OrRhs { rhs, span }),
                // Arithmetic, comparison/equality, and `is` strict-evaluate both
                // operands, then apply at `BinApply`.
                _ => frame.conts.push(Cont::BinRhs { op, rhs, span }),
            }
            // Evaluate lhs first (top of the LIFO stack); the pushed cont resumes.
            frame.conts.push(Cont::Eval { node: lhs });
            return Ok(());
        }
        Node::Unary { op, operand } => {
            let op = *op;
            let operand = *operand;
            let span = resolved.ast.span(node);
            let frame = machine.frames.last_mut().expect("eval with no frame");
            frame.conts.push(Cont::UnaryApply { op, span });
            frame.conts.push(Cont::Eval { node: operand });
            return Ok(());
        }
        // A call schedules callee/argument evaluation, then `Apply` (call.rs); a
        // block-parameter invocation takes the block path (block.rs).
        Node::Call { .. } => return call::eval_call(resolved, heap, machine, node),
        // An anonymous `fn` expression interns its own closure value (L§6.10),
        // reading its captured cells from the creating environment (M2a.8).
        Node::Callable { .. } => call::make_callable(resolved, heap, machine, node),
        other => unimplemented!("expression not yet in the machine (M2a.5+): {other:?}"),
    };
    machine.reg = Some(value);
    Ok(())
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

fn bin_rhs(
    machine: &mut Machine,
    op: BinaryOp,
    rhs: NodeId,
    span: crate::span::Span,
) -> Result<(), Raise> {
    let lhs = take_value(machine, span)?;
    let frame = machine.frames.last_mut().expect("bin_rhs with no frame");
    frame.conts.push(Cont::BinApply { op, lhs, span });
    frame.conts.push(Cont::Eval { node: rhs });
    Ok(())
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
