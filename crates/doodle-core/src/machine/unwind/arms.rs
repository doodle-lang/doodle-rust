//! The exit **arms** (machine-design §12): the per-transfer frame/cont manipulation the
//! unwinder dispatches to — loop/block `break`/`continue`, `return` (punch-through to the
//! home callable), and the native-boundary resolution (S-46). Each abandons frames or
//! conts via the shared [`cleanup`](super::cleanup) primitives so a `with` restore runs
//! as it passes. The transfer *setup* (`exit_apply`) and the dispatcher (`step`) stay in
//! the parent module.

use super::super::cont::Cont;
use super::super::error::{ExceptionKind, Raise};
use super::super::frame::{Consumer, FrameKind};
use super::super::{Machine, Value};
use super::Unwind;
use super::cleanup::{cleanup_and_pop_frame, discard_cont};
use crate::ast::{Node, NodeId};
use crate::heap::Heap;
use crate::resolve::{BodyKind, ResolvedModule};

/// `break` in a loop: pop continuations off the current frame until the matching
/// loop continuation is discarded — resuming after the loop.
pub(super) fn loop_break(machine: &mut Machine, heap: &mut Heap, loop_node: NodeId) {
    let cont = machine
        .frames
        .last_mut()
        .expect("loop break with no frame")
        .conts
        .pop()
        .expect("the target loop continuation must be on the stack");
    if is_loop_cont(&cont, loop_node) {
        // A `while`/`loop` yields Void (L§7.6/§7.7); a `break` skips the condition
        // re-eval that would otherwise clear the register, so clear it here — the
        // loop must not leak its last body value (the seq boundary clears it too,
        // but make the loop-yields-Void invariant explicit at the exit).
        machine.reg = None;
        machine.unwind = None;
    } else {
        // A cont discarded on the way to the loop cont: a `break` from inside a `with`
        // body punches through it, running its restore (machine-design §12, S-9).
        discard_cont(&cont, machine, heap);
    }
}

/// `continue` in a loop: pop to the matching loop continuation, then re-enter it —
/// a `while` re-evaluates its condition (so `WhileCheck` re-checks); a `loop`'s
/// `LoopReloop` re-runs the body when it next executes.
pub(super) fn loop_continue(
    resolved: &ResolvedModule,
    machine: &mut Machine,
    heap: &mut Heap,
    loop_node: NodeId,
) {
    let at_target = matches!(
        machine.frames.last().expect("loop continue with no frame").conts.last(),
        Some(c) if is_loop_cont(c, loop_node)
    );
    if !at_target {
        // A cont discarded on the way to the loop cont: run its `with` restore (S-9).
        let cont = machine
            .frames
            .last_mut()
            .expect("loop continue with no frame")
            .conts
            .pop()
            .expect("a continuation to discard toward the loop cont");
        discard_cont(&cont, machine, heap);
        return;
    }
    // The target loop continuation is on top. A `while` needs its condition
    // re-evaluated before `WhileCheck` re-checks; a `loop`'s `LoopReloop` re-runs
    // the body on its own.
    let frame = machine
        .frames
        .last_mut()
        .expect("loop continue with no frame");
    if let Some(Cont::WhileCheck { node }) = frame.conts.last() {
        let cond = while_cond(resolved, *node);
        frame.conts.push(Cont::Eval { node: cond });
    }
    machine.unwind = None;
}

/// `continue` in a block: deliver `value` as the block's yield to its invoker, then
/// let the block frame's [`ReturnBarrier`](Cont::ReturnBarrier) finish it normally.
pub(super) fn block_continue(machine: &mut Machine, heap: &mut Heap, value: Option<Value>) {
    let at_barrier = matches!(
        machine
            .frames
            .last()
            .expect("block continue with no frame")
            .conts
            .last(),
        Some(Cont::ReturnBarrier)
    );
    if at_barrier {
        machine.reg = value;
        machine.unwind = None;
    } else {
        // A cont discarded on the way to the block's ReturnBarrier: run its `with`
        // restore (a `continue` from inside a `with` body punches through it, S-9).
        let cont = machine
            .frames
            .last_mut()
            .expect("block continue with no frame")
            .conts
            .pop()
            .expect("a continuation to discard toward the ReturnBarrier");
        discard_cont(&cont, machine, heap);
    }
}

