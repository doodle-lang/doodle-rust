//! Module loading state (engine spec E§6, S-60): the parked-suspension kinds, the
//! per-module load-state machine, and the path/canonical caches that make loading a
//! **singleton** (L§11.3).
//!
//! An `import` for a not-yet-loaded, non-native module path **suspends** — a
//! capability-style request (E§7.5) the host resolves with the module's source, a
//! `NotFound`, or a `Raise`. So the machine's single parked slot ([`Machine::pending`])
//! holds *either* a capability request *or* an import request — [`Suspension`] — never
//! both, and the drive loop routes each to its resolution path. The load bookkeeping
//! ([`ModuleLoad`]) lives on the [`Machine`](super::Machine) so the few frame-lifecycle
//! sites that flip a module `loading → loaded/failed` reach it through `&mut machine`
//! with no signature churn; it holds no heap references, so it is not a GC root. The
//! immutable per-module resolved AST + namespace live in the instance's module table.

use super::cont::Cont;
use super::error::{ExceptionKind, Raise};
use super::frame::FrameKind;
use super::intrinsic::PendingRequest;
use super::{CellIdx, LoadedModule, Machine, Value};
use crate::ast::{ImportTarget, Node, NodeId};
use crate::heap::Heap;
use crate::resolve::ResolvedModule;
use crate::span::{ModuleId, Span};

/// The single suspension a [`Machine`](super::Machine) can have parked (E§7.5): a
/// suspending **capability** call awaiting a host value/raise, or an **import**
/// awaiting the host's module resolution. Mutually exclusive — at most one is parked,
/// and which one determines whether the drive returns [`Outcome::Suspended`] or
/// [`Outcome::SuspendedImport`](crate::drive::Outcome::SuspendedImport).
pub(crate) enum Suspension {
    /// A suspending-capability call (E§5.3/§7.5): resolved with a value or a raise.
    Capability(PendingRequest),
    /// An `import` of a not-yet-loaded, non-native module (E§6): resolved with the
    /// module source, a `NotFound`, or a `Raise`.
    Import(PendingImport),
}

/// A parked import request (E§6, S-60): the machine hit an `import` for a module path
/// it has not loaded, so it stores the request identity here and the drive returns
/// [`Outcome::SuspendedImport`](crate::drive::Outcome::SuspendedImport). The importer
/// frame stays parked beneath; the host resolves with the module's source (then the
/// engine parses, pushes the module's top-level frame, and drives it), a `NotFound`
/// (→ a `module-not-found` raise here), or a `Raise` (→ that value raised here).
pub(crate) struct PendingImport {
    /// The dotted module path requested — the request identity (E§6/§7.5).
    pub(crate) path: Vec<Box<str>>,
    /// The importing module, for the not-found / cycle diagnostics.
    pub(crate) importer: ModuleId,
    /// The `import` statement's span: where a `NotFound`/`Raise` resolution raises.
    pub(crate) span: Span,
}

/// A module's load state (L§11.3, E§6, S-8). A module absent from the table is *unloaded*
/// (represented by absence, not a variant); once loading begins it is one of these.
#[derive(Clone, Copy, Debug)]
pub(crate) enum LoadState {
    /// Its top-level is on the stack, executing (a re-entrant `import` of it is a
    /// **circular import**, L§11.3).
    Loading,
    /// Its top-level completed; imports of it bind against the cached instance.
    Loaded,
    /// Its load **failed** (S-8) — its top-level raised, or its source had static errors
    /// (a `module-load-error`). The state **retains that exception value**, so a re-import
    /// **re-raises it unchanged** (no retry, no wrapping, no fresh slug): a
    /// determinism/replay requirement (a reload run must reproduce the same raise) and a
    /// disambiguation from `Loading` (a re-import of a failed module is not a cycle). The
    /// retained value is a GC root (`gc.rs`). Re-raise is latent in a single run — a load
    /// failure is uncatchable (imports are module-level, never inside a `try`) so it
    /// terminates the program; the retained value is re-raised only once reload/REPL
    /// (M9b) can re-import.
    Failed(Value),
}

/// Per-instance module-load bookkeeping (E§6, L§11.3), held on the [`Machine`](super::Machine).
///
/// `states` is parallel to the instance's module table by [`ModuleId`] index; the two
/// caches make loading a singleton: `by_path` short-circuits a repeated import of the
/// *same requested path* (no re-suspend), and `by_canonical` dedupes distinct paths the
/// host maps to one canonical module (L§11.3). Both are small ordered lists scanned
/// linearly — deterministic, no hashing on an observable path (E§11).
pub(crate) struct ModuleLoad {
    states: Vec<LoadState>,
    by_path: Vec<(Box<str>, ModuleId)>,
    by_canonical: Vec<(Box<str>, ModuleId)>,
    not_modules: Vec<Box<str>>,
}

