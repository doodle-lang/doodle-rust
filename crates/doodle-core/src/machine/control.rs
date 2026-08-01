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
use super::error::{ExceptionKind, Raise};
use super::frame::{Frame, FrameKind};
use super::step::take_value;
use super::{Machine, Value, compare, local};
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
    heap: &Heap,
    machine: &Machine,
    namespace: &Namespace,
    node: NodeId,
) -> Result<Value, Raise> {
    let span = resolved.ast.span(node);
    let name = ident_name(resolved, node);
    match resolution(resolved, node) {
        Resolution::LocalSlot(slot) => read_slot(machine, heap, slot, name, span),
        Resolution::ModuleName(idx) => {
            let name = &resolved.name_refs[idx as usize].name;
            read_cell(heap, find_cell(namespace, name), name, span)
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

/// Writes an assignment's value (now in the register) to its target lvalue. At
/// M2a.4a the target is a simple name; field/index place chains are M4 (S-38).
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
    let target = *target;
    let span = resolved.ast.span(assign);
    let value = take_value(machine, span)?;
    if !matches!(resolved.ast.node(target), Node::Ident(_)) {
        unimplemented!("assignment to a field/index place is M4 (S-38)");
    }
    match resolution(resolved, target) {
        Resolution::LocalSlot(slot) => set_slot(machine, heap, slot, value),
        Resolution::ModuleName(idx) => {
            let name = &resolved.name_refs[idx as usize].name;
            let cell = find_cell(namespace, name).ok_or_else(|| name_not_defined(name, span))?;
            heap.cell_mut(cell).value = Some(value);
        }
        // A block body writing an enclosing local through the defining static link
        // (§7/§8.5 — a block can mutate its enclosing variables).
        Resolution::BlockOuter { hops, slot } => {
            let owner = outer_frame(machine, hops);
            set_slot_at(machine, heap, owner, slot, value);
        }
    }
    Ok(())
}

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

fn set_slot(machine: &mut Machine, heap: &mut Heap, slot: u16, value: Value) {
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
fn set_slot_at(machine: &mut Machine, heap: &mut Heap, frame: usize, slot: u16, value: Value) {
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

fn read_cell(heap: &Heap, cell: Option<CellIdx>, name: &str, span: Span) -> Result<Value, Raise> {
    match cell {
        Some(c) => match heap.cell(c).value {
            Some(v) => Ok(v),
            None => Err(used_before_defined(name, span)),
        },
        None => Err(name_not_defined(name, span)),
    }
}

/// Finds a module cell by name (linear scan — the namespace is small and this
/// keeps lookup deterministic and hashing-free).
fn find_cell(namespace: &Namespace, name: &str) -> Option<CellIdx> {
    for (n, cell) in namespace {
        if n.as_ref() == name {
            return Some(*cell);
        }
    }
    None
}

/// The resolver's resolution for `node` (an `Ident` reference, or an lvalue
/// target — always resolved).
fn resolution(resolved: &ResolvedModule, node: NodeId) -> Resolution {
    resolved.resolutions[node.0 as usize].expect("a reference/lvalue is always resolved")
}

fn ident_name(resolved: &ResolvedModule, node: NodeId) -> &str {
    match resolved.ast.node(node) {
        Node::Ident(name) => name,
        _ => "the name", // read_ref is only ever called on an Ident
    }
}

fn decl_name(resolved: &ResolvedModule, decl: NodeId) -> &str {
    match resolved.ast.node(decl) {
        Node::Let { name, .. } | Node::Const { name, .. } => name,
        Node::Callable {
            name: Some(name), ..
        } => name,
        _ => unreachable!("bind_decl over a node with no binding name"),
    }
}

fn name_not_defined(name: &str, span: Span) -> Raise {
    Raise::new(
        ExceptionKind::NameNotDefined,
        format!("`{name}` isn't defined"),
        span,
    )
}

fn used_before_defined(name: &str, span: Span) -> Raise {
    Raise::new(
        ExceptionKind::UsedBeforeDefined,
        format!("`{name}` is used here before it's defined"),
        span,
    )
}
