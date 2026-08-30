//! Statement and control-flow transitions: name reads/writes, `let`/`const`
//! binding, assignment, and `if`/`while`/`loop` (machine-design §6/§7).
//!
//! Module-level bindings live in **cells** (MD §6); a construct-body local lives
//! in a frame **slot** (MD §7). The resolver already decided which: a `let`/
//! `const` decl node carries a `LocalSlot` resolution when it is a frame local,
//! and none when it is a module global; a name *reference* carries `LocalSlot`
//! or `ModuleName`. A binding read before its declaration executes (an
//! uninitialized cell/slot) is a **use-before-defined** error (the temporal dead
//! zone); a name with no binding at all is **not defined**.
//!
//! Control constructs run their bodies in the **enclosing frame** (the resolver
//! models this — only callable/block bodies open a frame), so a construct-body
//! body is just a `Seq` over its statements. Their conditions are strict `Bool`s
//! (no truthiness, L§4.3). `break`/`continue` (which target the loop conts) are
//! M2a.6.

use super::cont::Cont;
use super::error::Raise;
use super::frame::{Frame, FrameKind};
use super::step::take_value;
use super::{LoadedModule, Machine, Value, compare, local};
use crate::ast::{Node, NodeId};
use crate::heap::Heap;
use crate::machine::CellIdx;
use crate::resolve::{Resolution, ResolvedModule};
use crate::span::Span;

/// The per-instance module namespace (`name → cell`) at M2a — a small ordered
/// list scanned linearly (deterministic; no hashing on a Doodle-observable path).
pub(crate) type Namespace = [(Box<str>, CellIdx)];

/// Reads the value of a name reference (`Node::Ident`) — a frame local or a
/// module cell — raising if it is undefined or used before it was defined.
pub(crate) fn read_ref(
    resolved: &ResolvedModule,
    modules: &[LoadedModule],
    heap: &Heap,
    machine: &Machine,
    node: NodeId,
) -> Result<Value, Raise> {
    let span = resolved.ast.span(node);
    let name = ident_name(resolved, node);
    match resolution(resolved, node) {
        Resolution::LocalSlot(slot) => read_slot(machine, heap, slot, name, span),
        Resolution::ModuleName(idx) => {
            let name = &resolved.name_refs[idx as usize].name;
            let cur = resolved.canonical_id.0 as usize;
            lookup_free(modules, cur, machine, heap, name, span)
        }
        // A block body reading an enclosing local through the defining static link
        // (§7): chase `defining` `hops` times, then read that frame's slot.
        Resolution::BlockOuter { hops, slot } => {
            read_slot_at(machine, heap, outer_frame(machine, hops), slot, name, span)
        }
    }
}

/// Binds a `let`/`const` initializer (now in the register) to its target — a
/// module cell (a global) or a frame slot (a construct-body local). The
/// statement yields Void.
pub(crate) fn bind_let(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    namespace: &Namespace,
    decl: NodeId,
) -> Result<(), Raise> {
    let value = take_value(machine, resolved.ast.span(decl))?;
    bind_decl(resolved, heap, machine, namespace, decl, value);
    Ok(())
}

/// Binds `value` to a declaration's target — a module cell (a global) or a frame
/// slot (a construct-body local) — per the resolver's decision. Shared by
/// `let`/`const` binding and `to`/`fn` definition (`call::define_callable`).
pub(crate) fn bind_decl(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    namespace: &Namespace,
    decl: NodeId,
    value: Value,
) {
    // Binding a value record copies it (L§4.14); a `ref` record, a scalar, or a
    // callable/type (a `to`/`fn`/`record` definition binds one) is shared unchanged.
    let value = super::record::copy_on_bind(value, heap);
    match resolved.resolutions[decl.0 as usize] {
        // A construct-body / nested local: a frame slot. A declaration is a new
        // binding, so a boxed slot gets a fresh cell (loop-fresh).
        Some(Resolution::LocalSlot(slot)) => rebind_slot(machine, heap, slot, value),
        // A module global: the declaration's name binds its cell (created at load).
        None => {
            let name = decl_name(resolved, decl);
            let cell = find_cell(namespace, name).expect("a module global's cell exists");
            heap.cell_mut(cell).value = Some(value);
        }
        Some(other) => unreachable!("unexpected resolution on a decl: {other:?}"),
    }
}

/// Writes a **name** assignment's value (now in the register) to its binding — a
/// module cell, a frame slot, or an enclosing local (a block writing outward). A
/// value record is copied for binding (L§4.14). Field/index place targets take the
/// [`assign_place_obj`] path instead (they are never dispatched here).
/// Schedules an `if`: evaluate the first arm's condition, then choose. Shared by
/// statement- and expression-position `if`.
pub(crate) fn schedule_if(frame: &mut Frame, resolved: &ResolvedModule, node: NodeId) {
    let first_cond = match resolved.ast.node(node) {
        Node::If { arms, .. } => arms[0].cond, // the parser guarantees ≥ 1 arm
        _ => unreachable!("schedule_if over a non-If node"),
    };
    frame.conts.push(Cont::IfChoose { node, index: 0 });
    frame.conts.push(Cont::Eval { node: first_cond });
}

