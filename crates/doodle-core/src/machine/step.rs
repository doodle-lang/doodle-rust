//! The machine's single transition, `step` (machine-design §8): pop the top
//! frame's top continuation and perform one step of work.
//!
//! **Scope (M2a.3a).** Statement sequencing, literal + arithmetic evaluation
//! (`+ - * / // % **`, unary `- +`), module-top-level return (Void), and the
//! **raise** path — a failing operation returns `Err(Raise)`, which the drive
//! loop turns into `Raised` (there are no handlers yet; `try`/`rescue` is M4, and
//! the §12 unwind mechanism arrives at M2a.6). Comparison/equality/logical
//! operators and `not` are M2a.3b; other node kinds reach an `unimplemented!`.

use super::cont::Cont;
use super::error::{ExceptionKind, Raise};
use super::frame::{Frame, FrameKind};
use super::{Machine, Value, arith};
use crate::ast::{BinaryOp, Node, NodeId, UnaryOp};
use crate::heap::Heap;
use crate::resolve::ResolvedModule;
use num_bigint::BigInt;

/// Performs one machine transition. Precondition: `machine` has at least one
/// frame (the caller checks `is_halted` first). Returns `Err` if the transition
/// raised a runtime error.
pub(crate) fn step(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
) -> Result<(), Raise> {
    // Pop the top frame's top continuation; the borrow ends before we dispatch,
    // so a transition is free to push work back onto the same (or a new) frame.
    let cont = machine
        .frames
        .last_mut()
        .expect("step with no frame")
        .conts
        .pop();
    match cont {
        Some(Cont::Seq { block, next }) => {
            seq_step(resolved, machine, block, next);
            Ok(())
        }
        Some(Cont::Eval { node }) => {
            eval(resolved, heap, machine, node);
            Ok(())
        }
        Some(Cont::BinRhs { op, rhs, span }) => bin_rhs(machine, op, rhs, span),
        Some(Cont::BinApply { op, lhs, span }) => {
            let rhs = take_value(machine, span)?;
            machine.reg = Some(arith::binary(op, lhs, rhs, heap, span)?);
            Ok(())
        }
        Some(Cont::UnaryApply { op, span }) => {
            let operand = take_value(machine, span)?;
            machine.reg = Some(arith::unary(op, operand, heap, span)?);
            Ok(())
        }
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
        other => unimplemented!("statement not yet in the machine (M2a.4+): {other:?}"),
    }
}

/// Evaluates one expression, either producing a value into the register (a leaf)
/// or scheduling continuations that will (a compound operator).
fn eval(resolved: &ResolvedModule, heap: &mut Heap, machine: &mut Machine, node: NodeId) {
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
        Node::Binary { op, lhs, rhs } => {
            let op = *op;
            if !is_arithmetic(op) {
                unimplemented!("operator not yet in the machine (M2a.3b): {op:?}");
            }
            let (lhs, rhs) = (*lhs, *rhs);
            let span = resolved.ast.span(node);
            let frame = machine.frames.last_mut().expect("eval with no frame");
            // Evaluate lhs first (top of the LIFO stack), then BinRhs takes over.
            frame.conts.push(Cont::BinRhs { op, rhs, span });
            frame.conts.push(Cont::Eval { node: lhs });
            return;
        }
        Node::Unary { op, operand } => {
            let op = *op;
            if matches!(op, UnaryOp::Not) {
                unimplemented!("`not` not yet in the machine (M2a.3b)");
            }
            let operand = *operand;
            let span = resolved.ast.span(node);
            let frame = machine.frames.last_mut().expect("eval with no frame");
            frame.conts.push(Cont::UnaryApply { op, span });
            frame.conts.push(Cont::Eval { node: operand });
            return;
        }
        other => unimplemented!("expression not yet in the machine (M2a.3b+): {other:?}"),
    };
    machine.reg = Some(value);
}

/// After the top frame returns, delivering its result. A module completes Void
/// (L§6.11) — its final statement's transient value is discarded. Callable-frame
/// returns (which deliver `reg` to the caller) arrive at M2a.5.
fn return_from_top_frame(machine: &mut Machine) {
    let frame = machine.frames.pop().expect("return with no frame");
    match frame.kind {
        FrameKind::ModuleTopLevel => machine.reg = None,
    }
}

/// Takes the register's value, raising if it is Void (L§6.11): a procedure result
/// used where a value is required. (Structural backstop for the resolver's static
/// S-6 check; reachable dynamically once calls can return Void, M2a.5.)
fn take_value(machine: &mut Machine, span: crate::span::Span) -> Result<Value, Raise> {
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
