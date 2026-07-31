//! Calls: callee/argument evaluation, argument binding, and callable-frame
//! entry/return (machine-design §8/§10).
//!
//! **Scope (M2a.5).** Non-tail calls of `to`/`fn`/anonymous-`fn` values with
//! positional + keyword arguments and parameter defaults (L§8.3), plus interning
//! callable values. **Not here:** block arguments and the `return` unwind (M2a.6),
//! closure captures (M2a.8), tail-call frame reuse (M2a.7). A call node carrying a
//! block argument, or a callee declaring a block parameter, reaches an
//! `unimplemented!` — no M2a.5 program exercises one.
//!
//! A call runs as: evaluate the callee ([`Cont::CallGotCallee`]), evaluate each
//! argument left to right ([`Cont::CallGotArg`]), then [`apply`] binds parameters
//! and pushes a [`FrameKind::Callable`] frame whose bottom cont is a
//! [`Cont::ReturnBarrier`]. When the body drains to the barrier,
//! [`return_from_callable`] delivers the result — a `fn`'s value, or Void for a
//! `to` (L§8.4).

use super::cont::Cont;
use super::error::{ExceptionKind, Raise};
use super::frame::{Frame, FrameKind};
use super::step::take_value;
use super::{Machine, Value, block, control};
use crate::ast::{Arg, Node, NodeId, Param};
use crate::heap::{CalObj, Heap};
use crate::resolve::{BodyKind, ParamInfo, ResolvedModule};
use crate::span::Span;

/// Schedules a call (an expression). A `body(args)` invocation of the current
/// callable's block parameter (§8.5) takes the block path (`block.rs`); every
/// other call evaluates its callee, then [`Cont::CallGotCallee`] takes over. A
/// trailing `do … end` block argument is carried on the `Call` node and bound in
/// [`apply`].
pub(crate) fn eval_call(
    resolved: &ResolvedModule,
    heap: &Heap,
    machine: &mut Machine,
    call: NodeId,
) -> Result<(), Raise> {
    let Node::Call { callee, .. } = resolved.ast.node(call) else {
        unreachable!("eval_call over a non-Call node");
    };
    let callee = *callee;
    if block::is_block_invocation(resolved, heap, machine, callee) {
        return block::eval_block_call(resolved, machine, call);
    }
    let frame = machine.frames.last_mut().expect("eval_call with no frame");
    frame.conts.push(Cont::CallGotCallee { call });
    frame.conts.push(Cont::Eval { node: callee });
    Ok(())
}

/// The callee is now in the register: start evaluating arguments left to right,
/// or apply immediately when there are none.
pub(crate) fn got_callee(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    call: NodeId,
) -> Result<(), Raise> {
    let span = resolved.ast.span(call);
    let callee = take_value(machine, span)?;
    let Node::Call { args, .. } = resolved.ast.node(call) else {
        unreachable!("CallGotCallee over a non-Call node");
    };
    match args.first() {
        Some(first) => {
            let first_expr = arg_expr(first);
            let frame = machine.frames.last_mut().expect("got_callee with no frame");
            frame.conts.push(Cont::CallGotArg {
                call,
                callee,
                values: Vec::new(),
                index: 0,
            });
            frame.conts.push(Cont::Eval { node: first_expr });
            Ok(())
        }
        None => apply(resolved, heap, machine, call, callee, Vec::new()),
    }
}

/// The argument at `index` is now in the register: stash it, then evaluate the
/// next argument or apply once the last is in.
pub(crate) fn got_arg(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    call: NodeId,
    callee: Value,
    mut values: Vec<Value>,
    index: u32,
) -> Result<(), Raise> {
    let span = resolved.ast.span(call);
    values.push(take_value(machine, span)?);
    let Node::Call { args, .. } = resolved.ast.node(call) else {
        unreachable!("CallGotArg over a non-Call node");
    };
    let next = index as usize + 1;
    match args.get(next) {
        Some(arg) => {
            let next_expr = arg_expr(arg);
            let frame = machine.frames.last_mut().expect("got_arg with no frame");
            frame.conts.push(Cont::CallGotArg {
                call,
                callee,
                values,
                index: index + 1,
            });
            frame.conts.push(Cont::Eval { node: next_expr });
            Ok(())
        }
        None => apply(resolved, heap, machine, call, callee, values),
    }
}

