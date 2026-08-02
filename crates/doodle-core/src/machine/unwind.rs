//! Non-local exits and the unwind mechanism (machine-design §12): `return`,
//! `break`, and `continue`. One in-flight [`Unwind`] record drives the transfer
//! to the exit's **resolver-annotated** target (the resolver decided "nearest
//! loop / block / consumer / home callable" lexically, so the machine performs no
//! dynamic scan).
//!
//! **Scope (M2a.6b).** The three intra-instance exits over blocks and loops, with
//! **punch-through** (a `return` in a block exits the writing function, not the
//! block's consumer). `raise`/`try`/`rescue` (M4) and `with` restoration (the
//! `WithRestore` cont, M4) add cont categories the unwinder executes as it pops;
//! at M2a.6 there are none, so the unwinder pops continuations and frames inertly.
//! Cancellation (§12) joins with the limits (M2a.9).

use super::cont::Cont;
use super::error::{ExceptionKind, Raise};
use super::frame::{Consumer, FrameKind};
use super::step::take_value;
use super::{Machine, Value};
use crate::ast::{Node, NodeId};
use crate::heap::Heap;
use crate::resolve::{BodyKind, ExitTarget, ResolvedModule};

/// An in-flight non-local transfer (machine-design §12). Its target frame/loop is
/// resolved when the exit executes ([`exit_apply`]); the unwinder then pops toward
/// it one continuation or frame at a time.
#[derive(Clone, Copy)]
pub(crate) enum Unwind {
    /// `break` in a loop: pop the current frame's continuations to the matching
    /// loop continuation and discard it (resume after the loop).
    LoopBreak {
        /// The `while`/`loop` node that is the target.
        loop_node: NodeId,
    },
    /// `continue` in a loop: pop to the matching loop continuation and re-enter it
    /// (a `while` re-evaluates its condition; a `loop` re-runs its body).
    LoopContinue {
        /// The `while`/`loop` node that is the target.
        loop_node: NodeId,
    },
    /// `continue` in a block: end this block invocation, delivering `value` to the
    /// invoker (the ordinary block-return path, §8.5).
    BlockContinue {
        /// The value this invocation yields (`None` for a bare `continue`).
        value: Option<Value>,
    },
    /// `break` in a block: exit the block-consuming call, delivering `value` as its
    /// result (§8.5). Unwinds frames through the consumer frame inclusive.
    BlockBreak {
        /// The value the consuming call produces (`None` for a bare `break`).
        value: Option<Value>,
        /// The consuming (invoking) frame's index — the unwind target.
        consumer: usize,
        /// That frame's `serial` (integrity check).
        consumer_serial: u64,
    },
    /// `return`: exit the enclosing callable, delivering `value` as its result and
    /// punching through any intervening blocks/consumers (§12).
    Return {
        /// The returned value (`None` for a `to`'s `return` or a bare `return`).
        value: Option<Value>,
        /// The home callable frame's index — the unwind target.
        home: usize,
    },
}

impl Unwind {
    /// The in-flight value this transfer carries, if any — a GC root (machine-design
    /// §15). A collection cannot currently begin mid-unwind (`step` runs the unwind
    /// path without hitting a safe point), so this is belt-and-suspenders; MD §15
    /// lists `unwind` as a root regardless, and it stays correct if that changes.
    pub(crate) fn gc_value(&self) -> Option<Value> {
        match self {
            Unwind::BlockContinue { value }
            | Unwind::BlockBreak { value, .. }
            | Unwind::Return { value, .. } => *value,
            Unwind::LoopBreak { .. } | Unwind::LoopContinue { .. } => None,
        }
    }
}

