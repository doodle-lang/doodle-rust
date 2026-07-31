//! Block arguments (L§8.5): passing a `do … end` block to a callee, invoking a
//! bound block parameter, and the static-link [`FrameKind::Block`] frames that
//! run block bodies (machine-design §8/§10).
//!
//! **Scope (M2a.6a).** Passing/binding a block argument, invoking it (`body(args)`),
//! and normal block completion — the block yields its last expression's value to
//! its invoker (§8.5). The three-tier exits (`continue`/`break`/`return`) that
//! target a block or its consumer are the unwind mechanism (M2a.6b, §12).
//!
//! A block is **second-class** (§8.5): its runtime handle ([`BlockDescriptor`]) is
//! never a Doodle value, so it lives on the callee frame's `block_param`, not in a
//! slot. A call whose callee names a block-parameter slot is a block invocation.
//!
//! **Known gaps (tracked, see claude-todo):** the resolver does not yet enforce
//! that a block parameter is used *only* in invocation position (§8.5 forbids
//! using one as a value); such misuse currently reaches an empty slot and raises a
//! misleading error. And [`is_block_invocation`] recognizes only a **direct**
//! invocation (a `LocalSlot` callee) — invoking a received block from *inside a
//! nested block* (a `BlockOuter` callee) is not yet routed here.

use super::cont::Cont;
use super::error::{ExceptionKind, Raise};
use super::frame::{BlockDescriptor, Consumer, Frame, FrameKind};
use super::step::take_value;
use super::{Machine, Value, call, control};
use crate::ast::{Node, NodeId};
use crate::heap::Heap;
use crate::resolve::{ParamInfo, Resolution, ResolvedModule};
use crate::span::Span;

/// Whether a call whose callee is `callee` invokes a block parameter (`body(args)`,
/// §8.5) rather than a callable value. True iff `callee` is an `Ident` resolving to
/// a block-parameter slot — either the current callable's own slot (a `LocalSlot`,
/// the direct case) or an enclosing callable's, reached from inside a nested block
/// through the defining chain (a `BlockOuter`). A block parameter slot never holds
/// a callable value, so this is unambiguous.
pub(crate) fn is_block_invocation(
    resolved: &ResolvedModule,
    heap: &Heap,
    machine: &Machine,
    callee: NodeId,
) -> bool {
    if !matches!(resolved.ast.node(callee), Node::Ident(_)) {
        return false;
    }
    let Some((owner, slot)) = block_param_owner(machine, resolved, callee) else {
        return false;
    };
    let FrameKind::Callable { cal } = &machine.frames[owner].kind else {
        return false;
    };
    let info = &resolved.callables[heap.callable(*cal).callable as usize];
    info.params.iter().any(|p| p.is_block && p.slot == slot)
}

/// The frame that owns the block parameter a call's `callee` names, and the slot,
/// if the callee resolves to a slot. For a direct invocation (`LocalSlot`) the
/// owner is the current frame; for an invocation from inside a nested block
/// (`BlockOuter`) it is the enclosing callable reached via the defining chain
/// (machine-design §7/§10). Returns `None` for any other callee.
fn block_param_owner(
    machine: &Machine,
    resolved: &ResolvedModule,
    callee: NodeId,
) -> Option<(usize, u16)> {
    match resolved.resolutions[callee.0 as usize] {
        Some(Resolution::LocalSlot(slot)) => Some((machine.frames.len() - 1, slot)),
        Some(Resolution::BlockOuter { hops, slot }) => {
            Some((control::outer_frame(machine, hops), slot))
        }
        _ => None,
    }
}

/// Schedules a block invocation `body(args)`: evaluate the block's arguments left
/// to right, or, with none, invoke it immediately.
pub(crate) fn eval_block_call(
    resolved: &ResolvedModule,
    machine: &mut Machine,
    call: NodeId,
) -> Result<(), Raise> {
    let Node::Call { args, .. } = resolved.ast.node(call) else {
        unreachable!("eval_block_call over a non-Call node");
    };
    match args.first() {
        Some(first) => {
            let first_expr = call::arg_expr(first);
            let frame = machine.frames.last_mut().expect("a frame is active");
            frame.conts.push(Cont::BlockGotArg {
                call,
                values: Vec::new(),
                index: 0,
            });
            frame.conts.push(Cont::Eval { node: first_expr });
            Ok(())
        }
        None => block_apply(resolved, machine, call, Vec::new()),
    }
}

