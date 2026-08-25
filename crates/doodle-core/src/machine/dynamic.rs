//! Dynamic parameters at run time (L§5.5, machine-design §13). A `parameter`
//! declares a module cell holding the current value; `with p = v do … end` opens a
//! dynamic binding for the body's dynamic extent and restores it on **every** exit.
//!
//! A `parameter`'s default is seeded like any global initializer (the shared
//! `BindLet` path), and a read of a dynamic parameter is an ordinary module-cell load
//! (`control::read_ref`) — so both ride existing machinery. What is new here is the
//! `with` producer: [`with_bind`] looks up the parameter cell, pushes `(cell, old)`
//! onto `dyn_stack`, writes the new value, and schedules the body under a
//! [`WithRestore`](Cont::WithRestore) marking that `dyn_stack` position. The restore
//! itself — on normal completion and on every unwind tier — is the cleanup mechanism
//! in `unwind::restore`.

use super::cont::Cont;
use super::error::Raise;
use super::step::take_value;
use super::{Machine, control, record};
use crate::ast::{Node, NodeId};
use crate::heap::Heap;
use crate::machine::control::Namespace;
use crate::resolve::{Resolution, ResolvedModule};

/// Opens a `with`'s dynamic binding once its value is in the register (§5.5): resolve
/// the named `parameter` cell, save its current value on `dyn_stack`, write the new
/// value in, and schedule the body under a [`WithRestore`](Cont::WithRestore) that pops
/// the save on every exit (normal, `break`/`continue`/`return`, raise, cancel). The
/// body's value stays in the register and becomes the `with`'s value; restoration
/// touches only the cell and the save stack.
pub(crate) fn with_bind(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    namespace: &Namespace,
    with_node: NodeId,
) -> Result<(), Raise> {
    let Node::With { body, .. } = resolved.ast.node(with_node) else {
        unreachable!("with_bind over a non-With node");
    };
    let body = *body;
    // The resolver records the dynamic-parameter name as a free module-name reference
    // on the `with` node itself; its cell is a module global.
    let Some(Resolution::ModuleName(idx)) = resolved.resolutions[with_node.0 as usize] else {
        unreachable!("a `with` records its parameter name as a module-name reference");
    };
    let span = resolved.ast.span(with_node);
    let value = take_value(machine, span)?;
    let name = &resolved.name_refs[idx as usize].name;
    let (cell, old) = control::param_cell(heap, namespace, name, span)?;
    // Establishing the binding copies a value record, like any binding (L§4.14); the
    // saved `old` is likewise a value that was copied when it was itself bound.
    let value = record::copy_on_bind(value, heap);
    let dyn_mark = u32::try_from(machine.dyn_stack.len()).expect("dyn_stack exceeds u32");
    machine.dyn_stack.push((cell, old));
    heap.cell_mut(cell).value = Some(value);
    // LIFO: run the body first; the `WithRestore` beneath it restores on completion, or
    // is run by the unwinder on any non-local exit through it.
    let frame = machine.frames.last_mut().expect("a frame is active");
    frame.conts.push(Cont::WithRestore { dyn_mark });
    frame.conts.push(Cont::Seq {
        block: body,
        next: 0,
    });
    Ok(())
}