/// The `if` choice, with arm `index`'s condition in the register: if true, run
/// that arm's body; else advance to the next arm, the `else`, or nothing (a
/// statement `if` with no match yields Void — the register is already `None`).
pub(crate) fn if_choose(
    resolved: &ResolvedModule,
    machine: &mut Machine,
    node: NodeId,
    index: u32,
) -> Result<(), Raise> {
    let (body, cond_span, next_cond, else_body) = {
        let Node::If { arms, else_body } = resolved.ast.node(node) else {
            unreachable!("IfChoose over a non-If node");
        };
        let arm = &arms[index as usize];
        let next = index as usize + 1;
        let next_cond = arms.get(next).map(|a| a.cond);
        (arm.body, resolved.ast.span(arm.cond), next_cond, *else_body)
    };
    let cond = take_value(machine, cond_span)?;
    let frame = machine.frames.last_mut().expect("a frame is active");
    if compare::as_bool(cond, "if", cond_span)? {
        frame.conts.push(Cont::Seq {
            block: body,
            next: 0,
        });
    } else if let Some(next_cond) = next_cond {
        frame.conts.push(Cont::IfChoose {
            node,
            index: index + 1,
        });
        frame.conts.push(Cont::Eval { node: next_cond });
    } else if let Some(else_body) = else_body {
        frame.conts.push(Cont::Seq {
            block: else_body,
            next: 0,
        });
    }
    Ok(())
}

/// A `while` step, with the condition in the register: if true, run the body then
/// re-check; else the loop is done (yields Void — the register is already `None`).
pub(crate) fn while_check(
    resolved: &ResolvedModule,
    machine: &mut Machine,
    node: NodeId,
) -> Result<(), Raise> {
    let (cond, cond_span, body) = match resolved.ast.node(node) {
        Node::While { cond, body } => (*cond, resolved.ast.span(*cond), *body),
        _ => unreachable!("WhileCheck over a non-While node"),
    };
    let value = take_value(machine, cond_span)?;
    if compare::as_bool(value, "while", cond_span)? {
        let frame = machine.frames.last_mut().expect("a frame is active");
        // LIFO: run the body, then re-evaluate the condition, then re-check.
        frame.conts.push(Cont::WhileCheck { node });
        frame.conts.push(Cont::Eval { node: cond });
        frame.conts.push(Cont::Seq {
            block: body,
            next: 0,
        });
    }
    Ok(())
}

/// Re-enters a `loop` body: run the body, then loop again.
pub(crate) fn loop_reloop(resolved: &ResolvedModule, machine: &mut Machine, node: NodeId) {
    let body = match resolved.ast.node(node) {
        Node::Loop { body } => *body,
        _ => unreachable!("LoopReloop over a non-Loop node"),
    };
    let frame = machine.frames.last_mut().expect("a frame is active");
    frame.conts.push(Cont::LoopReloop { node });
    frame.conts.push(Cont::Seq {
        block: body,
        next: 0,
    });
}

fn read_slot(
    machine: &Machine,
    heap: &Heap,
    slot: u16,
    name: &str,
    span: Span,
) -> Result<Value, Raise> {
    read_slot_at(machine, heap, machine.frames.len() - 1, slot, name, span)
}

pub(crate) fn set_slot(machine: &mut Machine, heap: &mut Heap, slot: u16, value: Value) {
    let top = machine.frames.len() - 1;
    set_slot_at(machine, heap, top, slot, value);
}

/// Reads slot `slot` of the frame at `frame` (the top frame, or an enclosing one
/// reached by a block static link), dereferencing a cell-boxed slot (§7), and
/// raising if it is uninitialized.
fn read_slot_at(
    machine: &Machine,
    heap: &Heap,
    frame: usize,
    slot: u16,
    name: &str,
    span: Span,
) -> Result<Value, Raise> {
    match local::read(heap, machine.frames[frame].locals[slot as usize]) {
        Some(v) => Ok(v),
        None => Err(used_before_defined(name, span)),
    }
}

/// Writes to an existing slot (an assignment) — mutating the cell for a boxed slot.
pub(crate) fn set_slot_at(
    machine: &mut Machine,
    heap: &mut Heap,
    frame: usize,
    slot: u16,
    value: Value,
) {
    local::write(
        heap,
        &mut machine.frames[frame].locals[slot as usize],
        value,
    );
}

/// Binds a `let`/`const`/declaration value to the top frame's slot — a boxed slot
/// gets a **fresh** cell (loop-fresh, L§5.4), a direct slot is set inline.
fn rebind_slot(machine: &mut Machine, heap: &mut Heap, slot: u16, value: Value) {
    let top = machine.frames.len() - 1;
    local::rebind(heap, &mut machine.frames[top].locals[slot as usize], value);
}

/// The frame reached by chasing the top (block) frame's `defining` static link
/// `hops` times (machine-design §7). `hops = 0` is the block's own frame; each
/// hop steps to the frame the block was defined in. The intervening frames are
/// block frames (the resolver guarantees the chain does not cross a callable
/// boundary); the defining `serial` is checked to catch a stale link. Shared with
/// `block.rs` (a block-parameter invocation from inside a nested block reaches the
/// owning callable the same way).
pub(crate) fn outer_frame(machine: &Machine, hops: u16) -> usize {
    let mut idx = machine.frames.len() - 1;
    for _ in 0..hops {
        let FrameKind::Block {
            defining,
            defining_serial,
            ..
        } = machine.frames[idx].kind
        else {
            unreachable!("a BlockOuter static-link chase reached a non-block frame");
        };
        debug_assert_eq!(
            machine.frames[defining].serial, defining_serial,
            "stale block defining link — a frame slot was reused (machine-design §8)"
        );
        idx = defining;
    }
    idx
}

mod names;

// Name resolution and the name-miss raise helpers live in `names` (split for length); their
// public paths stay `control::…` so callers are unchanged.
use names::{decl_name, ident_name, used_before_defined};
pub(crate) use names::{
    find_cell, lookup_free, module_display, name_not_defined, no_such_member, not_exported,
    param_cell, read_cell, resolution,
};