/// A block invocation's argument at `index` is now in the register: stash it, then
/// evaluate the next argument or invoke the block once the last is in.
pub(crate) fn got_block_arg(
    resolved: &ResolvedModule,
    machine: &mut Machine,
    call: NodeId,
    mut values: Vec<Value>,
    index: u32,
) -> Result<(), Raise> {
    let span = resolved.ast.span(call);
    values.push(take_value(machine, span)?);
    let Node::Call { args, .. } = resolved.ast.node(call) else {
        unreachable!("BlockGotArg over a non-Call node");
    };
    let next = index as usize + 1;
    match args.get(next) {
        Some(arg) => {
            let next_expr = call::arg_expr(arg);
            let frame = machine.frames.last_mut().expect("a frame is active");
            frame.conts.push(Cont::BlockGotArg {
                call,
                values,
                index: index + 1,
            });
            frame.conts.push(Cont::Eval { node: next_expr });
            Ok(())
        }
        None => block_apply(resolved, machine, call, values),
    }
}

/// Invokes a bound block parameter with `arg_values`: binds them to the block's
/// parameters and pushes a [`FrameKind::Block`] frame whose `defining` link comes
/// from the descriptor and whose **consumer is the frame that received the block**
/// (the owner), not necessarily the frame that invoked it. Owner and invoker
/// coincide for a direct invocation; for an invocation from inside a nested block
/// the owner is the enclosing callable reached via the defining chain (§10/§12) —
/// so a `break` in the invoked block exits the call that *received* it, per L§7.10.
/// Block parameters have no defaults, so each must be supplied.
fn block_apply(
    resolved: &ResolvedModule,
    machine: &mut Machine,
    call: NodeId,
    arg_values: Vec<Value>,
) -> Result<(), Raise> {
    let span = resolved.ast.span(call);
    let Node::Call { callee, .. } = resolved.ast.node(call) else {
        unreachable!("block_apply over a non-Call node");
    };
    let (owner, _) =
        block_param_owner(machine, resolved, *callee).expect("a block invocation callee");
    let owner_serial = machine.frames[owner].serial;
    let Some(desc) = machine.frames[owner].block_param else {
        return Err(Raise::new(
            ExceptionKind::ArgumentError,
            "no block was given to invoke here",
            span,
        ));
    };
    let block_info = &resolved.callables[desc.callable as usize];
    let (slots, filled) = call::bind_arguments(
        resolved,
        call,
        &block_info.params,
        block_info.slot_count,
        &arg_values,
        span,
    )?;
    for (i, pi) in block_info.params.iter().enumerate() {
        if !filled[i] {
            return Err(Raise::new(
                ExceptionKind::ArgumentError,
                format!("missing argument `{}` for this block", pi.name),
                span,
            ));
        }
    }
    let body = block_info.body;
    let serial = machine.next_frame_serial();
    machine.frames.push(Frame::block(
        desc.defining,
        desc.defining_serial,
        Consumer::DoodleCall {
            frame: owner,
            serial: owner_serial,
        },
        slots,
        body,
        serial,
    ));
    Ok(())
}

/// Builds the [`BlockDescriptor`] for a call's trailing `do … end` argument and
/// checks it against the callee's parameters (§8.3/§8.5): a block argument to a
/// callee with no block parameter, or a missing block argument for a callee that
/// needs one, each raise. Called from [`call::apply`] before the callee frame is
/// pushed, so the descriptor's `defining` link names the **caller** frame (where
/// the block was written).
pub(crate) fn bind_block_argument(
    resolved: &ResolvedModule,
    machine: &Machine,
    call: NodeId,
    params: &[ParamInfo],
    span: Span,
) -> Result<Option<BlockDescriptor>, Raise> {
    let Node::Call { block, .. } = resolved.ast.node(call) else {
        unreachable!("bind_block_argument over a non-Call node");
    };
    let has_block_param = params.iter().any(|p| p.is_block);
    match (block, has_block_param) {
        (Some(block_arg), true) => {
            let callable = resolved
                .callables
                .iter()
                .position(|c| c.decl == block_arg.body)
                .expect("a block argument has a resolved Block CallableInfo")
                as u32;
            let defining = machine.frames.len() - 1;
            let defining_serial = machine.frames[defining].serial;
            Ok(Some(BlockDescriptor {
                defining,
                defining_serial,
                callable,
            }))
        }
        (Some(_), false) => Err(Raise::new(
            ExceptionKind::ArgumentError,
            "this call passes a `do … end` block, but the callee takes no block",
            span,
        )),
        (None, true) => Err(Raise::new(
            ExceptionKind::ArgumentError,
            "this callee needs a `do … end` block argument",
            span,
        )),
        (None, false) => Ok(None),
    }
}
