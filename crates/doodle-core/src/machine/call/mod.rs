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
use super::frame::{Frame, FrameKind};
use super::step::take_value;
use super::{LoadedModule, Machine, Value, block, intrinsic, local};
use crate::ast::{Arg, Node, NodeId, Param};
use crate::heap::{CallableTarget, Heap};
use crate::resolve::{BodyKind, ParamInfo, ResolvedModule};
use crate::span::Span;

mod frame;

// Callable-value construction + frame return live in `frame` (split for length); their
// public paths stay `call::…` so callers are unchanged.
pub(crate) use frame::{define_callable, make_callable, return_from_callable};

/// Schedules a call (an expression). A `body(args)` invocation of the current
/// callable's block parameter (§8.5) takes the block path (`block.rs`); every
/// other call evaluates its callee, then [`Cont::CallGotCallee`] takes over. A
/// trailing `do … end` block argument is carried on the `Call` node and bound in
/// [`apply`].
pub(crate) fn eval_call(
    resolved: &ResolvedModule,
    modules: &[LoadedModule],
    heap: &mut Heap,
    machine: &mut Machine,
    call: NodeId,
) -> Result<(), Raise> {
    let Node::Call { callee, .. } = resolved.ast.node(call) else {
        unreachable!("eval_call over a non-Call node");
    };
    let callee = *callee;
    if block::is_block_invocation(resolved, heap, machine, callee) {
        return block::eval_block_call(resolved, modules, heap, machine, call);
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
    modules: &mut [LoadedModule],
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
        None => apply(resolved, modules, heap, machine, call, callee, Vec::new()),
    }
}