impl ModuleLoad {
    /// The bookkeeping for a fresh instance whose only module is the main module
    /// (`ModuleId(0)`): its top-level runs from the start, so it is `Loading` until the
    /// program completes. It has no host path/canonical id, so the caches start empty —
    /// a sub-module importing the entry module by path is an obscure corner deferred
    /// past M5.1.
    pub(crate) fn new() -> Self {
        ModuleLoad {
            states: vec![LoadState::Loading],
            by_path: Vec::new(),
            by_canonical: Vec::new(),
            not_modules: Vec::new(),
        }
    }

    /// The load state of `module` (its table index must be in range — every table
    /// module has a state).
    pub(crate) fn state(&self, module: ModuleId) -> LoadState {
        self.states[module.0 as usize]
    }

    /// Flips `module` to `state` (a frame-lifecycle transition: `loading → loaded` on
    /// its top-level completing, `loading → failed` on a raise unwinding out of it).
    pub(crate) fn set_state(&mut self, module: ModuleId, state: LoadState) {
        self.states[module.0 as usize] = state;
    }

    /// The module a requested `path` already resolved to, if any (the repeat-import
    /// short-circuit).
    pub(crate) fn by_path(&self, path: &[Box<str>]) -> Option<ModuleId> {
        let key = join_path(path);
        self.by_path
            .iter()
            .find(|(p, _)| **p == *key)
            .map(|(_, id)| *id)
    }

    /// The module a host `canonical_id` already loaded as, if any (the singleton-by-
    /// canonical-identity dedupe, L§11.3).
    pub(crate) fn by_canonical(&self, canonical: &str) -> Option<ModuleId> {
        self.by_canonical
            .iter()
            .find(|(c, _)| **c == *canonical)
            .map(|(_, id)| *id)
    }

    /// Registers a freshly pushed module (table index `id`) as `Loading`, keyed by the
    /// requested `path` and the host `canonical` id. `states` must be extended in lockstep
    /// with the instance's module table, so `id.0` equals the current `states` length.
    pub(crate) fn begin(&mut self, path: &[Box<str>], canonical: &str, id: ModuleId) {
        debug_assert_eq!(id.0 as usize, self.states.len(), "state table out of step");
        self.states.push(LoadState::Loading);
        self.by_path.push((join_path(path), id));
        self.by_canonical.push((canonical.into(), id));
    }

    /// Aliases a requested `path` to an already-loaded `id` (a distinct path the host
    /// mapped to a canonical module already in the table) — the L§11.3 singleton, no
    /// second load.
    pub(crate) fn alias_path(&mut self, path: &[Box<str>], id: ModuleId) {
        self.by_path.push((join_path(path), id));
    }

    /// A dotted path that resolved to `module`, if one is recorded — for the circular-
    /// import and ambiguity diagnostics. Returns the first registered path (an aliased
    /// module may have several); the entry module has none.
    pub(crate) fn path_of(&self, module: ModuleId) -> Option<Box<str>> {
        self.by_path
            .iter()
            .find(|(_, id)| *id == module)
            .map(|(p, _)| p.clone())
    }

    /// Records that a requested dotted `path` is **not a module** (the host resolved it
    /// `NotFound`), so a later attempt at the same path takes the S-7 member fallback
    /// (`import a.b` → member `b` of module `a`) instead of re-suspending.
    pub(crate) fn mark_not_module(&mut self, path: &[Box<str>]) {
        self.not_modules.push(join_path(path));
    }

    /// Whether `path` is known not to be a module (a recorded `NotFound`, S-7).
    pub(crate) fn is_not_module(&self, path: &[Box<str>]) -> bool {
        let key = join_path(path);
        self.not_modules.iter().any(|p| **p == *key)
    }

    /// The exception value each `failed` module retains (S-8) — GC roots, so a retained
    /// value survives until a re-import re-raises it.
    pub(crate) fn failed_values(&self) -> impl Iterator<Item = Value> + '_ {
        self.states.iter().filter_map(|s| match s {
            LoadState::Failed(value) => Some(*value),
            _ => None,
        })
    }
}

