//! The unwind **cleanup** mechanism (machine-design §12/§13): the conts the unwinder
//! executes as it pops. Every exit runs each [`WithRestore`](Cont::WithRestore) it
//! passes ([`restore`], via [`discard_cont`]); a raise additionally unwinds
//! frame-by-frame ([`raise_unwind`]) and catches at the nearest
//! [`TryHandler`](Cont::TryHandler) ([`try_catch`]). The exit *arms* (which decide the
//! target) live in the parent module and call these primitives.

use super::super::Machine;
use super::super::cont::Cont;
use super::super::frame::FrameKind;
use super::super::modload::LoadState;
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
/// binding, and a [`PopHandler`](Cont::PopHandler) pops the handling stack — the same
/// two clean-ups [`raise_unwind`] runs, since a non-local exit (break/continue/return/
/// cancel) abandons a rescue body exactly as a raise unwinding past it does. A
/// [`TryHandler`](Cont::TryHandler) on a non-raise unwind is discarded inertly (only a
/// raise catches there).
pub(super) fn discard_cont(cont: &Cont, machine: &mut Machine, heap: &mut Heap) {
    match cont {
        Cont::WithRestore { dyn_mark } => restore(machine, heap, *dyn_mark),
        // Without this, a `break`/`continue`/`return` out of a rescue body leaves its
        // handling entry on the stack: a later bare `raise` re-raises the stale exception,
        // and the stack grows unbounded (L§12.2).
        Cont::PopHandler => machine.pop_handling(),
        _ => {} // discarded inertly
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
            let frame = machine
                .frames
                .pop()
                .expect("a frame to abandon for a raise");
            // A raise unwinding out of a still-loading module's top level — an importer
            // frame remains beneath (E§6, S-8): its load failed, so mark it `failed`
            // **retaining the raised value** (a re-import re-raises it unchanged), and the
            // raise keeps propagating into the importer (the `import` fails). `raise_unwind`
            // runs only for a `Raise` unwind, so the in-flight value is available.
            if matches!(frame.kind, FrameKind::ModuleTopLevel)
                && !machine.frames.is_empty()
                && let Some(super::Unwind::Raise { value, .. }) = &machine.unwind
            {
                let value = *value;
                machine
                    .load
                    .set_state(frame.module, LoadState::Failed(value));
            }
            return;
        };
        match cont {
            Cont::WithRestore { dyn_mark } => restore(machine, heap, dyn_mark),
            // A rescue body abandoned mid-raise pops its handling entry (L§12.2), like a
            // `WithRestore` — the raise punches through an inner `try`'s handler.
            Cont::PopHandler => machine.pop_handling(),
            Cont::TryHandler { try_node } => {
                try_catch(resolved, heap, machine, try_node);
                return;
            }
            _ => {} // discarded inertly
        }
    }
}

/// Catches a raise at a [`TryHandler`](Cont::TryHandler) (machine-design §12, L§12.2):
/// takes the raised value + trace off the in-flight raise, clears it (the unwind stops
/// here), and hands them to [`protect::catch`](super::super::protect::catch), which binds
/// the rescue variable and schedules the rescue body.
fn try_catch(resolved: &ResolvedModule, heap: &mut Heap, machine: &mut Machine, try_node: NodeId) {
    let Some(super::Unwind::Raise { value, trace }) = machine.unwind.take() else {
        unreachable!("a TryHandler is reached only while a raise is in flight");
    };
    super::super::protect::catch(resolved, heap, machine, try_node, value, trace);
}
