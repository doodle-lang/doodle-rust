//! Protected execution: `try`/`rescue` and `raise` (L§12; machine-design §12). A `try`
//! runs its body under a [`TryHandler`](Cont::TryHandler) — the raise catch-point. A
//! raise unwinds to the nearest one ([`catch`]), which binds the caught value to the
//! rescue variable and runs the rescue body. `raise value` throws any value; a bare
//! `raise` re-raises the exception currently being handled, with its original trace.

use super::cont::Cont;
use super::error::{Raise, Trace};
use super::frame::Frame;
use super::step::take_value;
use super::{Machine, Value, local, record, unwind};
use crate::ast::{Node, NodeId};
use crate::heap::Heap;
use crate::resolve::{Resolution, ResolvedModule};

/// The machine state exceptions-as-values (L§12) needs: arming a value-carrying raise
/// and the handling stack a bare re-raise reads. (An `impl Machine` block here, like
/// `load.rs`/`lifecycle.rs` carry `impl Instance`, keeps `machine.rs` within its length
/// budget.)
impl Machine {
    /// Arms an in-flight **Raise unwind** (machine-design §12) carrying an already-built
    /// value — a program `raise value` or a host raise (E§7.5). The unwinder then runs
    /// `WithRestore` cleanup and seeks a `TryHandler`, or drains to the terminal `Raised`.
    pub(crate) fn arm_raise_value(&mut self, value: Value, trace: Trace) {
        self.unwind = Some(unwind::Unwind::Raise {
            value,
            trace,
            trapped: false,
        });
    }

    /// Pushes the exception a rescue body is now handling (L§12.2), for a bare re-raise.
    pub(crate) fn push_handling(&mut self, value: Value, trace: Trace) {
        self.handling.push((value, trace));
    }

    /// The exception currently being handled (the top of the handling stack), if any —
    /// what a bare `raise` re-raises, with its original trace (L§12.1).
    pub(crate) fn current_handling(&self) -> Option<(Value, Trace)> {
        self.handling.last().cloned()
    }

    /// Pops the exception being handled as its rescue body finishes (a `PopHandler`).
    pub(crate) fn pop_handling(&mut self) {
        self.handling.pop();
    }
}

/// Schedules a `try` (L§12.2), in statement or expression position (L§6.9): run the
/// protected body under a `TryHandler`. If the body completes normally the handler is
/// discarded and the body's value is the `try`'s value; a raise reaching the handler runs
/// [`catch`], whose rescue body's value is the `try`'s value instead.
pub(crate) fn schedule_try(frame: &mut Frame, resolved: &ResolvedModule, node: NodeId) {
    let body = match resolved.ast.node(node) {
        Node::Try { body, .. } => *body,
        _ => unreachable!("schedule_try over a non-Try node"),
    };
    // LIFO: run the body first; the `TryHandler` beneath it catches a raise, or is
    // discarded on normal completion (`step`'s dispatch).
    frame.conts.push(Cont::TryHandler { try_node: node });
    frame.conts.push(Cont::Seq {
        block: body,
        next: 0,
    });
}

/// Catches a raise at a `try`'s `TryHandler` (L§12.2): binds the caught `value` to the
/// rescue variable's slot (recorded on the `Try` node by the resolver), pushes the
/// exception onto the handling stack — with its original `trace`, for a bare re-raise —
/// and schedules the rescue body under a [`PopHandler`](Cont::PopHandler) that clears the
/// entry on any exit. The rescue body's value becomes the `try`'s value (L§6.9).
pub(crate) fn catch(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    try_node: NodeId,
    value: Value,
    trace: Trace,
) {
    let rescue_body = match resolved.ast.node(try_node) {
        Node::Try { rescue_body, .. } => *rescue_body,
        _ => unreachable!("catch over a non-Try node"),
    };
    let Some(Resolution::LocalSlot(slot)) = resolved.resolutions[try_node.0 as usize] else {
        unreachable!("a resolved `try` records its rescue binding as a local slot");
    };
    // Bind the caught value to the rescue variable (a fresh binding, loop-fresh) — a
    // value record is copied like any binding (L§4.14). The handling stack keeps the
    // original value + trace, so a bare re-raise re-raises exactly what was caught.
    let bound = record::copy_on_bind(value, heap);
    let top = machine.frames.len() - 1;
    local::rebind(heap, &mut machine.frames[top].locals[slot as usize], bound);
    machine.push_handling(value, trace);
    let frame = machine.frames.last_mut().expect("a frame is active");
    frame.conts.push(Cont::PopHandler);
    frame.conts.push(Cont::Seq {
        block: rescue_body,
        next: 0,
    });
}

/// Applies a `raise` (L§12.1): `raise value` arms a Raise unwind carrying the evaluated
/// value (now in the register); a bare `raise` re-raises the exception currently being
/// handled — the top of the handling stack — with its **original** trace.
pub(crate) fn raise_apply(
    resolved: &ResolvedModule,
    heap: &Heap,
    machine: &mut Machine,
    node: NodeId,
) -> Result<(), Raise> {
    let span = resolved.ast.span(node);
    let (value, trace) = match resolved.ast.node(node) {
        Node::Raise(Some(_)) => {
            // `raise value`: throw the evaluated value with a trace captured here.
            let value = take_value(machine, span)?;
            let trace = super::observe::capture_trace(resolved, heap, machine, Some(span));
            (value, trace)
        }
        // A bare `raise` re-raises the exception being handled, with its **original**
        // trace (L§12.1). Valid only inside a rescue body, so the handling stack is
        // non-empty there.
        Node::Raise(None) => machine
            .current_handling()
            .expect("a bare `raise` outside a rescue body"),
        _ => unreachable!("raise_apply over a non-Raise node"),
    };
    machine.arm_raise_value(value, trace);
    Ok(())
}
