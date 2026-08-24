//! Calls: callee/argument evaluation, argument binding, and callable-frame
//! entry/return (machine-design §8/§10/§11).
//!
//! **Scope (M2a.8).** Calls of `to`/`fn`/anonymous-`fn` values with positional +
//! keyword arguments and parameter defaults (L§8.3), interning callable values
//! (with **closure captures** read from the creating environment, §7/§10), and
//! **proper tail calls** (the apply-time kind gate + frame reuse, S-55). Block
//! arguments are bound here and invoked in `block.rs`; a frame's cell-boxed slots
//! (representation B) are built in `local.rs`.
//!
//! A call runs as: evaluate the callee ([`Cont::CallGotCallee`]), evaluate each
//! argument left to right ([`Cont::CallGotArg`]), then [`apply`] binds parameters
//! and either **pushes** a [`FrameKind::Callable`] frame or, for a kind-matched
//! marked tail call, **reuses** the current frame in place (§11) — its bottom cont
//! is a [`Cont::ReturnBarrier`]. When the body drains to the barrier,
//! [`return_from_callable`] delivers the result — a `fn`'s value, Void for a `to`,
//! or a raise if a `fn` fell off the end (L§8.4).

use super::cont::Cont;
use super::error::{ExceptionKind, Raise};
use super::frame::{Frame, FrameKind, Local};
use super::step::take_value;
use super::{Machine, Value, block, control, intrinsic, local};
use crate::ast::{Arg, Node, NodeId, Param};
use crate::heap::{CalObj, CallableTarget, Heap};
use crate::resolve::{BodyKind, ParamInfo, Resolution, ResolvedModule};
use crate::span::Span;

/// Schedules a call (an expression). A `body(args)` invocation of the current
/// callable's block parameter (§8.5) takes the block path (`block.rs`); every
/// other call evaluates its callee, then [`Cont::CallGotCallee`] takes over. A
/// trailing `do … end` block argument is carried on the `Call` node and bound in
/// [`apply`].
pub(crate) fn eval_call(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    call: NodeId,
) -> Result<(), Raise> {
    let Node::Call { callee, .. } = resolved.ast.node(call) else {
        unreachable!("eval_call over a non-Call node");
    };
    let callee = *callee;
    if block::is_block_invocation(resolved, heap, machine, callee) {
        return block::eval_block_call(resolved, heap, machine, call);
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
    namespace: &control::Namespace,
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
        None => apply(resolved, heap, machine, namespace, call, callee, Vec::new()),
    }
}

/// The argument at `index` is now in the register: stash it, then evaluate the
/// next argument or apply once the last is in. (The parameters mirror the
/// `CallGotArg` continuation's fields plus the standard step context.)
#[allow(clippy::too_many_arguments)]
pub(crate) fn got_arg(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    namespace: &control::Namespace,
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
        None => apply(resolved, heap, machine, namespace, call, callee, values),
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
    namespace: &control::Namespace,
    call: NodeId,
    callee: Value,
    arg_values: Vec<Value>,
) -> Result<(), Raise> {
    let span = resolved.ast.span(call);
    // A **record type value** used as a callee constructs an instance (L§9); a built-in
    // type value is not callable and falls through to the error below.
    if let Value::Type(idx) = callee
        && matches!(heap.type_value(idx).kind, super::TypeKind::Record(_))
    {
        return super::record::construct(resolved, heap, machine, call, idx, arg_values);
    }
    let Value::Callable(cal) = callee else {
        return Err(Raise::new(
            ExceptionKind::NotCallable,
            "this isn't something you can call",
            span,
        ));
    };
    // A host intrinsic foreign function runs its callback **inline** (E§5.2) — it
    // never becomes a callable frame — so it dispatches here, before any frame or
    // tail-reuse machinery. A source callable takes the frame path below.
    if let CallableTarget::Intrinsic(id) = heap.callable(cal).target {
        return intrinsic::apply(resolved, heap, machine, namespace, call, id, arg_values);
    }
    let callable_id = heap.callable(cal).source_id() as usize;
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

    let callee_kind = info.kind;
    // A marked tail call whose callee's kind matches the current frame's **reuses**
    // that frame instead of growing the stack (proper tail calls, MD §11) — so a
    // tail loop runs in constant memory. A kind mismatch (or a non-tail call) pushes
    // an ordinary frame, giving exact non-tail semantics (S-55).
    let reuse = resolved.tail_calls[call.0 as usize]
        && reuses_current_frame(machine, resolved, heap, callee_kind);
    // Build the callee's slots (representation B): cell-box captured slots and
    // splice the closure's captured cells (§7/§10). `captured` is cloned so the
    // immutable read releases before the cell allocations.
    let captured = heap.callable(cal).captures.clone();
    let locals = local::build(resolved, heap, callable_id, &slots, &captured);
    if reuse {
        let top = machine
            .frames
            .last()
            .expect("a frame is active at a tail call");
        let FrameKind::Callable { cal: elided } = top.kind else {
            unreachable!("only a callable frame is reused (S-55 kind gate)");
        };
        machine.record_elided(elided, top.serial);
        machine
            .frames
            .last_mut()
            .expect("a frame is active")
            .reuse_as_callable(cal, locals, body, block_param, call);
    } else {
        let serial = machine.next_frame_serial();
        machine.frames.push(Frame::callable(
            cal,
            locals,
            body,
            serial,
            block_param,
            call,
        ));
    }
    // Defaults are evaluated in the callee activation, before the body (LIFO: push
    // in reverse source order so earlier defaults run — and bind — first).
    let frame = machine.frames.last_mut().expect("callable frame active");
    for &(slot, expr) in defaults.iter().rev() {
        frame.conts.push(Cont::BindDefault {
            slot,
            default: expr,
        });
        frame.conts.push(Cont::Eval { node: expr });
    }
    Ok(())
}

/// The S-55 apply-time kind gate: whether a marked tail call reuses the current
/// frame. A callable frame is reused iff the callee's kind matches its own; a
/// mismatch pushes an ordinary frame (exact non-tail parity — a `to` tail-calling
/// an `fn` still discards the value; an `fn` tail-calling a `to` still falls off at
/// its own barrier). (Block-frame tail reuse — MD §11's Block↔Callable case — is
/// deferred, tracked in claude-todo; a block-body tail call falls back to an
/// ordinary frame, which is correct, just not constant-memory.)
fn reuses_current_frame(
    machine: &Machine,
    resolved: &ResolvedModule,
    heap: &Heap,
    callee_kind: BodyKind,
) -> bool {
    match &machine.frames.last().expect("a frame is active").kind {
        FrameKind::Callable { cal } => {
            resolved.callables[heap.callable(*cal).source_id() as usize].kind == callee_kind
        }
        FrameKind::Block { .. } | FrameKind::ModuleTopLevel => false,
    }
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
/// frame slot (L§8.2), through the cell for a cell-boxed (captured) parameter. A
/// default is an expression, so it must yield a value.
pub(crate) fn bind_default(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    slot: u16,
    default: NodeId,
) -> Result<(), Raise> {
    let value = take_value(machine, resolved.ast.span(default))?;
    let top = machine.frames.len() - 1;
    local::write(heap, &mut machine.frames[top].locals[slot as usize], value);
    Ok(())
}

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
            let cell = heap.alloc_cell(None);
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
