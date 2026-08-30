//! Protocol member **dispatch** (L§10.3, S-31): resolving a member call to its implementation
//! and entering it. [`dispatch_call`] binds arguments against the member signature, dispatches
//! on the first argument's runtime type, and either enters the resolved source implementation
//! ([`enter_dispatch_target`]) or yields a native default's value; [`enter_unary`] is the
//! single-argument entry interpolation drives for `Stringable.to_string`. Split from the
//! registration half (`load.rs`) for length; both are the runtime glue over `mod.rs`.

use super::{Dispatch, NativeDefault, dispatch_type_of};
use crate::ast::{Arg, Node, NodeId};
use crate::heap::Heap;
use crate::machine::error::{ExceptionKind, Raise};
use crate::machine::exception as exc;
use crate::machine::frame::Frame;
use crate::machine::value::CalIdx;
use crate::machine::{LoadedModule, Machine, Value, block, local};
use crate::resolve::ResolvedModule;
use crate::span::Span;

/// Dispatches a protocol member call (L§10.3, S-31): binds the call's arguments against the
/// member's declared signature, dispatches on the runtime type of the value bound to the
/// **first parameter**, and enters the resolved implementation — or raises not-implemented
/// / ambiguous. `protocol_filter` is `Some(id)` for the qualified form `P.member`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_call(
    resolved: &ResolvedModule,
    modules: &mut [LoadedModule],
    heap: &mut Heap,
    machine: &mut Machine,
    call: NodeId,
    member: u32,
    protocol_filter: Option<u32>,
    arg_values: Vec<Value>,
) -> Result<(), Raise> {
    let span = resolved.ast.span(call);
    // The member signature the call binds against (S-31): its ordinary parameter names.
    // A dispatcher's member is always declared by (the filtered) protocol.
    let ordinary: Vec<Box<str>> = machine
        .protocols
        .member_call_signature(member, protocol_filter)
        .map(|(names, _)| names.to_vec())
        .expect("a dispatcher's member is declared by its protocol");
    // Bind positional / keyword arguments to the member's ordinary parameters by name; the
    // `do … end` block argument is bound to the implementation's block parameter later.
    let Node::Call { args, .. } = resolved.ast.node(call) else {
        unreachable!("dispatch_call over a non-Call node");
    };
    let mut ordered: Vec<Option<Value>> = vec![None; ordinary.len()];
    let mut pos = 0usize;
    for (arg, &val) in args.iter().zip(arg_values.iter()) {
        let p =
            match arg {
                Arg::Positional(_) => {
                    if pos >= ordinary.len() {
                        let got = args
                            .iter()
                            .filter(|a| matches!(a, Arg::Positional(_)))
                            .count();
                        return Err(Raise::new(
                            ExceptionKind::TooManyArguments,
                            "too many arguments for this call",
                            span,
                        )
                        .with_details(exc::too_many_arguments_details(None, ordinary.len(), got)));
                    }
                    let p = pos;
                    pos += 1;
                    p
                }
                Arg::Keyword { name, .. } => {
                    match ordinary.iter().position(|n| n.as_ref() == name.as_ref()) {
                        Some(p) => p,
                        None => {
                            return Err(Raise::new(
                                ExceptionKind::UnknownKeyword,
                                format!("`{name}` isn't a parameter of this protocol member"),
                                span,
                            )
                            .with_details(
                                exc::unknown_keyword_details(None, name.as_ref(), &ordinary),
                            ));
                        }
                    }
                }
            };
        if ordered[p].is_some() {
            return Err(Raise::new(
                ExceptionKind::DuplicateArgument,
                format!("`{}` was given more than once", ordinary[p]),
                span,
            )
            .with_details(exc::parameter_details(None, ordinary[p].as_ref())));
        }
        ordered[p] = Some(val);
    }
    // The dispatch argument is the value bound to the first parameter (S-31).
    let Some(first) = ordered.first().copied().flatten() else {
        let parameter = ordinary
            .first()
            .map_or("the first argument", |n| n.as_ref());
        return Err(Raise::new(
            ExceptionKind::MissingArgument,
            "this protocol call is missing its first argument",
            span,
        )
        .with_details(exc::parameter_details(None, parameter)));
    };
    let dt = dispatch_type_of(first, heap, modules, &machine.intrinsics);
    match machine.protocols.resolve(member, dt, protocol_filter, heap) {
        Dispatch::Call(cal) => {
            // A member parameter has no default (M5.5a), so every ordinary parameter must
            // be bound before the implementation runs.
            let mut values = Vec::with_capacity(ordinary.len());
            for (i, slot) in ordered.into_iter().enumerate() {
                match slot {
                    Some(v) => values.push(v),
                    None => {
                        return Err(Raise::new(
                            ExceptionKind::MissingArgument,
                            format!("missing argument `{}` for this call", ordinary[i]),
                            span,
                        )
                        .with_details(exc::parameter_details(None, ordinary[i].as_ref())));
                    }
                }
            }
            enter_dispatch_target(resolved, modules, heap, machine, call, cal, values, span)
        }
        // A well-known member (`to_string`/`hash`) with no explicit implementation for this
        // type falls to the engine's native seam (L§15, D-M5-1) and yields its value directly
        // — no frame. Reaches here from a bare `to_string(x)` or a qualified `Hashable.hash(x)`.
        Dispatch::Native(nd) => {
            machine.reg = Some(match nd {
                NativeDefault::Render => {
                    let text = crate::machine::stringify::render(heap, first);
                    Value::Str(heap.alloc_string(text.into_boxed_str()))
                }
                NativeDefault::Hash => {
                    let h = crate::machine::hash::native_key_hash(first, heap, span)?;
                    crate::machine::hash::hash_as_value(h, heap)
                }
            });
            Ok(())
        }
        Dispatch::NotImplemented {
            type_name,
            protocol,
            member,
        } => Err(Raise::new(
            ExceptionKind::ProtocolNotImplemented,
            format!(
                "`{type_name}` has no `{member}` — `{type_name}` doesn't implement `{protocol}`; \
                 add `implement {protocol} for {type_name}`"
            ),
            span,
        )
        .with_details(vec![
            ("type", exc::DetailVal::str(type_name.to_string())),
            ("protocol", exc::DetailVal::str(protocol.to_string())),
            ("member", exc::DetailVal::str(member.to_string())),
        ])),
        Dispatch::Ambiguous {
            member,
            protocols: (a, b),
            type_name,
        } => Err(Raise::new(
            ExceptionKind::AmbiguousMember,
            format!(
                "`{member}` is a member of both `{a}` and `{b}` for `{type_name}` — \
                 call `{a}.{member}(…)` or `{b}.{member}(…)` to say which one you mean"
            ),
            span,
        )
        .with_details(vec![
            ("member", exc::DetailVal::str(member.to_string())),
            (
                "protocols",
                exc::DetailVal::strs([a.to_string(), b.to_string()]),
            ),
            ("type", exc::DetailVal::str(type_name.to_string())),
        ])),
    }
}

