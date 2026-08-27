//! Instance construction (engine spec E§3.1): `create`/`load*`/`load_full` — building a
//! `Ready` [`Instance`] from a resolved module, seeding its global namespace (S-43) and
//! its module top-level frame. Split from `machine.rs` (the `Instance`/`Machine`
//! definitions and lifecycle) so that file stays within the hygiene length limit; these
//! carry their own `impl Instance` block, as `lifecycle.rs`/`boundary.rs` do.

use super::{
    Arc, AtomicBool, CellIdx, Config, ConfigError, Cont, Directive, Frame, FusedCounter,
    HandleTable, Heap, Instance, InstanceState, Limits, Machine, ModuleLoad, ResolvedModule,
    TypeIdx, UNICODE_VERSION, UnicodeVersion, Value, intrinsic, limits, local, ring, types,
};
use crate::heap::CellKind;

impl Instance {
    /// Creates a `Ready` instance for `module` under `config` (engine spec E§3.1).
    /// Validates the config first (S-41): a requested Unicode version that is not the
    /// engine's build-pinned one is rejected — the engine supports exactly its pinned
    /// version, so a host or replay can assert the expected version at create time
    /// rather than diverge silently on grapheme/normalization behavior (E§11). `None`
    /// uses the pinned version.
    pub fn create(module: ResolvedModule, config: Config) -> Result<Self, ConfigError> {
        if let Some(requested) = config.unicode_version
            && requested != UNICODE_VERSION
        {
            return Err(ConfigError::UnsupportedUnicodeVersion {
                requested,
                pinned: UNICODE_VERSION,
            });
        }
        Ok(Self::load_with_limits(module, config.limits))
    }

    /// The Unicode/UCD version this engine is pinned to (L§4.4; the config's
    /// target-version field is validated against it, S-41).
    pub fn unicode_version() -> UnicodeVersion {
        UNICODE_VERSION
    }

    /// Loads a resolved module into a fresh `Ready` instance with the
    /// [`Default`](Limits) resource limits and no intrinsics. See [`load_full`](Self::load_full).
    pub fn load(module: ResolvedModule) -> Self {
        Self::load_full(module, Limits::default(), intrinsic::Registry::new())
    }

    /// Loads a resolved module under the given resource limits (E§10.2), no
    /// intrinsics. See [`load_full`](Self::load_full).
    pub fn load_with_limits(module: ResolvedModule, limits: Limits) -> Self {
        Self::load_full(module, limits, intrinsic::Registry::new())
    }

    /// Loads a resolved module with host-registered intrinsic foreign functions
    /// (E§5.1, S-43) and the [`Default`](Limits) limits. `registry` holds the
    /// intrinsics the host registered **before** this load (E§5.5); they seed as
    /// read-only global names after the program's declarations and the built-in
    /// type values, so a program's own declaration of the same name shadows one.
    pub fn load_with_intrinsics(module: ResolvedModule, registry: intrinsic::Registry) -> Self {
        Self::load_full(module, Limits::default(), registry)
    }

    /// Loads a resolved module with host-registered intrinsics (S-43) under the given
    /// resource limits (E§10.2) — [`load_with_intrinsics`](Self::load_with_intrinsics)
    /// and [`load_with_limits`](Self::load_with_limits) combined.
    pub fn load_with_intrinsics_and_limits(
        module: ResolvedModule,
        limits: Limits,
        registry: intrinsic::Registry,
    ) -> Self {
        Self::load_full(module, limits, registry)
    }

    /// Loads a resolved module into a fresh `Ready` instance (machine-design §18)
    /// under the given resource limits (E§10.2) and intrinsic registry (S-43). Each
    /// module-level name gets an **uninitialized** binding cell (its `let`/`const`
    /// fills it when it executes; a read before then is a use-before-defined error).
    /// The module top level becomes an ordinary, drivable `ModuleTopLevel` frame
    /// whose pending work sequences its statements.
    fn load_full(module: ResolvedModule, limits: Limits, intrinsics: intrinsic::Registry) -> Self {
        debug_assert!(
            matches!(
                module.ast.node(module.root),
                crate::ast::Node::Module { .. }
            ),
            "load: a resolved module's root must be the `Module` node"
        );
        let mut heap = Heap::new();
        let canonical_id = module.canonical_id;
        // The built-in `Error` record type (L§12.1, S-58): the value record
        // `Error(kind, message, details)` the engine raises and Doodle code can
        // construct/inspect. One `Error` type is shared by every module in the instance,
        // so it is created once here and remembered on the machine (an engine raise
        // materializes one without a namespace scan) and passed to each module's seeding.
        let error_type = alloc_error_type(&mut heap);
        let namespace = seed_namespace(&module, &mut heap, error_type, &intrinsics);
        // The main module's namespace cells are the instance's first permanent GC roots
        // (each loaded module appends its own, `machine/import.rs`); captured before
        // `namespace` moves into the module table.
        let module_root_cells: Vec<CellIdx> = namespace.iter().map(|(_, cell)| *cell).collect();
        // The module top level's construct-body locals may be cell-boxed (a `fn`
        // captured one, §7), so build its slots like any frame — no params, no
        // captures. `raw` is all-`None`; `let`s fill the slots as they execute.
        let module_id = module
            .callables
            .iter()
            .position(|c| matches!(c.kind, crate::resolve::BodyKind::ModuleTopLevel))
            .expect("a resolved module has a top-level callable");
        let raw = vec![None; module.callables[module_id].slot_count as usize];
        let locals = local::build(&module, &mut heap, module_id, &raw, &[]);
        let root = module.root;
        let resolved = Arc::new(module);
        let frame = Frame::module_top_level(
            canonical_id, // the main module — `ModuleId(0)`
            locals,
            Cont::Seq {
                block: root,
                next: 0,
            },
            0, // the module frame is frame serial 0; further frames count up
        );
        Instance {
            modules: vec![super::LoadedModule {
                resolved,
                namespace,
                wildcards: Vec::new(),
            }],
            heap,
            machine: Machine {
                frames: vec![frame],
                reg: None,
                frame_serial: 1,
                unwind: None,
                ring: ring::RingBuffer::new(),
                fuel: FusedCounter::new(&limits),
                gc_threshold: limits::GC_MIN_BYTES,
                handles: HandleTable::new(),
                intrinsics,
                output: Vec::new(),
                pending: None,
                load: ModuleLoad::new(),
                protocols: super::protocol::Registry::default(),
                module_root_cells,
                directive: Directive::RunToCompletion,
                pending_fault: None,
                foreign_roots: Vec::new(),
                dyn_stack: Vec::new(),
                handling: Vec::new(),
                error_type,
                reentry_depth: 0,
                gc_every_safe_point: false,
                cancel: Arc::new(AtomicBool::new(false)),
                limits,
            },
            state: InstanceState::Ready,
        }
    }
}