/// Processes one target of an `import` statement (E§6, L§11.3): the target at `next` in
/// `import`'s `Node::Import`. A target whose module is already loaded advances to the
/// next target (name binding per import form is M5.2); one still loading raises a
/// **circular import**; one whose earlier load failed raises (**S-8**, no retry); one
/// absent from the table **parks an import suspension** and re-pushes this cont at the
/// same `next`, so that once the host supplies the source and the module's top level
/// drives to completion, the target is retried (now loaded) and processing continues.
pub(crate) fn step_import_targets(
    resolved: &ResolvedModule,
    modules: &mut [LoadedModule],
    heap: &mut Heap,
    machine: &mut Machine,
    import: NodeId,
    next: u32,
) -> Result<(), Raise> {
    let cur = resolved.canonical_id;
    let Node::Import(targets) = resolved.ast.node(import) else {
        unreachable!("ImportTargets over a non-import node");
    };
    // All targets processed: the `import` statement is done (nothing re-pushed).
    let Some(target) = targets.get(next as usize) else {
        return Ok(());
    };
    let path = target.path.clone();
    let span = resolved.ast.span(import);
    let importer = machine.frames.last().expect("an importer frame").module;
    // S-7: try the **whole path as a module** first; a path already known not to be a
    // module falls back to `member` (the last segment) of the **prefix** module.
    if let Some(id) = machine.load.by_path(&path) {
        return resolve_loaded(
            resolved, modules, heap, machine, cur, import, next, target, id, None, span,
        );
    }
    if path.len() > 1 && machine.load.is_not_module(&path) {
        // A wildcard's whole path is the module path — no member fallback; if that module
        // does not exist, it is a plain miss.
        if target.wildcard {
            return Err(Raise::new(
                ExceptionKind::ModuleNotFound,
                format!("the module `{}` was not found", join_path(&path)),
                span,
            ));
        }
        let prefix: Vec<Box<str>> = path[..path.len() - 1].to_vec();
        let member: Box<str> = path.last().expect("a multi-segment path").clone();
        if let Some(module) = machine.load.by_path(&prefix) {
            return resolve_loaded(
                resolved,
                modules,
                heap,
                machine,
                cur,
                import,
                next,
                target,
                module,
                Some(member),
                span,
            );
        }
        // Load the prefix module, then retry this target in fallback mode.
        return suspend_for(machine, import, next, prefix, importer, span);
    }
    // Unknown path: suspend to try the whole path as a module (E§6).
    suspend_for(machine, import, next, path, importer, span)
}

/// Re-pushes the `ImportTargets` cont at `next` (to retry this target once the module has
/// loaded) and parks an import suspension for `path` (E§6).
fn suspend_for(
    machine: &mut Machine,
    import: NodeId,
    next: u32,
    path: Vec<Box<str>>,
    importer: ModuleId,
    span: Span,
) -> Result<(), Raise> {
    push_import_targets(machine, import, next);
    machine.pending = Some(Suspension::Import(PendingImport {
        path,
        importer,
        span,
    }));
    Ok(())
}

/// Acts on a target whose module (`id`) is in the table: on `Loaded`, bind it (a
/// `member` of it via cell aliasing, else its module value) and advance; on `Loading`
/// raise a circular import; on `Failed` re-raise its retained exception (S-8).
#[allow(clippy::too_many_arguments)]
fn resolve_loaded(
    resolved: &ResolvedModule,
    modules: &mut [LoadedModule],
    heap: &mut Heap,
    machine: &mut Machine,
    cur: ModuleId,
    import: NodeId,
    next: u32,
    target: &ImportTarget,
    id: ModuleId,
    member: Option<Box<str>>,
    span: Span,
) -> Result<(), Raise> {
    match machine.load.state(id) {
        LoadState::Loaded => {
            match member {
                Some(member) => bind_member(cur, modules, machine, target, id, &member, span)?,
                None if target.wildcard => bind_wildcard(cur, modules, id),
                None => bind_module_value(cur, modules, heap, machine, target, id),
            }
            push_import_targets(machine, import, next + 1);
            Ok(())
        }
        LoadState::Loading => Err(circular_import(machine, id, span)),
        LoadState::Failed(value) => {
            let trace = super::observe::capture_trace(resolved, heap, machine, Some(span));
            machine.arm_raise_value(value, trace);
            Ok(())
        }
    }
}

/// Binds a whole-path module import (`import m` / `import m as y`) into the importer's
/// (`cur`'s) namespace (AD5): its `Value::Module` under the `as` alias or the path's last
/// segment; the new cell joins the permanent GC roots. A wildcard target binds nothing yet
/// (M5.2c).
fn bind_module_value(
    cur: ModuleId,
    modules: &mut [LoadedModule],
    heap: &mut Heap,
    machine: &mut Machine,
    target: &ImportTarget,
    id: ModuleId,
) {
    let name: Box<str> = target
        .alias
        .clone()
        .unwrap_or_else(|| target.path.last().expect("a non-empty import path").clone());
    let cell: CellIdx = heap.alloc_cell(crate::heap::CellKind::Const, Some(Value::Module(id)));
    modules[cur.0 as usize].namespace.push((name, cell));
    // A runtime-bound namespace cell must join the permanent GC roots (AD5): unlike the
    // load-seeded cells, it is added after `seed_namespace`, so root it explicitly.
    machine.module_root_cells.push(cell);
}