/// Enters a resolved dispatch **implementation** (L§10.3): the `ordered` values are the
/// call's arguments already bound against the protocol member's signature (S-31 — keyword
/// names are the member's), so they fill the implementation's ordinary parameter slots
/// positionally (it conforms in arity and receives them in declaration order); the
/// `do … end` block argument, if any, binds to the implementation's block parameter. A
/// dispatched call always pushes a frame (proper tail dispatch is a later optimization).
#[allow(clippy::too_many_arguments)]
fn enter_dispatch_target(
    resolved: &ResolvedModule,
    modules: &[LoadedModule],
    heap: &mut Heap,
    machine: &mut Machine,
    call: NodeId,
    cal: CalIdx,
    ordered: Vec<Value>,
    span: Span,
) -> Result<(), Raise> {
    let callee_module = heap.callable(cal).module;
    let callee_resolved: &ResolvedModule = &modules[callee_module.0 as usize].resolved;
    let callable_id = heap.callable(cal).source_id() as usize;
    let info = &callee_resolved.callables[callable_id];
    // Place each bound value into its implementation parameter slot, in declaration order.
    let mut slots: Vec<Option<Value>> = vec![None; info.slot_count as usize];
    let mut next = 0usize;
    for pi in &info.params {
        if pi.is_block {
            continue;
        }
        let Some(&value) = ordered.get(next) else {
            return Err(Raise::new(
                ExceptionKind::MissingArgument,
                "this call is missing an argument",
                span,
            )
            .with_details(exc::parameter_details(None, pi.name.as_ref())));
        };
        slots[pi.slot as usize] = Some(crate::machine::record::copy_on_bind(value, heap));
        next += 1;
    }
    if next != ordered.len() {
        return Err(Raise::new(
            ExceptionKind::TooManyArguments,
            "too many arguments for this call",
            span,
        )
        .with_details(exc::too_many_arguments_details(None, next, ordered.len())));
    }
    let block_param = block::bind_block_argument(resolved, machine, call, &info.params, span)?;
    let body = info.body;
    let captured = heap.callable(cal).captures.clone();
    let locals = local::build(callee_resolved, heap, callable_id, &slots, &captured);
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
    Ok(())
}