/// Binds `arg_values` to the callee's parameters (L§8.3) and pushes its frame.
///
/// The callee must be a callable. Positional arguments fill parameters left to
/// right; keyword arguments fill by name; any parameter left unfilled must have a
/// default (evaluated in the callee activation, L§8.2) or the call raises. A
/// non-callable callee, too many arguments, an unknown or duplicated keyword, or
/// a missing required argument each raise (L§6.4/§8.3).
fn apply(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    call: NodeId,
    callee: Value,
    arg_values: Vec<Value>,
) -> Result<(), Raise> {
    let span = resolved.ast.span(call);
    let Value::Callable(cal) = callee else {
        return Err(Raise::new(
            ExceptionKind::NotCallable,
            "this isn't something you can call",
            span,
        ));
    };
    let callable_id = heap.callable(cal).callable as usize;
    let info = &resolved.callables[callable_id];
    let params = &info.params;
    let body = info.body;
    let (slots, filled) =
        bind_arguments(resolved, call, params, info.slot_count, &arg_values, span)?;

    // Unfilled ordinary parameters need a default (scheduled below) or the call is
    // missing a required argument. A block parameter is filled from the `do … end`
    // argument (below), never here.
    let mut defaults: Vec<(u16, NodeId)> = Vec::new();
    for (i, pi) in params.iter().enumerate() {
        if filled[i] || pi.is_block {
            continue;
        }
        if pi.has_default {
            defaults.push((pi.slot, default_expr(resolved, info.decl, i)));
        } else {
            return Err(arg_error(
                span,
                format!("missing argument `{}` for this call", pi.name),
            ));
        }
    }

    // Bind the `do … end` block argument (if any) to the callee's block parameter,
    // checking the two are consistent (§8.3/§8.5). Computed before the callee frame
    // is pushed, so the descriptor's defining link names the caller frame.
    let block_param = block::bind_block_argument(resolved, machine, call, params, span)?;

    let serial = machine.next_frame_serial();
    machine
        .frames
        .push(Frame::callable(cal, slots, body, serial, block_param));
    // Defaults are evaluated in the callee activation, before the body (LIFO: push
    // in reverse source order so earlier defaults run — and bind — first).
    let frame = machine
        .frames
        .last_mut()
        .expect("callable frame just pushed");
    for &(slot, expr) in defaults.iter().rev() {
        frame.conts.push(Cont::BindDefault {
            slot,
            default: expr,
        });
        frame.conts.push(Cont::Eval { node: expr });
    }
    Ok(())
}

/// Binds positional + keyword arguments to `params` (L§8.3), returning the filled
/// slot vector (sized `slot_count`) and a per-parameter filled flag. Only ordinary
/// parameters are bound: a block parameter is filled from the `do … end` argument
/// (§8.5), so a positional or keyword argument targeting one raises. Too many
/// arguments, an unknown keyword, and a duplicate binding each raise. Shared by
/// callable [`apply`] and block invocation ([`block`]).
pub(crate) fn bind_arguments(
    resolved: &ResolvedModule,
    call: NodeId,
    params: &[ParamInfo],
    slot_count: u16,
    arg_values: &[Value],
    span: Span,
) -> Result<(Vec<Option<Value>>, Vec<bool>), Raise> {
    let mut slots: Vec<Option<Value>> = vec![None; slot_count as usize];
    let mut filled = vec![false; params.len()];
    // Positional arguments come before keyword arguments (the parser guarantees it).
    let Node::Call { args, .. } = resolved.ast.node(call) else {
        unreachable!("bind_arguments over a non-Call node");
    };
    let mut pos = 0usize;
    for (arg, &val) in args.iter().zip(arg_values.iter()) {
        let p = match arg {
            // A positional fills the next ordinary parameter; the block parameter
            // is last, so reaching it means there are too many positionals.
            Arg::Positional(_) => {
                if pos >= params.len() || params[pos].is_block {
                    return Err(arg_error(span, "too many arguments for this call"));
                }
                let p = pos;
                pos += 1;
                p
            }
            Arg::Keyword { name, .. } => match params.iter().position(|pi| *pi.name == **name) {
                Some(p) if params[p].is_block => {
                    return Err(arg_error(
                        span,
                        format!("`{name}` takes a `do … end` block, not a keyword argument"),
                    ));
                }
                Some(p) => p,
                None => return Err(arg_error(span, format!("`{name}` isn't a parameter here"))),
            },
        };
        if filled[p] {
            return Err(arg_error(
                span,
                format!("`{}` was given more than once", params[p].name),
            ));
        }
        slots[params[p].slot as usize] = Some(val);
        filled[p] = true;
    }
    Ok((slots, filled))
}

