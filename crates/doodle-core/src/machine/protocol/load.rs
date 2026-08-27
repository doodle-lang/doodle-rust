//! Protocol **load-time registration** and **dispatch** (L§10, S-31): the runtime glue
//! that turns `protocol`/`implement` declarations into registry entries ([`define_protocol`],
//! [`define_implement`]) and resolves a member call to an implementation ([`dispatch_call`]).
//! Split from the registry data type (`mod.rs`) for length; this child module reaches the
//! registry's private registration and lookup methods through `super`.

use super::{Dispatch, MemberDecl, Registry, dispatch_type_of};
use crate::ast::{Arg, CallableKind, Node, NodeId, Param};
use crate::heap::{CalObj, CallableTarget, CellKind, Heap, TypeObj};
use crate::machine::error::{ExceptionKind, Raise};
use crate::machine::frame::Frame;
use crate::machine::value::CalIdx;
use crate::machine::{
    LoadedModule, Machine, ProtocolType, TypeKind, Value, block, call, control, local,
};
use crate::resolve::{BodyKind, ResolvedModule};
use crate::span::Span;

/// The `to`/`fn` kind of a protocol member's AST declaration.
fn body_kind(kind: CallableKind) -> BodyKind {
    match kind {
        CallableKind::Proc => BodyKind::Proc,
        CallableKind::Func => BodyKind::Func,
    }
}

/// The ordinary parameter names (in order) and the block parameter name of a member
/// signature (L§10.1): the signature a bare member call binds against (S-31).
fn split_params(params: &[Param]) -> (Vec<Box<str>>, Option<Box<str>>) {
    let mut ordinary = Vec::new();
    let mut block = None;
    for p in params {
        match p {
            Param::Ordinary { name, .. } => ordinary.push(name.clone()),
            Param::Block { name } => block = Some(name.clone()),
        }
    }
    (ordinary, block)
}

/// Registers a `protocol P … end` declaration at load (L§10.1): interns each member,
/// records the protocol, binds `P` to its protocol value, and binds each member name to a
/// **dispatcher cell** in the module namespace so a bare member call resolves to it (AD5).
pub(crate) fn define_protocol(
    resolved: &ResolvedModule,
    modules: &mut [LoadedModule],
    heap: &mut Heap,
    machine: &mut Machine,
    cur: usize,
    decl: NodeId,
) -> Result<(), Raise> {
    let Node::Protocol {
        name,
        extends,
        members,
        ..
    } = resolved.ast.node(decl)
    else {
        unreachable!("define_protocol over a non-Protocol node");
    };
    let (name, extends, members) = (name.clone(), extends.clone(), members.clone());
    let module = resolved.canonical_id;
    // Resolve the `extends` parent (S-61): it must already be a protocol value. A parent
    // is declared before its child (a forward or self reference reads an uninitialized
    // cell → used-before-defined), so the chain is acyclic and parent-first by construction.
    let parent = match &extends {
        Some(pname) => Some(protocol_id_of(
            heap,
            &modules[cur].namespace,
            pname,
            resolved.ast.span(decl),
        )?),
        None => None,
    };
    let mut member_decls = Vec::with_capacity(members.len());
    let mut dispatcher_names: Vec<(u32, Box<str>)> = Vec::new();
    for m in &members {
        let member = machine.protocols.intern_member(&m.name);
        let (params, block_param) = split_params(&m.params);
        // A default-bodied member interns its body as a callable (the resolver gave the
        // body node its own `CallableInfo`, so `make_callable` finds it, `dispatch.rs`).
        let default = m.body.map(
            |body| match call::make_callable(resolved, heap, machine, body) {
                Value::Callable(cal) => cal,
                _ => unreachable!("a protocol member body interns to a callable"),
            },
        );
        member_decls.push(MemberDecl {
            member,
            kind: body_kind(m.kind),
            params,
            block_param,
            default,
        });
        dispatcher_names.push((member, m.name.clone()));
    }
    let id = machine
        .protocols
        .add_protocol(name.clone(), module, parent, member_decls);
    // Bind `P` to its protocol value (the resolver declared `P` as a module global, so its
    // cell exists — created uninitialized at load, filled here when the declaration runs).
    let ty = heap.alloc_type(TypeObj {
        kind: TypeKind::Protocol(ProtocolType {
            name: name.clone(),
            id,
        }),
    });
    let cell =
        control::find_cell(&modules[cur].namespace, &name).expect("a protocol's global cell");
    heap.cell_mut(cell).value = Some(Value::Type(ty));
    // Bind each member name to a dispatcher cell (once per name: a second protocol
    // declaring the same member reuses the existing dispatcher, since dispatch is over
    // every protocol supplying the name).
    for (member, mname) in dispatcher_names {
        if control::find_cell(&modules[cur].namespace, &mname)
            .is_some_and(|c| matches!(heap.cell(c).kind, CellKind::Dispatcher(_)))
        {
            continue;
        }
        let dcal = heap.alloc_callable(CalObj {
            module,
            target: CallableTarget::Dispatcher {
                member,
                protocol: None,
            },
            captures: Vec::new(),
        });
        let dcell = heap.alloc_cell(CellKind::Dispatcher(member), Some(Value::Callable(dcal)));
        modules[cur].namespace.push((mname, dcell));
        machine.module_root_cells.push(dcell);
    }
    Ok(())
}