/// Enters a **unary** protocol implementation — one argument, no block — the shape string
/// interpolation drives for `Stringable.to_string` (L§15, S-37). Pushes the impl frame; its
/// returned value lands in the register for the caller's resume continuation. `call_site` is
/// the interpolation's `StrLit` node, used for the frame's error attribution.
pub(crate) fn enter_unary(
    modules: &[LoadedModule],
    heap: &mut Heap,
    machine: &mut Machine,
    cal: CalIdx,
    arg: Value,
    call_site: NodeId,
    span: Span,
) -> Result<(), Raise> {
    let callee_module = heap.callable(cal).module;
    let callee_resolved: &ResolvedModule = &modules[callee_module.0 as usize].resolved;
    let callable_id = heap.callable(cal).source_id() as usize;
    let info = &callee_resolved.callables[callable_id];
    // A well-known unary member (`to_string`/`hash`) takes exactly its `self` — one ordinary
    // parameter, no block. A malformed implementation (extra parameter, or a block) must raise
    // rather than run with an unbound slot; the static conformance check catches a same-module
    // one, this is the backstop for a native-protocol implementation the resolver can't see.
    let ordinary = info.params.iter().filter(|p| !p.is_block).count();
    let has_block = info.params.iter().any(|p| p.is_block);
    if ordinary != 1 || has_block {
        // A malformed native-protocol implementation of a unary member: too few ordinary
        // parameters (missing the value) or too many (extra parameter/block).
        let message = "this implementation must take exactly one input (the value) and no block";
        let raise = if ordinary == 0 {
            Raise::new(ExceptionKind::MissingArgument, message, span)
                .with_details(exc::parameter_details(None, "the value"))
        } else {
            let got = ordinary + usize::from(has_block);
            Raise::new(ExceptionKind::TooManyArguments, message, span)
                .with_details(exc::too_many_arguments_details(None, 1, got))
        };
        return Err(raise);
    }
    let pi = info
        .params
        .iter()
        .find(|p| !p.is_block)
        .expect("one ordinary parameter, just checked");
    let mut slots: Vec<Option<Value>> = vec![None; info.slot_count as usize];
    slots[pi.slot as usize] = Some(crate::machine::record::copy_on_bind(arg, heap));
    let body = info.body;
    let captured = heap.callable(cal).captures.clone();
    let locals = local::build(callee_resolved, heap, callable_id, &slots, &captured);
    let serial = machine.next_frame_serial();
    let dyn_depth = machine.dyn_stack.len() as u32;
    machine.frames.push(Frame::callable(
        callee_module,
        cal,
        locals,
        body,
        serial,
        None, // a `to_string` takes no block
        call_site,
        dyn_depth,
    ));
    Ok(())
}