/// `break` in a block: pop frames (the block, then any intervening frames) through
/// the consumer frame inclusive, delivering `value` as the consuming call's result.
/// Returns `Some(post-pop depth)` on the settling transition that pops the consumer
/// (a return safe point), `None` for an intervening pop.
pub(super) fn block_break(
    machine: &mut Machine,
    heap: &mut Heap,
    value: Option<Value>,
    consumer: usize,
    serial: u64,
) -> Option<usize> {
    let top = machine.frames.len() - 1;
    if top == consumer {
        debug_assert_eq!(
            machine.frames[top].serial, serial,
            "stale consumer link — a frame slot was reused (machine-design §8)"
        );
        cleanup_and_pop_frame(machine, heap);
        machine.reg = value;
        machine.unwind = None;
        Some(machine.frames.len())
    } else {
        // An intervening frame (the block, or a punched-through consumer): abandon it,
        // running each `WithRestore` it holds (machine-design §12).
        cleanup_and_pop_frame(machine, heap);
        None
    }
}

/// `break` targeting a **native** block-consumer (E§7.6, S-46): pop one frame toward
/// the native `boundary`. There is no consumer frame at the boundary, so this never
/// pops it — once the stack drains to `boundary` the reentrant nested drive
/// ([`invoke_block`](super::intrinsic)) sees the still-parked unwind and relays it as
/// `NonLocalExit(Break)`; the native call's apply site then completes the call
/// ([`resume_native_boundary`]). The nested drive stops stepping the moment the stack
/// reaches `boundary`, so this is only reached with a frame above it to pop.
pub(super) fn native_break(machine: &mut Machine, heap: &mut Heap, boundary: usize) {
    debug_assert!(
        machine.frames.len() > boundary,
        "native_break at/under its boundary — the nested drive should have caught it"
    );
    cleanup_and_pop_frame(machine, heap);
}

/// Resolves a parked non-local exit at a native block-consumer's apply site (E§7.6,
/// S-46), given the native call's `boundary` (frame depth) and `kind`. Precondition:
/// `machine.unwind` is `Some` (the reentrant nested drive returned `NonLocalExit`).
///
/// A `break` targeting *this* call ([`Unwind::NativeBreak`] at `boundary`) **completes
/// it**: the unwind clears and its value becomes the call's result (Void for a `to`),
/// returning `true` — the drive resumes normally. A valued `break` to a **procedure**
/// consumer has no value destination (the open S-10 half), so it raises for parity
/// with the [`Consumer::DoodleCall`] path. Any other in-flight exit (a `return`, or a
/// `break` aimed at a construct enclosing this call) is left parked and returns
/// `false` — it keeps unwinding past this call in the enclosing drive.
pub(crate) fn resume_native_boundary(
    machine: &mut Machine,
    boundary: usize,
    kind: BodyKind,
) -> Result<bool, Raise> {
    let Some(Unwind::NativeBreak {
        value,
        boundary: target,
        span,
    }) = &machine.unwind
    else {
        return Ok(false);
    };
    let (value, target, span) = (*value, *target, *span);
    if target != boundary {
        // A `break` aimed at an enclosing native consumer: keep it parked (this only
        // arises with nested native consumers — the inner apply resumes its own break;
        // an outer one propagates here). Left in flight for the enclosing drive.
        return Ok(false);
    }
    // Consume the parked unwind before delivering or raising.
    machine.unwind = None;
    if value.is_some() && kind == BodyKind::Proc {
        return Err(Raise::new(
            ExceptionKind::NoValueDestination,
            "this `break` gives a value, but the block-consuming call is a procedure, \
             which yields none",
            span,
        ));
    }
    machine.reg = value;
    Ok(true)
}

