//! Name resolution and the name-miss raise helpers: reading a free module-level name across a
//! module's own namespace and its wildcard imports (S-13/S-60), the `with`-target parameter-cell
//! resolution (S-39), and the cross-module member-access / name-not-defined raises. Split from
//! `control/mod.rs` (the control-flow transitions) to stay within the hygiene length limit.

use super::Namespace;
use crate::ast::{Node, NodeId};
use crate::heap::Heap;
use crate::machine::error::{ExceptionKind, Raise};
use crate::machine::exception::DetailVal;
use crate::machine::{CellIdx, LoadedModule, Machine, Value};
use crate::resolve::{Resolution, ResolvedModule};
use crate::span::{ModuleId, Span};

pub(crate) fn read_cell(
    heap: &Heap,
    cell: Option<CellIdx>,
    name: &str,
    span: Span,
) -> Result<Value, Raise> {
    match cell {
        Some(c) => match heap.cell(c).value {
            Some(v) => Ok(v),
            None => Err(used_before_defined(name, span)),
        },
        None => Err(name_not_defined(name, span)),
    }
}

/// Resolves a `with`'s dynamic-parameter cell and reads its current value — for `with` to save
/// before it rebinds (machine-design §13, L§5.5). The name resolves like any free name (own
/// namespace → wildcards, S-39/S-13): an own declaration or selective import in the namespace,
/// else a wildcard-supplied name (two distinct wildcard bindings raise `ambiguous-import`, since
/// a `with` target is a *use*). The target must be a **dynamic parameter**; an imported
/// non-parameter — whose kind the resolver could not see — raises `with-target-not-parameter`
/// here, before any binding. Raises `NameNotDefined`/`UsedBeforeDefined` as a name read would.
pub(crate) fn param_cell(
    modules: &[LoadedModule],
    cur: usize,
    machine: &Machine,
    heap: &Heap,
    name: &str,
    span: Span,
) -> Result<(CellIdx, Value), Raise> {
    let cell = match find_cell(&modules[cur].namespace, name) {
        Some(c) => c,
        None => wildcard_cell(modules, cur, machine, name, span)?,
    };
    if !matches!(heap.cell(cell).kind, crate::heap::CellKind::Parameter) {
        return Err(with_target_not_parameter(name, heap.cell(cell).kind, span));
    }
    let old = read_cell(heap, Some(cell), name, span)?;
    Ok((cell, old))
}

/// The runtime `with-target-not-parameter` raise (L§5.5, S-58): a `with` targeted an imported
/// name that is not a dynamic parameter. The kind word is as precise as the cell records — a
/// `to`/`fn`/`record`/`const` all bind a `const` cell, so all read as "a constant" here (the
/// static diagnostic distinguishes them for a same-module target).
fn with_target_not_parameter(name: &str, kind: crate::heap::CellKind, span: Span) -> Raise {
    use crate::heap::CellKind;
    let (what, kind_slug) = match kind {
        CellKind::Const => ("a constant", "constant"),
        CellKind::Let => ("a variable", "variable"),
        CellKind::Dispatcher(_) => ("a protocol member", "protocol-member"),
        CellKind::Parameter => ("a dynamic parameter", "parameter"),
    };
    // `details.module` (the exporter, S-58 `{name, module, kind}`) is deferred: this fires for
    // an imported target, but the exporter isn't tracked on the resolved cell (selective imports
    // bind a cell with no provenance) — threading it needs the wildcard/namespace resolution to
    // return the source module. `name` + `kind` are carried now.
    Raise::new(
        ExceptionKind::WithTargetNotParameter,
        format!("`{name}` is {what}, not a dynamic parameter — `with` needs a parameter"),
        span,
    )
    .with_details(vec![
        ("name", DetailVal::str(name)),
        ("kind", DetailVal::str(kind_slug)),
    ])
}

/// Resolves a **free** module-level name (L§11.2): an explicit binding — the module's own
/// declaration or a selective import, both in its namespace — wins; otherwise it resolves
/// through the module's wildcard imports, the implicit prelude among them (S-60). Shared by
/// name reads (`read_ref`) and load-time protocol/type-name resolution (`implement`/`extends`).
pub(crate) fn lookup_free(
    modules: &[LoadedModule],
    cur: usize,
    machine: &Machine,
    heap: &Heap,
    name: &str,
    span: Span,
) -> Result<Value, Raise> {
    match find_cell(&modules[cur].namespace, name) {
        Some(cell) => read_cell(heap, Some(cell), name, span),
        None => wildcard_lookup(modules, cur, machine, heap, name, span),
    }
}