/// Allocates the built-in `Error` record type (L§12.1, S-58) into `heap`, returning its
/// index. One per instance, shared by every module.
fn alloc_error_type(heap: &mut Heap) -> TypeIdx {
    heap.alloc_type(crate::heap::TypeObj {
        kind: crate::machine::TypeKind::Record(crate::machine::RecordType {
            name: "Error".into(),
            fields: Box::new(["kind".into(), "message".into(), "details".into()]),
            is_ref: false,
        }),
    })
}

/// Seeds a module's namespace in S-43 order: the module's own globals (uninitialized
/// cells — their `let`/`const`/`to`/`fn` fill them in execution order, the temporal dead
/// zone), then the built-in type-value prelude, then the `Error` type value, then the host
/// intrinsics. Each later group is appended after, so an earlier binding of the same name
/// wins the linear `find_cell` scan (control.rs): a user global shadows a type value or an
/// intrinsic — the relationship the M5 prelude star-import preserves. Every module in the
/// instance (the main module or one loaded by `import`, E§6) is seeded the same way, so an
/// imported module's own top level sees the same prelude; `error_type` is the shared
/// built-in `Error`. The built-in type *values* are per-module heap objects for now —
/// cross-module type-value identity (an `Int` from one module `==` another's) is an M5.5
/// dispatch concern, not yet observable (an imported module runs only for effect at M5.1).
pub(super) fn seed_namespace(
    module: &ResolvedModule,
    heap: &mut Heap,
    error_type: TypeIdx,
    intrinsics: &intrinsic::Registry,
) -> Vec<(Box<str>, CellIdx)> {
    let mut namespace: Vec<(Box<str>, CellIdx)> = module
        .globals
        .iter()
        .map(|g| (g.name.clone(), heap.alloc_cell(cell_kind_of(g.kind), None)))
        .collect();
    for &(name, builtin) in types::BUILTINS {
        let kind = crate::machine::TypeKind::Builtin(builtin);
        let ty = Value::Type(heap.alloc_type(crate::heap::TypeObj { kind }));
        namespace.push((name.into(), heap.alloc_cell(CellKind::Const, Some(ty))));
    }
    namespace.push((
        "Error".into(),
        heap.alloc_cell(CellKind::Const, Some(Value::Type(error_type))),
    ));
    for (i, intrinsic) in intrinsics.iter().enumerate() {
        // Each intrinsic interns to one foreign `CalObj` (its registration index is the
        // `CallableTarget::Intrinsic` id) held by a read-only global cell.
        let cal = heap.alloc_callable(crate::heap::CalObj {
            module: module.canonical_id,
            target: crate::heap::CallableTarget::Intrinsic(i as u32),
            captures: Vec::new(),
        });
        namespace.push((
            intrinsic.name.clone(),
            heap.alloc_cell(CellKind::Const, Some(Value::Callable(cal))),
        ));
    }
    namespace
}

/// The [`CellKind`] a module global's declaration category maps to (machine-design
/// §6): a `let` is the only mutable, `=`-assignable binding; a `parameter` is a
/// dynamic parameter; every other declaration (`const`, `to`/`fn`, `record`,
/// `protocol`, `module`) is a non-reassignable `const` binding. (A protocol's
/// member *dispatcher* cells are not globals — they are added at protocol load,
/// `machine/protocol.rs`.)
fn cell_kind_of(kind: crate::resolve::GlobalKind) -> CellKind {
    use crate::resolve::GlobalKind;
    match kind {
        GlobalKind::Let => CellKind::Let,
        GlobalKind::Parameter => CellKind::Parameter,
        GlobalKind::Const
        | GlobalKind::Proc
        | GlobalKind::Fn
        | GlobalKind::Record
        | GlobalKind::Protocol
        | GlobalKind::Module => CellKind::Const,
    }
}
