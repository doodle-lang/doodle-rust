//! Assignment scheduling (L§5.3, L§14): turning an `Assign` statement into the transitions
//! that store its value. A **name** target stores directly (`assign_to`); a **place** target
//! (`object.field = v`, `object[key] = v`) navigates the object first, evaluating operands
//! left to right, and the final store lands in `record::field_set` / `dict::index_set`. Split
//! from `control.rs` for length; the slot/cell/reference helpers live there.

use super::cont::Cont;
use super::control::{self, Namespace};
use super::error::Raise;
use super::step::take_value;
use super::{Machine, Value, record};
use crate::ast::{Node, NodeId};
use crate::heap::Heap;
use crate::resolve::{Resolution, ResolvedModule};

/// A **name** assignment (`x = v`): the RHS value is in the register; store it into the
/// target's binding — a local slot, a module cell, or an enclosing block-outer slot.
pub(crate) fn assign_to(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    namespace: &Namespace,
    assign: NodeId,
) -> Result<(), Raise> {
    let Node::Assign { target, .. } = resolved.ast.node(assign) else {
        unreachable!("AssignTo over a non-Assign node");
    };
    debug_assert!(
        matches!(resolved.ast.node(*target), Node::Ident(_)),
        "AssignTo is only dispatched for a name target; places go through AssignPlaceObj"
    );
    let target = *target;
    let span = resolved.ast.span(assign);
    let value = record::copy_on_bind(take_value(machine, span)?, heap);
    match control::resolution(resolved, target) {
        Resolution::LocalSlot(slot) => control::set_slot(machine, heap, slot, value),
        Resolution::ModuleName(idx) => {
            let name = &resolved.name_refs[idx as usize].name;
            let cell = control::find_cell(namespace, name)
                .ok_or_else(|| control::name_not_defined(name, span))?;
            heap.cell_mut(cell).value = Some(value);
        }
        // A block body writing an enclosing local through the defining static link
        // (§7/§8.5 — a block can mutate its enclosing variables).
        Resolution::BlockOuter { hops, slot } => {
            let owner = control::outer_frame(machine, hops);
            control::set_slot_at(machine, heap, owner, slot, value);
        }
    }
    Ok(())
}

/// A place assignment's target **object** is now in the register — the *actual*
/// object (place navigation copies nothing, L§5.3). Branch on the target kind:
/// a `Field` target evaluates the RHS next (then sets the field); an `Index` target
/// evaluates the key first (left-to-right, L§14), then the RHS. The copy for binding
/// fires at the final store, not here.
pub(crate) fn assign_place_obj(
    resolved: &ResolvedModule,
    machine: &mut Machine,
    assign: NodeId,
) -> Result<(), Raise> {
    let Node::Assign { target, value } = resolved.ast.node(assign) else {
        unreachable!("AssignPlaceObj over a non-Assign node");
    };
    let (target, value) = (*target, *value);
    let object = take_value(machine, resolved.ast.span(target))?;
    let frame = machine.frames.last_mut().expect("a frame is active");
    match resolved.ast.node(target) {
        // `object.name = v`: evaluate the RHS, then set the field.
        Node::Field { .. } => {
            frame.conts.push(Cont::AssignFieldVal { assign, object });
            frame.conts.push(Cont::Eval { node: value });
        }
        // `object[key] = v`: evaluate the key, then the RHS, then store.
        Node::Index { index, .. } => {
            let index = *index;
            frame.conts.push(Cont::AssignIndexKey { assign, object });
            frame.conts.push(Cont::Eval { node: index });
        }
        other => unreachable!("AssignPlaceObj over a non-place target: {other:?}"),
    }
    Ok(())
}

/// An index place assignment's key is now in the register, its object saved: stash
/// the key and evaluate the RHS (left-to-right, L§14), which [`Cont::AssignIndexVal`]
/// then stores.
pub(crate) fn assign_index_key(
    resolved: &ResolvedModule,
    machine: &mut Machine,
    assign: NodeId,
    object: Value,
) -> Result<(), Raise> {
    let Node::Assign { target, value } = resolved.ast.node(assign) else {
        unreachable!("AssignIndexKey over a non-Assign node");
    };
    let (target, value) = (*target, *value);
    let key = take_value(machine, resolved.ast.span(target))?;
    let frame = machine.frames.last_mut().expect("a frame is active");
    frame.conts.push(Cont::AssignIndexVal {
        assign,
        object,
        key,
    });
    frame.conts.push(Cont::Eval { node: value });
    Ok(())
}