/// The outcome of resolving a free `name` across module `cur`'s wildcard imports (S-13/S-60):
/// a single **distinct binding** (one cell — two wildcards aliasing the *same* exporter cell,
/// S-39, are one binding), an **ambiguity** of two distinct bindings, or **nothing** (with the
/// module that has the name *privately*, for the helpful `not-exported` miss).
enum Wildcard {
    Bound(CellIdx),
    Ambiguous(ModuleId, ModuleId),
    None { private_in: Option<ModuleId> },
}

/// Resolves `name` across `cur`'s wildcard imports (`import m.*` and the implicit prelude, AD5),
/// scanning each source's own exports in import order and deduping by cell identity. Shared by
/// the read path ([`wildcard_lookup`]) and the `with`-target cell path ([`wildcard_cell`]).
fn wildcard_resolve(modules: &[LoadedModule], cur: usize, name: &str) -> Wildcard {
    let mut hits: Vec<(ModuleId, CellIdx)> = Vec::new();
    let mut private_in: Option<ModuleId> = None;
    for &w in &modules[cur].wildcards {
        // A wildcard exposes only the exporter's own **exported** definitions (L§11.2/§11.1).
        match modules[w.0 as usize].resolved.member_visibility(name) {
            crate::resolve::Membership::Exported => {
                if let Some(cell) = find_cell(&modules[w.0 as usize].namespace, name)
                    && !hits.iter().any(|(_, c)| *c == cell)
                {
                    hits.push((w, cell));
                }
            }
            // Declared but not exported: remembered for the helpful `not-exported` miss.
            crate::resolve::Membership::Private => {
                private_in.get_or_insert(w);
            }
            crate::resolve::Membership::Absent => {}
        }
    }
    match hits.as_slice() {
        [] => Wildcard::None { private_in },
        [(_, cell)] => Wildcard::Bound(*cell),
        [(a, _), (b, _), ..] => Wildcard::Ambiguous(*a, *b),
    }
}

/// Resolves a free `name` not found in `cur`'s namespace to a **value** through its wildcard
/// imports (S-13/S-60): one distinct binding reads its live alias; two are ambiguous; none is
/// undefined (or `not-exported` when a source has it privately).
fn wildcard_lookup(
    modules: &[LoadedModule],
    cur: usize,
    machine: &Machine,
    heap: &Heap,
    name: &str,
    span: Span,
) -> Result<Value, Raise> {
    match wildcard_resolve(modules, cur, name) {
        Wildcard::Bound(cell) => read_cell(heap, Some(cell), name, span),
        Wildcard::Ambiguous(a, b) => Err(ambiguous_import(machine, name, a, b, span)),
        Wildcard::None { private_in } => Err(wildcard_miss(machine, private_in, name, span)),
    }
}

/// Resolves a free `name` not found in `cur`'s namespace to a **cell** through its wildcard
/// imports — for `with` binding an imported dynamic parameter (S-39): it needs the exporter's
/// own cell (the same one reads see), not just its value. Same resolution as [`wildcard_lookup`].
fn wildcard_cell(
    modules: &[LoadedModule],
    cur: usize,
    machine: &Machine,
    name: &str,
    span: Span,
) -> Result<CellIdx, Raise> {
    match wildcard_resolve(modules, cur, name) {
        Wildcard::Bound(cell) => Ok(cell),
        Wildcard::Ambiguous(a, b) => Err(ambiguous_import(machine, name, a, b, span)),
        Wildcard::None { private_in } => Err(wildcard_miss(machine, private_in, name, span)),
    }
}

