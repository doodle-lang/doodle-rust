//! The machine's single transition, `step` (machine-design §8): pop the top
//! frame's top continuation and perform one step of work.
//!
//! **Scope (M2a.2).** Statement sequencing, literal evaluation, and
//! module-top-level return (Void). Operators, calls, binding, control flow, and
//! unwinding are added in later chunks (each new [`Cont`] variant gets an arm
//! here). Nodes the machine cannot yet run reach an `unimplemented!` — no
//! production path drives such a program at M2a.2 (`mode: run` conformance still
//! skips), and the message names the chunk that lands the behavior.

use super::cont::Cont;
use super::frame::{Frame, FrameKind};
use super::{Machine, Value};
use crate::ast::{Node, NodeId};
use crate::heap::Heap;
use crate::resolve::ResolvedModule;

/// Performs one machine transition. Precondition: `machine` has at least one
/// frame (the caller checks `is_halted` first).
pub(crate) fn step(resolved: &ResolvedModule, heap: &mut Heap, machine: &mut Machine) {
    // Pop the top frame's top continuation; the borrow ends before we dispatch,
    // so a transition is free to push work back onto the same (or a new) frame.
    let cont = machine
        .frames
        .last_mut()
        .expect("step with no frame")
        .conts
        .pop();
    match cont {
        Some(Cont::Seq { block, next }) => seq_step(resolved, machine, block, next),
        Some(Cont::Eval { node }) => eval(resolved, heap, machine, node),
        // The frame's work is drained: return from it.
        None => return_from_top_frame(machine),
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

/// Evaluates one expression into the result register. Only literals at M2a.2;
/// operators, names, calls, etc. join at M2a.3+.
fn eval(resolved: &ResolvedModule, heap: &mut Heap, machine: &mut Machine, node: NodeId) {
    let value = match resolved.ast.node(node) {
        Node::IntLit(n) => Value::Int(*n),
        Node::FloatLit(x) => Value::Float(*x),
        Node::BoolLit(b) => Value::Bool(*b),
        Node::NilLit => Value::Nil,
        Node::BytesLit(bytes) => Value::Bytes(heap.alloc_bytes(bytes.as_slice().into())),
        other => unimplemented!("expression not yet in the machine (M2a.3+): {other:?}"),
    };
    machine.reg = Some(value);
}

/// Returns from the top frame (its continuations are drained), popping it and
/// delivering its result. A module completes Void (L§6.11) — its final
/// statement's transient value is discarded. Callable-frame returns (which
/// deliver `reg` to the caller) arrive at M2a.5.
fn return_from_top_frame(machine: &mut Machine) {
    let frame = machine.frames.pop().expect("return with no frame");
    match frame.kind {
        FrameKind::ModuleTopLevel => machine.reg = None,
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
