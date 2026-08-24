//! Non-local exits and the unwind mechanism (machine-design §12): `return`,
//! `break`, and `continue`. One in-flight [`Unwind`] record drives the transfer
//! to the exit's **resolver-annotated** target (the resolver decided "nearest
//! loop / block / consumer / home callable" lexically, so the machine performs no
//! dynamic scan).
//!
//! **Scope.** The three intra-instance exits over blocks and loops, with
//! **punch-through** (a `return` in a block exits the writing function, not the
//! block's consumer); cancellation (§12); and (M4.5a) the **cleanup** category the
//! unwinder executes as it pops — every exit runs each
//! [`WithRestore`](super::cont::Cont::WithRestore) it passes ([`restore`]), and a
//! [`Raise`](Unwind::Raise) additionally catches at the nearest
//! [`TryHandler`](super::cont::Cont::TryHandler). The `with`/`parameter` producers
//! (M4.6) and the `try`/`rescue` handler binding (M4.5b) fill in the conts' producers;
//! M4.5a builds the mechanism and routes raises through this channel.

use super::error::{Exception, ExceptionKind, Raise, Trace};
use super::frame::Consumer;
use super::step::take_value;
use super::{Machine, Value};
use crate::ast::{Node, NodeId};
use crate::heap::Heap;
use crate::resolve::{ExitTarget, ResolvedModule};

mod arms;
mod cleanup;
pub(crate) use arms::resume_native_boundary;
use arms::{
    block_break, block_continue, consumer_is_proc, current_block_consumer, do_return,
    home_callable, loop_break, loop_continue, native_break,
};
pub(crate) use cleanup::restore;
use cleanup::{cleanup_and_pop_frame, raise_unwind};

/// An in-flight non-local transfer (machine-design §12). Its target frame/loop is
/// resolved when the exit executes ([`exit_apply`]); the unwinder then pops toward it
/// one continuation or frame at a time, running each
/// [`WithRestore`](super::cont::Cont::WithRestore) it passes. Not `Copy` — a
/// [`Raise`](Unwind::Raise) carries an owned exception.
#[derive(Clone)]
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
    /// `break` in a block invoked by a **native** block-consuming function (E§7.6,
    /// S-46): the target is the native call on the far side of the host boundary,
    /// which has no consumer frame. Pop frames down to `boundary`, then leave the
    /// unwind **parked** — the reentrant nested drive detects it there and relays it
    /// as `NonLocalExit(Break)`, and the native call's apply site
    /// ([`resume_native_boundary`]) completes the call with `value`.
    NativeBreak {
        /// The value the native consuming call produces (`None` for a bare `break`).
        value: Option<Value>,
        /// The native consumer's frame depth — pop down to here, then park.
        boundary: usize,
        /// The `break`'s span, for the valued-`break`-to-a-procedure raise (S-10).
        span: crate::span::Span,
    },
    /// **Cancellation** (E§10.1, §12): the host's stop button. Its target is
    /// *everything* — the unwinder pops every frame (running each frame's `WithRestore`
    /// cleanup as for a raise, but never catching at a `TryHandler`), and once the stack
    /// is empty the drive faults [`Cancelled`](crate::drive::EngineFault::Cancelled), a
    /// non-resumable stop that Doodle code cannot catch. Carries no value.
    Cancel,
    /// A **raise** in flight (machine-design §12; L§12): unwind every frame toward the
    /// nearest [`TryHandler`](super::cont::Cont::TryHandler) — the one genuinely dynamic
    /// target — running each [`WithRestore`](super::cont::Cont::WithRestore) as it pops.
    /// Carries the exception and trace captured at the raise site. A handler catches it;
    /// uncaught, it drains the whole stack and `step` reports the terminal `Raised`
    /// outcome. (The exception is host-facing kind+message today; it becomes a Doodle
    /// **value** — and thus a GC root — at M4.5b.)
    Raise {
        /// The raised exception (E§9).
        exception: Exception,
        /// The trace captured at the raise site (E§8.2).
        trace: Trace,
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
            | Unwind::Return { value, .. }
            | Unwind::NativeBreak { value, .. } => *value,
            // A `Raise`'s exception is host-facing kind+message (no heap value) until
            // exceptions-as-values (M4.5b), when this arm returns the exception value.
            Unwind::LoopBreak { .. }
            | Unwind::LoopContinue { .. }
            | Unwind::Cancel
            | Unwind::Raise { .. } => None,
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
        (Node::Break(_), ExitTarget::ConsumerCall) => match current_block_consumer(machine) {
            Consumer::DoodleCall {
                frame: consumer,
                serial: consumer_serial,
            } => {
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
            // A `break` out of a block invoked by a native block-consuming function
            // crosses the host boundary (E§7.6, S-46): unwind to the native `boundary`
            // and park there. The value-destination check (a valued `break` to a
            // procedure consumer, S-10) happens at the apply site, where the native
            // callee's kind is known ([`resume_native_boundary`]) — its span is carried
            // here for parity with the [`Consumer::DoodleCall`] raise above.
            Consumer::Native { boundary } => Unwind::NativeBreak {
                value,
                boundary,
                span,
            },
        },
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
    heap: &mut Heap,
    machine: &mut Machine,
) -> Result<Option<usize>, Raise> {
    // A raise carries a non-`Copy` exception; it needs none of that here (the exception
    // is read at the terminal drain in [`super::step`]), so handle it before the clone.
    if matches!(machine.unwind, Some(Unwind::Raise { .. })) {
        raise_unwind(resolved, heap, machine);
        return Ok(None);
    }
    match machine
        .unwind
        .clone()
        .expect("unwind::step with no in-flight unwind")
    {
        Unwind::LoopBreak { loop_node } => {
            loop_break(machine, heap, loop_node);
            Ok(None)
        }
        Unwind::LoopContinue { loop_node } => {
            loop_continue(resolved, machine, heap, loop_node);
            Ok(None)
        }
        Unwind::BlockContinue { value } => {
            block_continue(machine, heap, value);
            Ok(None)
        }
        Unwind::BlockBreak {
            value,
            consumer,
            consumer_serial,
        } => Ok(block_break(machine, heap, value, consumer, consumer_serial)),
        // A `return` can fall off the end of a `fn` (a bare `return`), so it alone
        // may raise as it delivers.
        Unwind::Return { value, home } => do_return(resolved, heap, machine, value, home),
        Unwind::NativeBreak { boundary, .. } => {
            native_break(machine, heap, boundary);
            Ok(None)
        }
        // Cancellation (E§10.1): tear the stack down one frame per transition, running
        // each frame's `WithRestore` cleanup as it pops. This reports no settling safe
        // point; [`super::step`] faults `Cancelled` once the stack is empty.
        Unwind::Cancel => {
            cleanup_and_pop_frame(machine, heap);
            Ok(None)
        }
        Unwind::Raise { .. } => unreachable!("a raise unwind is handled before the clone"),
    }
}