/// A parameter default's value is now in the register: write it into the callee
/// frame slot (L§8.2). A default is an expression, so it must yield a value.
pub(crate) fn bind_default(
    resolved: &ResolvedModule,
    machine: &mut Machine,
    slot: u16,
    default: NodeId,
) -> Result<(), Raise> {
    let value = take_value(machine, resolved.ast.span(default))?;
    machine
        .frames
        .last_mut()
        .expect("bind_default with no frame")
        .locals[slot as usize] = Some(value);
    Ok(())
}

/// Interns and binds a named `to`/`fn` declaration to its target (a module cell
/// or a frame slot). Runs when the declaration statement executes, so a call
/// before then reads an uninitialized binding — the temporal dead zone (M2a.4a).
pub(crate) fn define_callable(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    namespace: &control::Namespace,
    decl: NodeId,
) {
    let value = make_callable(resolved, heap, decl);
    control::bind_decl(resolved, heap, machine, namespace, decl, value);
}

/// Interns a callable value for the `Callable` node `decl`: one canonical
/// [`CalObj`] naming its `CallableId` (machine-design §8). A plain `to`/`fn`'s
/// declaration runs once, so this is its single canonical value; an anonymous
/// `fn` gets a fresh value per evaluation. Closure captures are M2a.8.
pub(crate) fn make_callable(resolved: &ResolvedModule, heap: &mut Heap, decl: NodeId) -> Value {
    let callable_id = resolved
        .callables
        .iter()
        .position(|c| c.decl == decl)
        .expect("a Callable node has a resolved CallableInfo");
    if !resolved.callables[callable_id].captures.is_empty() {
        unimplemented!("closure captures are M2a.8");
    }
    let cal = heap.alloc_callable(CalObj {
        module: resolved.canonical_id,
        callable: callable_id as u32,
        captures: Vec::new(),
    });
    Value::Callable(cal)
}

/// Delivers a frame's result when its body drains to the [`Cont::ReturnBarrier`],
/// then pops it. A `fn` leaves its value in the register; a `to` yields Void
/// (L§8.4), so the register is cleared; a block yields its last expression's value
/// to its invoker (§8.5), so the register is kept.
pub(crate) fn return_from_callable(resolved: &ResolvedModule, heap: &Heap, machine: &mut Machine) {
    let frame = machine.frames.pop().expect("return with no frame");
    match frame.kind {
        FrameKind::Callable { cal } => {
            match resolved.callables[heap.callable(cal).callable as usize].kind {
                // A procedure yields no value; discard the body's final transient value.
                BodyKind::Proc => machine.reg = None,
                // A function's value is the register's current contents (its final
                // expression, or an executed `return expr`).
                BodyKind::Func => {}
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
}

/// The expression a call argument evaluates (positional value or keyword value).
pub(crate) fn arg_expr(arg: &Arg) -> NodeId {
    match arg {
        Arg::Positional(e) => *e,
        Arg::Keyword { value, .. } => *value,
    }
}

/// The default-value expression of the `index`-th parameter of the callable
/// declared at `decl`. The AST `Param` list is parallel to the resolver's
/// `ParamInfo` list (same order), so `index` addresses both.
fn default_expr(resolved: &ResolvedModule, decl: NodeId, index: usize) -> NodeId {
    let Node::Callable { params, .. } = resolved.ast.node(decl) else {
        unreachable!("default_expr over a non-Callable node");
    };
    match &params[index] {
        Param::Ordinary {
            default: Some(e), ..
        } => *e,
        _ => unreachable!("default_expr on a parameter without a default"),
    }
}

fn arg_error(span: Span, message: impl Into<String>) -> Raise {
    Raise::new(ExceptionKind::ArgumentError, message, span)
}