/// Resolves a protocol name to its registry id at load (for `extends`, L§10.1): reads its
/// namespace cell and requires a protocol value (an undefined / not-yet-defined name raises
/// name-/used-before-defined; a non-protocol raises).
fn protocol_id_of(
    heap: &Heap,
    namespace: &[(Box<str>, crate::machine::value::CellIdx)],
    name: &str,
    span: Span,
) -> Result<u32, Raise> {
    let cell = control::find_cell(namespace, name);
    match control::read_cell(heap, cell, name, span)? {
        Value::Type(idx) => match &heap.type_value(idx).kind {
            TypeKind::Protocol(pt) => Ok(pt.id),
            _ => Err(not_a_protocol(name, span)),
        },
        _ => Err(not_a_protocol(name, span)),
    }
}

/// Registers an `implement P for T … end` block at load (L§10.2): resolves `P` to a
/// protocol id and `T` to its runtime-type key(s), interns each method, and records the
/// `(protocol, type, member) → callable` associations. (Full signature conformance and the
/// missing-required-member check land in M5.5b.)
pub(crate) fn define_implement(
    resolved: &ResolvedModule,
    modules: &mut [LoadedModule],
    heap: &mut Heap,
    machine: &mut Machine,
    cur: usize,
    decl: NodeId,
) -> Result<(), Raise> {
    let span = resolved.ast.span(decl);
    let Node::Implement {
        protocol,
        type_name,
        methods,
    } = resolved.ast.node(decl)
    else {
        unreachable!("define_implement over a non-Implement node");
    };
    let (protocol, type_name, methods) = (protocol.clone(), type_name.clone(), methods.clone());
    let namespace = &modules[cur].namespace;
    // Resolve `P` to a protocol value (its declaration ran earlier at the module top level).
    let pcell = control::find_cell(namespace, &protocol);
    let proto_id = match control::read_cell(heap, pcell, &protocol, span)? {
        Value::Type(idx) => match &heap.type_value(idx).kind {
            TypeKind::Protocol(pt) => pt.id,
            _ => return Err(not_a_protocol(&protocol, span)),
        },
        _ => return Err(not_a_protocol(&protocol, span)),
    };
    // Resolve `T` to its runtime-type key(s).
    let tcell = control::find_cell(namespace, &type_name);
    let Value::Type(tidx) = control::read_cell(heap, tcell, &type_name, span)? else {
        return Err(Raise::new(
            ExceptionKind::TypeMismatch,
            format!("`{type_name}` is not a type, so you can't `implement … for` it"),
            span,
        ));
    };
    let keys = Registry::type_keys(tidx, heap);
    if keys.is_empty() {
        return Err(Raise::new(
            ExceptionKind::TypeMismatch,
            format!("you can't implement a protocol for `{type_name}`"),
            span,
        ));
    }
    // Intern each method's callable, keyed by its member name.
    let mut resolved_methods: Vec<(u32, CalIdx)> = Vec::with_capacity(methods.len());
    for method in &methods {
        let Node::Callable {
            name: Some(mname), ..
        } = resolved.ast.node(*method)
        else {
            unreachable!("an implement method is a named callable");
        };
        let member = machine.protocols.intern_member(mname);
        let Value::Callable(cal) = call::make_callable(resolved, heap, machine, *method) else {
            unreachable!("a callable declaration interns to a callable value");
        };
        resolved_methods.push((member, cal));
    }
    for key in keys {
        machine
            .protocols
            .add_impl(proto_id, key, resolved_methods.clone());
    }
    Ok(())
}

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
        let p = match arg {
            Arg::Positional(_) => {
                if pos >= ordinary.len() {
                    return Err(arg_err(span, "too many arguments for this call".into()));
                }
                let p = pos;
                pos += 1;
                p
            }
            Arg::Keyword { name, .. } => {
                match ordinary.iter().position(|n| n.as_ref() == name.as_ref()) {
                    Some(p) => p,
                    None => {
                        return Err(arg_err(
                            span,
                            format!("`{name}` isn't a parameter of this protocol member"),
                        ));
                    }
                }
            }
        };
        if ordered[p].is_some() {
            return Err(arg_err(
                span,
                format!("`{}` was given more than once", ordinary[p]),
            ));
        }
        ordered[p] = Some(val);
    }
    // The dispatch argument is the value bound to the first parameter (S-31).
    let Some(first) = ordered.first().copied().flatten() else {
        return Err(arg_err(
            span,
            "this protocol call is missing its first argument".into(),
        ));
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
                        return Err(arg_err(
                            span,
                            format!("missing argument `{}` for this call", ordinary[i]),
                        ));
                    }
                }
            }
            enter_dispatch_target(resolved, modules, heap, machine, call, cal, values, span)
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
        )),
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
        )),
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
            return Err(arg_err(span, "this call is missing an argument".into()));
        };
        slots[pi.slot as usize] = Some(crate::machine::record::copy_on_bind(value, heap));
        next += 1;
    }
    if next != ordered.len() {
        return Err(arg_err(span, "too many arguments for this call".into()));
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

/// An argument-shape raise during dispatch (L§8.3, L§10.3).
fn arg_err(span: Span, message: String) -> Raise {
    Raise::new(ExceptionKind::ArgumentError, message, span)
}

/// The load-time raise for `implement P …` where `P` is not a protocol value.
fn not_a_protocol(name: &str, span: crate::span::Span) -> Raise {
    Raise::new(
        ExceptionKind::TypeMismatch,
        format!("`{name}` is not a protocol"),
        span,
    )
}