/// Records a wildcard import (`import m.*`, AD5, S-13): the exporter `id` joins the
/// importer's wildcard-source list (deduplicated). Its exported names are *not* bound into
/// the namespace — they resolve on use (`control::read_ref`), so an explicit binding always
/// wins and a name from two wildcards is ambiguous only when used.
fn bind_wildcard(cur: ModuleId, modules: &mut [LoadedModule], id: ModuleId) {
    let wildcards = &mut modules[cur.0 as usize].wildcards;
    if !wildcards.contains(&id) {
        wildcards.push(id);
    }
}

/// Binds a member import (`import m.x` / `import m.x as z`, S-7) into the importer's
/// (`cur`'s) namespace by **aliasing the exporter's cell** (AD5): the importer's name maps
/// to `m`'s existing binding cell for `member`, so reads see `m`'s live value (assignment
/// is already a static error, S-39). No new cell — the exporter's cell is already a GC
/// root. Raises if `member` is not one of `m`'s own module-level definitions.
#[allow(clippy::too_many_arguments)]
fn bind_member(
    cur: ModuleId,
    modules: &mut [LoadedModule],
    machine: &Machine,
    target: &ImportTarget,
    id: ModuleId,
    member: &str,
    span: Span,
) -> Result<(), Raise> {
    // No root to add — the aliased cell is the exporter's, already a GC root. Collect it
    // before mutating the importer's namespace (disjoint from its own read). Only a
    // **public** member may be imported (L§11.1): a private one raises `not-exported`, an
    // undeclared one `no-such-member`.
    let cell = {
        let exporter = &modules[id.0 as usize];
        match exporter.resolved.member_visibility(member) {
            crate::resolve::Membership::Exported => {
                super::control::find_cell(&exporter.namespace, member).expect("a member has a cell")
            }
            crate::resolve::Membership::Private => {
                let m = super::control::module_display(machine, id);
                return Err(super::control::not_exported(&m, member, span));
            }
            crate::resolve::Membership::Absent => {
                let m = super::control::module_display(machine, id);
                return Err(super::control::no_such_member(&m, member, span));
            }
        }
    };
    let name: Box<str> = target.alias.clone().unwrap_or_else(|| member.into());
    modules[cur.0 as usize].namespace.push((name, cell));
    Ok(())
}

/// Pushes an [`Cont::ImportTargets`] onto the importer's (top) frame.
fn push_import_targets(machine: &mut Machine, import: NodeId, next: u32) {
    machine
        .frames
        .last_mut()
        .expect("an importer frame")
        .conts
        .push(Cont::ImportTargets { import, next });
}

/// Builds the circular-import raise (L§11.3), naming the cycle from the module-top-level
/// frames currently on the stack (the active load chain, in load order): from `target`
/// (already loading, so on the chain) around to the innermost importer and back to it.
fn circular_import(machine: &Machine, target: ModuleId, span: Span) -> Raise {
    let chain: Vec<ModuleId> = machine
        .frames
        .iter()
        .filter_map(|f| match f.kind {
            FrameKind::ModuleTopLevel => Some(f.module),
            _ => None,
        })
        .collect();
    let start = chain.iter().position(|&m| m == target).unwrap_or(0);
    let mut labels: Vec<Box<str>> = chain[start..]
        .iter()
        .map(|&m| module_label(machine, m))
        .collect();
    labels.push(module_label(machine, target)); // close the cycle back to the target
    let rendered = labels
        .iter()
        .map(|s| s.as_ref())
        .collect::<Vec<&str>>()
        .join(" imports ");
    Raise::new(
        ExceptionKind::CircularImport,
        format!("circular import: {rendered}"),
        span,
    )
}

/// A module's path for a diagnostic, or `(the main module)` for the entry module (which
/// has no requested path).
fn module_label(machine: &Machine, module: ModuleId) -> Box<str> {
    machine
        .load
        .path_of(module)
        .unwrap_or_else(|| "(the main module)".into())
}

/// Joins dotted-path segments with `.` for the cache key and diagnostics
/// (`["a", "b"] → "a.b"`).
pub(crate) fn join_path(path: &[Box<str>]) -> Box<str> {
    path.iter()
        .map(|s| s.as_ref())
        .collect::<Vec<&str>>()
        .join(".")
        .into_boxed_str()
}