/// The argument at `index` is now in the register: stash it, then evaluate the
/// next argument or apply once the last is in. (The parameters mirror the
/// `CallGotArg` continuation's fields plus the standard step context.)
#[allow(clippy::too_many_arguments)]
pub(crate) fn got_arg(
    resolved: &ResolvedModule,
    modules: &mut [LoadedModule],
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
        None => apply(resolved, modules, heap, machine, call, callee, values),
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
    modules: &mut [LoadedModule],
    heap: &mut Heap,
    machine: &mut Machine,
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
        )
        .with_details(vec![(
            "type",
            super::exception::DetailVal::str(super::exception::value_type_name(callee, heap)),
        )]));
    };
    // A host intrinsic foreign function runs its callback **inline** (E§5.2) — it
    // never becomes a callable frame — so it dispatches here, before any frame or
    // tail-reuse machinery. A source callable takes the frame path below.
    if let CallableTarget::Intrinsic(id) = heap.callable(cal).target {
        return intrinsic::apply(resolved, modules, heap, machine, call, id, arg_values);
    }
    // A protocol **dispatcher** (L§10.3): bind the arguments against the member's
    // signature, dispatch on the first argument's runtime type, and enter the resolved
    // implementation (protocol.rs) — or raise not-implemented / ambiguous.
    if let CallableTarget::Dispatcher { member, protocol } = heap.callable(cal).target {
        return super::protocol::dispatch_call(
            resolved, modules, heap, machine, call, member, protocol, arg_values,
        );
    }
    // A source callable runs in the module it was **defined** in (its `CalObj`'s module),
    // so its parameters, defaults, slot layout, and body come from **that** module's
    // resolved AST — not the caller's — which is what makes a cross-module call correct
    // (AD5). The caller's `resolved` still supplies the call-site context (span, argument
    // nodes, tail mark, block argument).
    let callee_module = heap.callable(cal).module;
    let callee_resolved: &ResolvedModule = &modules[callee_module.0 as usize].resolved;
    let callable_id = heap.callable(cal).source_id() as usize;
    let info = &callee_resolved.callables[callable_id];
    let params = &info.params;
    let body = info.body;
    let callee = callee_name(callee_resolved, info.decl);
    let (slots, filled) = bind_arguments(
        resolved,
        heap,
        call,
        params,
        info.slot_count,
        &arg_values,
        callee,
        span,
    )?;

    // Unfilled ordinary parameters need a default (scheduled below) or the call is
    // missing a required argument. A block parameter is filled from the `do … end`
    // argument (below), never here.
    let mut defaults: Vec<(u16, NodeId)> = Vec::new();
    for (i, pi) in params.iter().enumerate() {
        if filled[i] || pi.is_block {
            continue;
        }
        if pi.has_default {
            defaults.push((pi.slot, default_expr(callee_resolved, info.decl, i)));
        } else {
            return Err(Raise::new(
                ExceptionKind::MissingArgument,
                format!("missing argument `{}` for this call", pi.name),
                span,
            )
            .with_details(super::exception::parameter_details(
                callee,
                pi.name.as_ref(),
            )));
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
    // immutable read releases before the cell allocations. The slot layout is the
    // callee's, so `local::build` reads the callee's module (AD5).
    let captured = heap.callable(cal).captures.clone();
    let locals = local::build(callee_resolved, heap, callable_id, &slots, &captured);
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
            .reuse_as_callable(callee_module, cal, locals, body, block_param, call);
    } else {
        let serial = machine.next_frame_serial();
        let dyn_depth = machine.dyn_stack.len() as u32;
        machine.frames.push(Frame::callable(
            callee_module,
            cal,
            locals,
            body,
            serial,
            block_param,
            call,
            dyn_depth,
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn bind_arguments(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    call: NodeId,
    params: &[ParamInfo],
    slot_count: u16,
    arg_values: &[Value],
    callee: Option<&str>,
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
                    let expected = params.iter().filter(|p| !p.is_block).count();
                    let got = args
                        .iter()
                        .filter(|a| matches!(a, Arg::Positional(_)))
                        .count();
                    return Err(Raise::new(
                        ExceptionKind::TooManyArguments,
                        "too many arguments for this call",
                        span,
                    )
                    .with_details(
                        super::exception::too_many_arguments_details(callee, expected, got),
                    ));
                }
                let p = pos;
                pos += 1;
                p
            }
            Arg::Keyword { name, .. } => match params.iter().position(|pi| *pi.name == **name) {
                // The block parameter cannot be bound by keyword — from the caller's side
                // that name is not a keyword-bindable parameter, so it reads as unknown.
                Some(p) if params[p].is_block => {
                    return Err(Raise::new(
                        ExceptionKind::UnknownKeyword,
                        format!("`{name}` takes a `do … end` block, not a keyword argument"),
                        span,
                    )
                    .with_details(super::exception::unknown_keyword_details(
                        callee,
                        name.as_ref(),
                        &keyword_parameters(params),
                    )));
                }
                Some(p) => p,
                None => {
                    return Err(Raise::new(
                        ExceptionKind::UnknownKeyword,
                        format!("`{name}` isn't a parameter here"),
                        span,
                    )
                    .with_details(super::exception::unknown_keyword_details(
                        callee,
                        name.as_ref(),
                        &keyword_parameters(params),
                    )));
                }
            },
        };
        if filled[p] {
            return Err(Raise::new(
                ExceptionKind::DuplicateArgument,
                format!("`{}` was given more than once", params[p].name),
                span,
            )
            .with_details(super::exception::parameter_details(
                callee,
                params[p].name.as_ref(),
            )));
        }
        // Binding an argument to a parameter copies a value record (L§4.14).
        slots[params[p].slot as usize] = Some(super::record::copy_on_bind(val, heap));
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
    // A default binds like any parameter: a value record is copied (L§4.14).
    let value = super::record::copy_on_bind(take_value(machine, resolved.ast.span(default))?, heap);
    let top = machine.frames.len() - 1;
    local::write(heap, &mut machine.frames[top].locals[slot as usize], value);
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

/// The callee's name for an argument-error's `details.callee` (S-58): a named `to`/`fn`'s
/// declared name, or `None` for an anonymous `fn` (and for a block, whose caller passes
/// `None` directly).
fn callee_name(resolved: &ResolvedModule, decl: NodeId) -> Option<&str> {
    match resolved.ast.node(decl) {
        Node::Callable { name, .. } => name.as_deref(),
        _ => None,
    }
}

/// The keyword-bindable parameter names of a callable (its ordinary, non-block parameters)
/// — the valid names an `unknown-keyword`'s `details.parameters` carries for a "did you
/// mean?" hint.
fn keyword_parameters(params: &[ParamInfo]) -> Vec<Box<str>> {
    params
        .iter()
        .filter(|p| !p.is_block)
        .map(|p| p.name.clone())
        .collect()
}