/// The exit operand (if any) is now in the register: resolve the exit's target and
/// arm the [`Unwind`]. A valued `break` exiting a **procedure** consumer has no
/// value destination (the open S-10 to-consumer half) and raises provisionally.
pub(crate) fn exit_apply(
    resolved: &ResolvedModule,
    heap: &Heap,
    machine: &mut Machine,
    exit: NodeId,
) -> Result<(), Raise> {
    let span = resolved.ast.span(exit);
    let has_operand = match resolved.ast.node(exit) {
        Node::Return(op) | Node::Break(op) | Node::Continue(op) => op.is_some(),
        _ => unreachable!("exit_apply over a non-exit node"),
    };
    let value = if has_operand {
        Some(take_value(machine, span)?)
    } else {
        None
    };
    let target = resolved.exit_targets[exit.0 as usize].expect("a well-placed exit is annotated");
    let unwind = match (resolved.ast.node(exit), target) {
        (Node::Return(_), ExitTarget::HomeCallable) => Unwind::Return {
            value,
            home: home_callable(machine),
        },
        // A valued `break`/`continue` targeting a loop is a static error (S-10 loop
        // half), so `value` is `None` here.
        (Node::Break(_), ExitTarget::ThisLoop(node)) => Unwind::LoopBreak { loop_node: node },
        (Node::Continue(_), ExitTarget::ThisLoop(node)) => Unwind::LoopContinue { loop_node: node },
        (Node::Continue(_), ExitTarget::ThisBlock) => Unwind::BlockContinue { value },
        (Node::Break(_), ExitTarget::ConsumerCall) => {
            let (consumer, consumer_serial) = current_block_consumer(machine);
            if value.is_some() && consumer_is_proc(resolved, heap, machine, consumer) {
                return Err(Raise::new(
                    ExceptionKind::NoValueDestination,
                    "this `break` gives a value, but the block-consuming call is a procedure, \
                     which yields none",
                    span,
                ));
            }
            Unwind::BlockBreak {
                value,
                consumer,
                consumer_serial,
            }
        }
        _ => unreachable!("resolver-annotated exit kind/target mismatch"),
    };
    machine.unwind = Some(unwind);
    Ok(())
}

/// Performs one unwind transition (machine-design §12). Precondition:
/// `machine.unwind` is `Some`. Returns `Ok(Some(depth))` on the **settling**
/// transition that pops the target frame and returns control to a shallower frame —
/// a return safe point (E§7.4) at that post-pop depth, the same as the fall-through
/// [`ReturnBarrier`](super::cont::Cont::ReturnBarrier) path — so `Step*` (esp.
/// `StepOut`) and the limit checks see it. Intervening pops and in-frame settles
/// (loop `break`/`continue`, block `continue`, which resume in the same frame and
/// let the *next* normal safe point fire) return `Ok(None)`.
pub(crate) fn step(
    resolved: &ResolvedModule,
    heap: &Heap,
    machine: &mut Machine,
) -> Result<Option<usize>, Raise> {
    match machine
        .unwind
        .expect("unwind::step with no in-flight unwind")
    {
        Unwind::LoopBreak { loop_node } => {
            loop_break(machine, loop_node);
            Ok(None)
        }
        Unwind::LoopContinue { loop_node } => {
            loop_continue(resolved, machine, loop_node);
            Ok(None)
        }
        Unwind::BlockContinue { value } => {
            block_continue(machine, value);
            Ok(None)
        }
        Unwind::BlockBreak {
            value,
            consumer,
            consumer_serial,
        } => Ok(block_break(machine, value, consumer, consumer_serial)),
        // A `return` can fall off the end of a `fn` (a bare `return`), so it alone
        // may raise as it delivers.
        Unwind::Return { value, home } => do_return(resolved, heap, machine, value, home),
    }
}

/// `break` in a loop: pop continuations off the current frame until the matching
/// loop continuation is discarded — resuming after the loop.
fn loop_break(machine: &mut Machine, loop_node: NodeId) {
    let frame = machine.frames.last_mut().expect("loop break with no frame");
    let cont = frame
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
    }
}

/// `continue` in a loop: pop to the matching loop continuation, then re-enter it —
/// a `while` re-evaluates its condition (so `WhileCheck` re-checks); a `loop`'s
/// `LoopReloop` re-runs the body when it next executes.
fn loop_continue(resolved: &ResolvedModule, machine: &mut Machine, loop_node: NodeId) {
    let frame = machine
        .frames
        .last_mut()
        .expect("loop continue with no frame");
    if !matches!(frame.conts.last(), Some(c) if is_loop_cont(c, loop_node)) {
        frame.conts.pop();
        return;
    }
    // The target loop continuation is on top. A `while` needs its condition
    // re-evaluated before `WhileCheck` re-checks; a `loop`'s `LoopReloop` re-runs
    // the body on its own.
    if let Some(Cont::WhileCheck { node }) = frame.conts.last() {
        let cond = while_cond(resolved, *node);
        frame.conts.push(Cont::Eval { node: cond });
    }
    machine.unwind = None;
}

