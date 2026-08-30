//! Callable-value construction and frame return: interning a `to`/`fn` declaration's
//! [`Value::Callable`] (with its closure captures), binding it, and delivering a callable
//! frame's result when its body drains to the `ReturnBarrier`. Split from `call/mod.rs`
//! (callee/argument evaluation and frame entry) to stay within the hygiene length limit.

use crate::ast::NodeId;
use crate::heap::{CalObj, CallableTarget, Heap};
use crate::machine::error::{ExceptionKind, Raise};
use crate::machine::frame::{FrameKind, Local};
use crate::machine::{Machine, Value, control, local};
use crate::resolve::{BodyKind, Resolution, ResolvedModule};

/// Interns and binds a named `to`/`fn` declaration to its target (a module cell
/// or a frame slot). Runs when the declaration statement executes, so a call
/// before then reads an uninitialized binding — the temporal dead zone (M2a.4a).
///
/// A **cell-boxed local** declaration needs **letrec** order: the callable's body
/// may reference its own name (a self-recursive helper — the reference crosses the
/// callable's `fn` boundary, so it resolves as a capture, §7), so the callable must
/// capture the **same** cell the binding fills. We therefore give the slot a fresh
/// cell *before* interning the callable (so `make_callable`'s self-capture reads it,
/// and each loop iteration's helper is a distinct binding, L§5.4), then fill it.
pub(crate) fn define_callable(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    namespace: &control::Namespace,
    decl: NodeId,
) {
    if let Some(Resolution::LocalSlot(slot)) = resolved.resolutions[decl.0 as usize] {
        let top = machine.frames.len() - 1;
        if matches!(machine.frames[top].locals[slot as usize], Local::Boxed(_)) {
            let cell = heap.alloc_cell(crate::heap::CellKind::Let, None);
            machine.frames[top].locals[slot as usize] = Local::Boxed(cell);
            let value = make_callable(resolved, heap, machine, decl);
            heap.cell_mut(cell).value = Some(value);
            return;
        }
    }
    // A direct slot or a module global: intern the callable, then bind it.
    let value = make_callable(resolved, heap, machine, decl);
    control::bind_decl(resolved, heap, machine, namespace, decl, value);
}

/// Interns a callable value for the `Callable` node `decl`: one canonical
/// [`CalObj`] naming its `CallableId` (machine-design §8), with its **captured
/// cells** read from the creating environment (representation B, §7/§10). A plain
/// `to`/`fn`'s declaration runs once, so this is its single canonical value; an
/// anonymous `fn` (a closure) gets a fresh value — with fresh captures — per
/// evaluation.
pub(crate) fn make_callable(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &Machine,
    decl: NodeId,
) -> Value {
    let callable_id = resolved
        .callables
        .iter()
        .position(|c| c.decl == decl)
        .expect("a Callable node has a resolved CallableInfo");
    // Each capture reads a cell from the creating environment: chase the creating
    // frame's defining chain `hops` (§7), then take the cell-boxed source slot's cell.
    let captures: Vec<_> = resolved.callables[callable_id]
        .captures
        .iter()
        .map(|cs| {
            let owner = control::outer_frame(machine, cs.from.hops);
            local::cell_of(machine.frames[owner].locals[cs.from.slot as usize])
        })
        .collect();
    let cal = heap.alloc_callable(CalObj {
        module: resolved.canonical_id,
        target: CallableTarget::Source(callable_id as u32),
        captures,
    });
    Value::Callable(cal)
}

/// Delivers a frame's result when its body drains to the [`Cont::ReturnBarrier`],
/// then pops it. A `fn` leaves its value in the register; a `to` yields Void
/// (L§8.4), so the register is cleared; a block yields its last expression's value
/// to its invoker (§8.5), so the register is kept. A `fn` reaching the barrier with
/// a Void register **fell off the end** (L§8.4) and raises — the runtime backstop
/// for the fn-tail-`to` case the resolver cannot catch statically (S-55).
///
/// [`Cont::ReturnBarrier`]: crate::machine::cont::Cont::ReturnBarrier
pub(crate) fn return_from_callable(
    resolved: &ResolvedModule,
    heap: &Heap,
    machine: &mut Machine,
) -> Result<(), Raise> {
    let frame = machine.frames.pop().expect("return with no frame");
    match frame.kind {
        FrameKind::Callable { cal } => {
            let id = heap.callable(cal).source_id() as usize;
            match resolved.callables[id].kind {
                // A procedure yields no value; discard the body's final transient value.
                BodyKind::Proc => machine.reg = None,
                // A function's value is the register's current contents (its final
                // expression, or an executed `return expr`) — Void means it fell off.
                BodyKind::Func => {
                    if machine.reg.is_none() {
                        return Err(Raise::new(
                            ExceptionKind::FunctionFellOffEnd,
                            "this function reached its end without producing a value",
                            resolved.ast.span(resolved.callables[id].decl),
                        ));
                    }
                }
                other => unreachable!("callable frame over a non-callable body: {other:?}"),
            }
        }
        // A block delivers its last expression's value to its invoker; keep `reg`.
        FrameKind::Block { .. } => {}
        FrameKind::ModuleTopLevel => {
            unreachable!(
                "the module top level returns via the empty-cont path, not a ReturnBarrier"
            )
        }
    }
    Ok(())
}