/// `return`: pop frames through the home callable inclusive (punching through any
/// intervening blocks/consumers), delivering `value` as the callable's result. A
/// bare `return` in a `fn` delivers no value — the function fell off the end
/// (L§8.4), the same rule the [`ReturnBarrier`](super::cont::Cont::ReturnBarrier)
/// applies on fall-through — so this delivery raises `FunctionFellOffEnd`.
pub(super) fn do_return(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    value: Option<Value>,
    home: usize,
) -> Result<Option<usize>, Raise> {
    let top = machine.frames.len() - 1;
    if top != home {
        // An intervening frame (a punched-through block/consumer): abandon it, running
        // each `WithRestore` it holds, then keep unwinding.
        cleanup_and_pop_frame(machine, heap);
        return Ok(None);
    }
    // A bare `return` from a `fn` (no value) fell off the end (L§8.4) → raise. Read the
    // home frame's declaration before it is cleaned up and popped.
    let fell_off_decl = match machine.frames[home].kind {
        FrameKind::Callable { cal } => {
            let id = heap.callable(cal).source_id() as usize;
            (resolved.callables[id].kind == BodyKind::Func && value.is_none())
                .then(|| resolved.callables[id].decl)
        }
        _ => None,
    };
    cleanup_and_pop_frame(machine, heap);
    machine.unwind = None;
    if let Some(decl) = fell_off_decl {
        return Err(Raise::new(
            ExceptionKind::FunctionFellOffEnd,
            "this function reached its end without producing a value",
            resolved.ast.span(decl),
        ));
    }
    machine.reg = value;
    Ok(Some(machine.frames.len()))
}

/// The home callable frame for a `return` (machine-design §12): the current frame
/// if it is a callable, else the frame reached by chasing the block `defining`
/// chain to the lexically enclosing callable (never the block's dynamic consumer).
pub(super) fn home_callable(machine: &Machine) -> usize {
    let mut idx = machine.frames.len() - 1;
    loop {
        match &machine.frames[idx].kind {
            FrameKind::Block { defining, .. } => idx = *defining,
            // A callable (a `return`'s only legal home — the resolver requires it).
            FrameKind::Callable { .. } | FrameKind::ModuleTopLevel => return idx,
        }
    }
}

/// The consumer of the current block frame (who invoked it) — the `break` target: a
/// Doodle frame, or a **native** boundary (a block invoked by a native block-consuming
/// function, MD §14).
pub(super) fn current_block_consumer(machine: &Machine) -> Consumer {
    let FrameKind::Block { consumer, .. } = &machine
        .frames
        .last()
        .expect("block break with no frame")
        .kind
    else {
        unreachable!("a block `break` outside a block frame");
    };
    *consumer
}

/// Whether the frame at `consumer` runs a procedure (a `to`) — for the open S-10
/// to-consumer half, where a valued `break` has no value destination.
pub(super) fn consumer_is_proc(
    resolved: &ResolvedModule,
    heap: &Heap,
    machine: &Machine,
    consumer: usize,
) -> bool {
    match &machine.frames[consumer].kind {
        FrameKind::Callable { cal } => matches!(
            resolved.callables[heap.callable(*cal).source_id() as usize].kind,
            BodyKind::Proc
        ),
        _ => false,
    }
}

/// Whether `cont` is the loop continuation for `loop_node` (its `while`/`loop`
/// node), which `break`/`continue` target.
fn is_loop_cont(cont: &Cont, loop_node: NodeId) -> bool {
    matches!(
        cont,
        Cont::WhileCheck { node } | Cont::LoopReloop { node } if *node == loop_node
    )
}

/// The condition expression of a `while` node.
fn while_cond(resolved: &ResolvedModule, node: NodeId) -> NodeId {
    match resolved.ast.node(node) {
        Node::While { cond, .. } => *cond,
        _ => unreachable!("while_cond over a non-While node"),
    }
}
