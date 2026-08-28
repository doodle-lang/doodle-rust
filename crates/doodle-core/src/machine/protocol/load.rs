//! Protocol **load-time registration** (L§10, S-31): the runtime glue that turns
//! `protocol`/`implement` declarations into registry entries ([`define_protocol`],
//! [`define_implement`]) and seeds the well-known protocol names ([`seed_wellknown`]).
//! Split from the registry data type (`mod.rs`) and the dispatch half (`dispatch.rs`) for
//! length; this child module reaches the registry's private registration methods through
//! `super`.

use super::{MemberDecl, Registry};
use crate::ast::{CallableKind, Node, NodeId, Param};
use crate::heap::{CalObj, CallableTarget, CellKind, Heap, TypeObj};
use crate::machine::error::{ExceptionKind, Raise};
use crate::machine::value::{CalIdx, CellIdx};
use crate::machine::{LoadedModule, Machine, ProtocolType, TypeKind, Value, call, control};
use crate::resolve::{BodyKind, ResolvedModule};
use crate::span::{ModuleId, Span};

/// Seeds the well-known protocol names into a module's namespace at load (L§15, D-M5-1):
/// `Stringable`/`Hashable` bind to their protocol values (for `x is P` and `implement P for
/// T`), and `to_string` binds to a public dispatcher cell. `hash` is deliberately **not**
/// bound as a bare name — it is the `Hashable` member (definable via `implement`, dispatched
/// by dict keys), and the engine's hash value is not stable across builds, so it is not
/// exposed as an ordinary function. Interpolation calls `to_string` directly by member id, a
/// hidden binding immune to shadowing (S-37), so re-binding the `to_string` name here is only
/// for a user's explicit `to_string(x)` call. Seeded per module after the prelude, so a user
/// global of the same name shadows these (S-43 order); folded into the prelude import at M5.8.
pub(crate) fn seed_wellknown(
    namespace: &mut Vec<(Box<str>, CellIdx)>,
    heap: &mut Heap,
    module: ModuleId,
    registry: &Registry,
) {
    for (name, id) in [
        ("Stringable", registry.stringable_id()),
        ("Hashable", registry.hashable_id()),
    ] {
        let Some(id) = id else { continue };
        let ty = heap.alloc_type(TypeObj {
            kind: TypeKind::Protocol(ProtocolType {
                name: name.into(),
                id,
            }),
        });
        namespace.push((
            name.into(),
            heap.alloc_cell(CellKind::Const, Some(Value::Type(ty))),
        ));
    }
    if let Some(member) = registry.to_string_member() {
        let dcal = heap.alloc_callable(CalObj {
            module,
            target: CallableTarget::Dispatcher {
                member,
                protocol: None,
            },
            captures: Vec::new(),
        });
        namespace.push((
            "to_string".into(),
            heap.alloc_cell(CellKind::Dispatcher(member), Some(Value::Callable(dcal))),
        ));
    }
}

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
            modules,
            cur,
            machine,
            heap,
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
            // A user-declared protocol member has no engine seam; unimplemented → raise.
            native_default: None,
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

/// Resolves a protocol name to its registry id at load (for `extends`, L§10.1): resolves the
/// name as a free module name (own namespace → wildcards, the prelude among them) and requires
/// a protocol value (an undefined / not-yet-defined name raises name-/used-before-defined; a
/// non-protocol raises).
fn protocol_id_of(
    modules: &[LoadedModule],
    cur: usize,
    machine: &Machine,
    heap: &Heap,
    name: &str,
    span: Span,
) -> Result<u32, Raise> {
    match control::lookup_free(modules, cur, machine, heap, name, span)? {
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
    // Resolve `P` (own declaration or a prelude/wildcard protocol) to a protocol value.
    let proto_id = match control::lookup_free(modules, cur, machine, heap, &protocol, span)? {
        Value::Type(idx) => match &heap.type_value(idx).kind {
            TypeKind::Protocol(pt) => pt.id,
            _ => return Err(not_a_protocol(&protocol, span)),
        },
        _ => return Err(not_a_protocol(&protocol, span)),
    };
    // Resolve `T` to its runtime-type key(s) — a built-in type value is a prelude name.
    let Value::Type(tidx) = control::lookup_free(modules, cur, machine, heap, &type_name, span)?
    else {
        return Err(Raise::new(
            ExceptionKind::TypeMismatch,
            format!("`{type_name}` is not a type, so you can't `implement … for` it"),
            span,
        ));
    };
    let keys = super::dtype::type_keys(tidx, heap);
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

/// The load-time raise for `implement P …` where `P` is not a protocol value.
fn not_a_protocol(name: &str, span: crate::span::Span) -> Raise {
    Raise::new(
        ExceptionKind::TypeMismatch,
        format!("`{name}` is not a protocol"),
        span,
    )
}
