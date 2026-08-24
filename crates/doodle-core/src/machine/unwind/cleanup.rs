//! The unwind **cleanup** mechanism (machine-design §12/§13): the conts the unwinder
//! executes as it pops. Every exit runs each [`WithRestore`](Cont::WithRestore) it
//! passes ([`restore`], via [`discard_cont`]); a raise additionally unwinds
//! frame-by-frame ([`raise_unwind`]) and catches at the nearest
//! [`TryHandler`](Cont::TryHandler) ([`try_catch`]). The exit *arms* (which decide the
//! target) live in the parent module and call these primitives.

use super::super::Machine;
use super::super::cont::Cont;
use crate::ast::NodeId;
use crate::heap::Heap;
use crate::resolve::ResolvedModule;

/// Restores dynamic bindings down to `dyn_mark` (machine-design §13): pop each
/// `(cell, old_value)` above the mark off the save stack, writing the saved value back
/// into its cell. Run by a [`WithRestore`](Cont::WithRestore) on normal completion and
/// by the unwinder as it pops past one on any exit.
pub(crate) fn restore(machine: &mut Machine, heap: &mut Heap, dyn_mark: u32) {
    while machine.dyn_stack.len() as u32 > dyn_mark {
        let (cell, old) = machine
            .dyn_stack
            .pop()
            .expect("a dyn_stack entry above the restore mark");
        heap.cell_mut(cell).value = Some(old);
    }
}

/// Runs the cleanup a discarded continuation carries as the unwinder pops past it
/// (machine-design §12): a [`WithRestore`](Cont::WithRestore) restores its dynamic
/// binding; every other cont — including a [`TryHandler`](Cont::TryHandler) on a
/// non-raise unwind — is discarded inertly.
pub(super) fn discard_cont(cont: &Cont, machine: &mut Machine, heap: &mut Heap) {
    if let Cont::WithRestore { dyn_mark } = cont {
        restore(machine, heap, *dyn_mark);
    }
}

/// Pops the top frame, running each [`WithRestore`](Cont::WithRestore) it still holds
/// as the frame is abandoned (machine-design §12). Used by the frame-popping exits
/// (`break`/`return`/cancel); a `TryHandler` here pops inertly — only a raise stops at
/// one ([`raise_unwind`]).
pub(super) fn cleanup_and_pop_frame(machine: &mut Machine, heap: &mut Heap) {
    while let Some(cont) = machine
        .frames
        .last_mut()
        .expect("a frame to unwind")
        .conts
        .pop()
    {
        discard_cont(&cont, machine, heap);
    }
    machine.frames.pop();
}

/// Unwinds one frame for a raise (machine-design §12): pop the top frame's conts,
/// running each [`WithRestore`](Cont::WithRestore); if a [`TryHandler`](Cont::TryHandler)
/// is reached, the raise is caught there ([`try_catch`]) and the unwind stops. Otherwise
/// the frame is abandoned and unwinding continues. An uncaught raise drains the stack —
/// [`super::super::step`] then reports the terminal `Raised`.
pub(super) fn raise_unwind(resolved: &ResolvedModule, heap: &mut Heap, machine: &mut Machine) {
    loop {
        let Some(cont) = machine
            .frames
            .last_mut()
            .expect("a frame to unwind for a raise")
            .conts
            .pop()
        else {
            // The frame's conts are exhausted with no handler: abandon it, keep unwinding.
            machine.frames.pop();
            return;
        };
        match cont {
            Cont::WithRestore { dyn_mark } => restore(machine, heap, dyn_mark),
            Cont::TryHandler { try_node } => {
                try_catch(resolved, machine, try_node);
                return;
            }
            _ => {} // discarded inertly
        }
    }
}

/// Catches a raise at a [`TryHandler`](Cont::TryHandler) (machine-design §12): stops the
/// unwind and hands off to the rescue handler.
///
/// **M4.5a** installs the recognition seam — it clears the in-flight raise so unwinding
/// halts at this frame (the `TryHandler`'s frame, whose remaining conts stay in place).
/// **M4.5b** fills in binding the caught exception value to the `Try`'s `rescue_name`
/// and entering `rescue_body`, leaving its value in the register as the `try`'s value.
/// There is no `try` producer until M4.5b, so this is reached only via a direct test.
fn try_catch(_resolved: &ResolvedModule, machine: &mut Machine, _try_node: NodeId) {
    machine.unwind = None;
    // M4.5b: bind `rescue_name` = the caught exception value, then run `rescue_body`.
}