/// The raise for a name supplied by two distinct wildcard bindings (L§11.2, S-13).
fn ambiguous_import(machine: &Machine, name: &str, a: ModuleId, b: ModuleId, span: Span) -> Raise {
    let from = |m: ModuleId| machine.load.path_of(m).unwrap_or_else(|| "?".into());
    let (fa, fb) = (from(a), from(b));
    Raise::new(
        ExceptionKind::AmbiguousImport,
        format!(
            "`{name}` is imported by wildcards from both `{fa}` and `{fb}` — import it \
             explicitly to say which one you mean",
        ),
        span,
    )
    .with_details(vec![
        ("name", DetailVal::str(name)),
        ("modules", DetailVal::strs([fa.to_string(), fb.to_string()])),
    ])
}

/// The raise for a wildcard miss: `not-exported` when a source has the name privately (the
/// helpful "I imported everything, why isn't it there?" case), else `name-not-defined`.
fn wildcard_miss(machine: &Machine, private_in: Option<ModuleId>, name: &str, span: Span) -> Raise {
    match private_in {
        Some(w) => not_exported(&module_display(machine, w), name, span),
        None => name_not_defined(name, span),
    }
}

/// Finds a module cell by name (linear scan — the namespace is small and this
/// keeps lookup deterministic and hashing-free).
pub(crate) fn find_cell(namespace: &Namespace, name: &str) -> Option<CellIdx> {
    for (n, cell) in namespace {
        if n.as_ref() == name {
            return Some(*cell);
        }
    }
    None
}

/// The resolver's resolution for `node` (an `Ident` reference, or an lvalue
/// target — always resolved).
pub(crate) fn resolution(resolved: &ResolvedModule, node: NodeId) -> Resolution {
    resolved.resolutions[node.0 as usize].expect("a reference/lvalue is always resolved")
}

/// The name a `Node::Ident` reference names (for a name-read diagnostic).
pub(super) fn ident_name(resolved: &ResolvedModule, node: NodeId) -> &str {
    match resolved.ast.node(node) {
        Node::Ident(name) => name,
        _ => "the name", // read_ref is only ever called on an Ident
    }
}

/// The name a binding declaration node binds (for a bind diagnostic).
pub(super) fn decl_name(resolved: &ResolvedModule, decl: NodeId) -> &str {
    match resolved.ast.node(decl) {
        Node::Let { name, .. } | Node::Const { name, .. } | Node::Parameter { name, .. } => name,
        Node::Callable {
            name: Some(name), ..
        } => name,
        Node::Record { name, .. } => name,
        _ => unreachable!("bind_decl over a node with no binding name"),
    }
}

pub(crate) fn name_not_defined(name: &str, span: Span) -> Raise {
    Raise::new(
        ExceptionKind::NameNotDefined,
        format!("`{name}` isn't defined"),
        span,
    )
    .with_details(vec![("name", DetailVal::str(name))])
}

pub(super) fn used_before_defined(name: &str, span: Span) -> Raise {
    Raise::new(
        ExceptionKind::UsedBeforeDefined,
        format!("`{name}` is used here before it's defined"),
        span,
    )
    .with_details(vec![("name", DetailVal::str(name))])
}

/// The display name of a module for a member-access diagnostic (L§11.1): its import path
/// (or native module name), or a placeholder for the pathless entry module.
pub(crate) fn module_display(machine: &Machine, module: ModuleId) -> Box<str> {
    machine
        .load
        .path_of(module)
        .unwrap_or_else(|| "this module".into())
}

/// A cross-module access of a **private** (declared but not exported) member (L§11.1): loud
/// and true, pointing at the fix (add it to the module's `exports`).
pub(crate) fn not_exported(module: &str, member: &str, span: Span) -> Raise {
    Raise::new(
        ExceptionKind::NotExported,
        format!(
            "`{member}` is private to module `{module}` — add it to `{module}`'s `exports` \
             to use it here"
        ),
        span,
    )
    .with_details(vec![
        ("module", DetailVal::str(module)),
        ("member", DetailVal::str(member)),
    ])
}

/// A cross-module access of a member the module **does not declare** (L§11.1): the module
/// container's access-miss kind (never `no-such-field`, which is the record's).
pub(crate) fn no_such_member(module: &str, member: &str, span: Span) -> Raise {
    Raise::new(
        ExceptionKind::NoSuchMember,
        format!("module `{module}` has no member `{member}`"),
        span,
    )
    .with_details(vec![
        ("module", DetailVal::str(module)),
        ("member", DetailVal::str(member)),
    ])
}
