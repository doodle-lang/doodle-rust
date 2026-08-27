//! Cell-boxed frame locals (representation B, machine-design §7/§10): building a
//! frame's slots at entry, and reading / writing / rebinding a slot — dereferencing
//! the heap cell for a captured (cell-boxed) slot.
//!
//! A slot the resolver marked `cell_boxed` (a nested `fn` captured it) is stored in
//! a heap [`CellObj`](crate::heap::CellObj) so the closure and the frame share one
//! mutable binding. At frame entry every cell-boxed slot is given a cell (a capture
//! slot **splices** the closure's captured cell; a parameter's cell holds its value;
//! a body local's cell starts uninitialized and is *replaced* by a fresh cell each
//! time its `let` runs — so per-iteration bindings are distinct, L§5.4 loop-fresh).

use super::Value;
use super::frame::Local;
use crate::heap::{CellKind, Heap};
use crate::machine::CellIdx;
use crate::resolve::ResolvedModule;

/// Reads a slot's value, dereferencing a cell-boxed slot. `None` = uninitialized.
pub(crate) fn read(heap: &Heap, local: Local) -> Option<Value> {
    match local {
        Local::Direct(v) => v,
        Local::Boxed(cell) => heap.cell(cell).value,
    }
}

/// Writes `value` into an **existing** binding — an assignment (`name = value`) or a
/// default fill — mutating the cell for a boxed slot (so captures observe the write).
pub(crate) fn write(heap: &mut Heap, local: &mut Local, value: Value) {
    match local {
        Local::Direct(v) => *v = Some(value),
        Local::Boxed(cell) => heap.cell_mut(*cell).value = Some(value),
    }
}

/// Binds a `let`/`const` value — a **new** binding. A boxed slot gets a **fresh**
/// cell, so a closure captured in a *previous* loop iteration keeps its own binding
/// (loop-fresh, L§5.4); a direct slot is set inline.
pub(crate) fn rebind(heap: &mut Heap, local: &mut Local, value: Value) {
    match local {
        Local::Direct(_) => *local = Local::Direct(Some(value)),
        Local::Boxed(_) => *local = Local::Boxed(heap.alloc_cell(CellKind::Let, Some(value))),
    }
}

/// The cell a capture reads from a source slot (machine-design §7): a cell-boxed
/// slot's cell. Panics on a direct slot — the resolver only sources captures from
/// cell-boxed slots.
pub(crate) fn cell_of(local: Local) -> CellIdx {
    match local {
        Local::Boxed(cell) => cell,
        Local::Direct(_) => unreachable!("a capture source must be a cell-boxed slot"),
    }
}

/// Builds a frame's `locals` at entry (machine-design §7). `raw` holds the bound
/// parameter values in their slots (`None` elsewhere, from `bind_arguments`);
/// `captured` are the closure's captured cells, parallel to the callable's
/// `captures`. Cell-boxed non-capture slots get a fresh cell (a parameter's holds
/// its value; a body local's is uninitialized until its `let`); capture slots
/// splice the captured cells; direct slots hold their value.
pub(crate) fn build(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    callable_id: usize,
    raw: &[Option<Value>],
    captured: &[CellIdx],
) -> Vec<Local> {
    let info = &resolved.callables[callable_id];
    let n = info.slot_count as usize;
    let mut locals = Vec::with_capacity(n);
    for (i, &value) in raw.iter().enumerate().take(n) {
        if info.captures.iter().any(|c| c.slot as usize == i) {
            // A capture slot — spliced below; a placeholder keeps the index aligned.
            locals.push(Local::Direct(None));
        } else if info.cell_boxed[i] {
            locals.push(Local::Boxed(heap.alloc_cell(CellKind::Let, value)));
        } else {
            locals.push(Local::Direct(value));
        }
    }
    for (cs, &cell) in info.captures.iter().zip(captured) {
        locals[cs.slot as usize] = Local::Boxed(cell);
    }
    locals
}