/// `continue` in a block: deliver `value` as the block's yield to its invoker, then
/// let the block frame's [`ReturnBarrier`](Cont::ReturnBarrier) finish it normally.
fn block_continue(machine: &mut Machine, value: Option<Value>) {
    let frame = machine
        .frames
        .last_mut()
        .expect("block continue with no frame");
    if matches!(frame.conts.last(), Some(Cont::ReturnBarrier)) {
        machine.reg = value;
        machine.unwind = None;
    } else {
        frame.conts.pop();
    }
}

/// `break` in a block: pop frames (the block, then any intervening frames) through
/// the consumer frame inclusive, delivering `value` as the consuming call's result.
/// Returns `Some(post-pop depth)` on the settling transition that pops the consumer
/// (a return safe point), `None` for an intervening pop.
fn block_break(
    machine: &mut Machine,
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
        machine.frames.pop();
        machine.reg = value;
        machine.unwind = None;
        Some(machine.frames.len())
    } else {
        // An intervening frame (the block, or a punched-through consumer); its
        // continuations are abandoned (no `WithRestore` to run until M4).
        machine.frames.pop();
        None
    }
}

/// `return`: pop frames through the home callable inclusive (punching through any
/// intervening blocks/consumers), delivering `value` as the callable's result. A
/// bare `return` in a `fn` delivers no value — the function fell off the end
/// (L§8.4), the same rule the [`ReturnBarrier`](super::cont::Cont::ReturnBarrier)
/// applies on fall-through — so this delivery raises `FunctionFellOffEnd`.
fn do_return(
    resolved: &ResolvedModule,
    heap: &Heap,
    machine: &mut Machine,
    value: Option<Value>,
    home: usize,
) -> Result<Option<usize>, Raise> {
    let top = machine.frames.len() - 1;
    if top != home {
        // An intervening frame (a punched-through block/consumer); keep unwinding.
        machine.frames.pop();
        return Ok(None);
    }
    let fell_off = match machine.frames[home].kind {
        FrameKind::Callable { cal } => {
            let id = heap.callable(cal).source_id() as usize;
            resolved.callables[id].kind == BodyKind::Func && value.is_none()
        }
        _ => false,
    };
    let home_frame = machine.frames.pop().expect("the home frame");
    machine.unwind = None;
    if fell_off {
        let FrameKind::Callable { cal } = home_frame.kind else {
            unreachable!("fell_off implies a callable home");
        };
        let id = heap.callable(cal).source_id() as usize;
        return Err(Raise::new(
            ExceptionKind::FunctionFellOffEnd,
            "this function reached its end without producing a value",
            resolved.ast.span(resolved.callables[id].decl),
        ));
    }
    machine.reg = value;
    Ok(Some(machine.frames.len()))
}

/// The home callable frame for a `return` (machine-design §12): the current frame
/// if it is a callable, else the frame reached by chasing the block `defining`
/// chain to the lexically enclosing callable (never the block's dynamic consumer).
fn home_callable(machine: &Machine) -> usize {
    let mut idx = machine.frames.len() - 1;
    loop {
        match &machine.frames[idx].kind {
            FrameKind::Block { defining, .. } => idx = *defining,
            // A callable (a `return`'s only legal home — the resolver requires it).
            FrameKind::Callable { .. } | FrameKind::ModuleTopLevel => return idx,
        }
    }
}

/// The consumer of the current block frame (who invoked it) — the `break` target.
fn current_block_consumer(machine: &Machine) -> (usize, u64) {
    let FrameKind::Block {
        consumer: Consumer::DoodleCall { frame, serial },
        ..
    } = &machine
        .frames
        .last()
        .expect("block break with no frame")
        .kind
    else {
        unreachable!("a block `break` outside a block frame with a Doodle consumer");
    };
    (*frame, *serial)
}

/// Whether the frame at `consumer` runs a procedure (a `to`) — for the open S-10
/// to-consumer half, where a valued `break` has no value destination.
fn consumer_is_proc(
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
