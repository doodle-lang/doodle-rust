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
use crate::diag::{Diagnostic, DiagnosticCode};
use crate::heap::CellKind;
use crate::span::ModuleId;

impl Instance {
    /// Creates a `Ready` instance for `module` under `config` (engine spec E§3.1).
    /// Validates the config first (S-41): a requested Unicode version that is not the
    /// engine's build-pinned one is rejected — the engine supports exactly its pinned
    /// version, so a host or replay can assert the expected version at create time
    /// rather than diverge silently on grapheme/normalization behavior (E§11). `None`
    /// uses the pinned version.
    pub fn create(module: ResolvedModule, config: Config) -> Result<Self, ConfigError> {
        Self::create_with_module_path(module, config, Self::DEFAULT_MODULE_PATH)
    }

    /// The default canonical id for the entry module (E§3.2) when a host loads through an
    /// entry point that does not name one. A host that shows filenames (the IDE) passes its
    /// own path via [`create_with_module_path`](Self::create_with_module_path) so breakpoints
    /// (E§8.6) and load diagnostics (S-63) address the entry module by that name.
    pub const DEFAULT_MODULE_PATH: &str = "main";

    /// Like [`create`](Self::create) but names the entry module's canonical id (E§3.2): the
    /// host-owned identity the entry module is addressed by — for breakpoints (E§8.6), the
    /// load-diagnostics schema (S-63), and singleton dedupe against a self-import (L§11.3).
    pub fn create_with_module_path(
        module: ResolvedModule,
        config: Config,
        module_path: &str,
    ) -> Result<Self, ConfigError> {
        if let Some(requested) = config.unicode_version
            && requested != UNICODE_VERSION
        {
            return Err(ConfigError::UnsupportedUnicodeVersion {
                requested,
                pinned: UNICODE_VERSION,
            });
        }
        Ok(Self::load_full(
            module,
            config.limits,
            intrinsic::Registry::new(),
            module_path,
        ))
    }

    /// The Unicode/UCD version this engine is pinned to (L§4.4; the config's
    /// target-version field is validated against it, S-41).
    pub fn unicode_version() -> UnicodeVersion {
        UNICODE_VERSION
    }

    /// Loads a resolved module into a fresh `Ready` instance with the
    /// [`Default`](Limits) resource limits and no intrinsics. See [`load_full`](Self::load_full).
    pub fn load(module: ResolvedModule) -> Self {
        Self::load_full(
            module,
            Limits::default(),
            intrinsic::Registry::new(),
            Self::DEFAULT_MODULE_PATH,
        )
    }

    /// Loads a resolved module under the given resource limits (E§10.2), no
    /// intrinsics. See [`load_full`](Self::load_full).
    pub fn load_with_limits(module: ResolvedModule, limits: Limits) -> Self {
        Self::load_full(
            module,
            limits,
            intrinsic::Registry::new(),
            Self::DEFAULT_MODULE_PATH,
        )
    }

    /// Loads a resolved module with host-registered intrinsic foreign functions
    /// (E§5.1, S-43) and the [`Default`](Limits) limits. `registry` holds the
    /// intrinsics the host registered **before** this load (E§5.5); they seed as
    /// read-only global names after the program's declarations and the built-in
    /// type values, so a program's own declaration of the same name shadows one.
    pub fn load_with_intrinsics(module: ResolvedModule, registry: intrinsic::Registry) -> Self {
        Self::load_full(
            module,
            Limits::default(),
            registry,
            Self::DEFAULT_MODULE_PATH,
        )
    }

    /// Loads a resolved module with host-registered intrinsics (S-43) under the given
    /// resource limits (E§10.2) — [`load_with_intrinsics`](Self::load_with_intrinsics)
    /// and [`load_with_limits`](Self::load_with_limits) combined.
    pub fn load_with_intrinsics_and_limits(
        module: ResolvedModule,
        limits: Limits,
        registry: intrinsic::Registry,
    ) -> Self {
        Self::load_full(module, limits, registry, Self::DEFAULT_MODULE_PATH)
    }

    /// Loads a resolved module into a fresh `Ready` instance (machine-design §18)
    /// under the given resource limits (E§10.2) and intrinsic registry (S-43). Each
    /// module-level name gets an **uninitialized** binding cell (its `let`/`const`
    /// fills it when it executes; a read before then is a use-before-defined error).
    /// The module top level becomes an ordinary, drivable `ModuleTopLevel` frame
    /// whose pending work sequences its statements.
    fn load_full(
        module: ResolvedModule,
        limits: Limits,
        intrinsics: intrinsic::Registry,
        module_path: &str,
    ) -> Self {
        debug_assert!(
            matches!(
                module.ast.node(module.root),
                crate::ast::Node::Module { .. }
            ),
            "load: a resolved module's root must be the `Module` node"
        );
        let mut heap = Heap::new();
        let canonical_id = module.canonical_id;
        // The registered intrinsics (flat, seeded into the main module, S-43) and native
        // modules (pre-loaded as their own modules, E§5.5) — a native module's function
        // members join the flat intrinsics in one id space `CallableTarget::Intrinsic`
        // indexes, appended after the flat ones so the flat ids stay stable.
        let (mut all_intrinsics, native_modules) = intrinsics.into_parts();
        // The flat intrinsics are the prelude's own functions (bound in the prelude module
        // below); native modules' function members append after them in the one
        // `CallableTarget::Intrinsic` id space, so the flat ids stay stable.
        let prelude_count = all_intrinsics.len();
        // The built-in `Error` record type (L§12.1, S-58): the value record
        // `Error(kind, message, details)` the engine raises and Doodle code can
        // construct/inspect. One `Error` type is shared by every module in the instance,
        // so it is created once here and remembered on the machine (an engine raise
        // materializes one without a namespace scan) and bound in the prelude module below.
        let error_type = alloc_error_type(&mut heap);
        // The instance-wide protocol registry, pre-populated with the engine's well-known
        // `Stringable`/`Hashable` (L§15, D-M5-1) so interpolation and dict keys can dispatch
        // them; the prelude module binds their names.
        let mut protocols = super::protocol::Registry::default();
        protocols.register_wellknown();
        // A source module's namespace holds only its own globals; the prelude names resolve
        // through its implicit prelude wildcard (S-60), added below.
        let namespace = seed_namespace(&module, &mut heap);
        // Every loaded module's namespace cells are permanent GC roots; the main module's
        // come first, then each pre-loaded native module's (below).
        let mut module_root_cells: Vec<CellIdx> = namespace.iter().map(|(_, cell)| *cell).collect();
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
        // The main module is `ModuleId(0)`; native modules take `1..=k` in registration
        // order (replay-identity input, E§11), pre-loaded and marked `Loaded`.
        let mut modules = vec![super::LoadedModule {
            resolved,
            namespace,
            wildcards: Vec::new(),
        }];
        let mut load = ModuleLoad::new(module_path);
        for module in native_modules {
            let id = ModuleId(modules.len() as u32);
            let path: Box<str> = module.name.clone();
            let built =
                super::native::build_native_module(module, id, &mut heap, &mut all_intrinsics);
            module_root_cells.extend(&built.cells);
            load.begin(std::slice::from_ref(&path), &path, id);
            load.set_state(id, super::modload::LoadState::Loaded);
            modules.push(super::LoadedModule {
                resolved: Arc::new(built.resolved),
                namespace: built.namespace,
                wildcards: Vec::new(),
            });
        }
        // The prelude module (L§11.2, S-60): built after the native modules so it takes the
        // last id, holding the type values, `Error`, well-known protocols, and the flat
        // intrinsics (`all_intrinsics[..prelude_count]`). Registered `Loaded` under the path
        // `prelude` so it is named in an ambiguity message (and importable to disambiguate).
        let prelude = ModuleId(modules.len() as u32);
        let built = build_prelude(
            prelude,
            &mut heap,
            error_type,
            &all_intrinsics[..prelude_count],
            &protocols,
        );
        module_root_cells.extend(&built.cells);
        let prelude_path: Box<str> = "prelude".into();
        load.begin(std::slice::from_ref(&prelude_path), &prelude_path, prelude);
        load.set_state(prelude, super::modload::LoadState::Loaded);
        modules.push(super::LoadedModule {
            resolved: Arc::new(built.resolved),
            namespace: built.namespace,
            wildcards: Vec::new(),
        });
        // Every source module implicitly wildcard-imports the prelude. The main module is the
        // only source module at construction; each `import`ed source module gets it at load
        // (`machine/import.rs`). Native/prelude modules never resolve free names, so they get
        // no wildcard.
        modules[0].wildcards.push(prelude);
        // The entry module's load-diagnostics contribution (S-63): its prelude-shadowing
        // warnings (a global that hides a prelude name, L§5.1), ordered by span. The entry
        // module's *lexical* front-end diagnostics were produced by the host's own resolve
        // (this constructor takes an already-resolved module); the host holds them and the
        // facade seeds them into this record when it wires the display surface (M6.9). Each
        // imported module's diagnostics append here as it loads (`machine/import.rs`).
        let mut load_diagnostics = prelude_shadowing(
            modules[0].resolved.as_ref(),
            &modules[prelude.0 as usize].namespace,
            canonical_id,
        );
        load_diagnostics.sort_by_key(|d| d.span.map_or(0, |s| s.start));
        Instance {
            modules,
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
                intrinsics: intrinsic::Registry::from_intrinsics(all_intrinsics),
                output: Vec::new(),
                pending: None,
                load,
                protocols,
                prelude,
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
                host_pause: Arc::new(AtomicBool::new(false)),
                breakpoints: super::breakpoint::Breakpoints::new(),
                safe_point_stmt: None,
                raise_trap_enabled: false,
                limits,
                load_diagnostics,
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

/// Seeds a module's namespace with its **own** module-level globals (machine-design §6):
/// one uninitialized cell per declaration, filled by its `let`/`const`/`to`/`fn` in
/// execution order (the temporal dead zone). The prelude names (type values, `Error`, the
/// well-known protocols, host intrinsics) are **not** here — they live in the shared prelude
/// module and resolve through the module's implicit prelude wildcard (L§11.2, S-60), so a
/// user global of the same name shadows the prelude (own declaration beats any wildcard).
pub(super) fn seed_namespace(module: &ResolvedModule, heap: &mut Heap) -> Vec<(Box<str>, CellIdx)> {
    module
        .globals
        .iter()
        .map(|g| (g.name.clone(), heap.alloc_cell(cell_kind_of(g.kind), None)))
        .collect()
}

/// Builds the engine **prelude** module (L§11.2, S-43/S-60): one shared synthetic module
/// holding the built-in type values, the `Error` type, the well-known `Stringable`/`Hashable`
/// protocols + `to_string` dispatcher, and the host's flat intrinsics — every prelude name as
/// a read-only `const`. Every source module implicitly wildcard-imports it, so these resolve
/// as ordinary wildcard names. `intrinsics` is the flat prelude slice (its index is each one's
/// `CallableTarget::Intrinsic` id); `id` is the module's own id (the `CalObj.module` of its
/// intrinsics). All members are public (`exports: None`), so the wildcard exposes them all.
fn build_prelude(
    id: ModuleId,
    heap: &mut Heap,
    error_type: TypeIdx,
    intrinsics: &[intrinsic::Intrinsic],
    protocols: &super::protocol::Registry,
) -> super::native::BuiltNativeModule {
    let mut namespace: Vec<(Box<str>, CellIdx)> = Vec::new();
    for &(name, builtin) in types::BUILTINS {
        let kind = crate::machine::TypeKind::Builtin(builtin);
        let ty = Value::Type(heap.alloc_type(crate::heap::TypeObj { kind }));
        namespace.push((name.into(), heap.alloc_cell(CellKind::Const, Some(ty))));
    }
    namespace.push((
        "Error".into(),
        heap.alloc_cell(CellKind::Const, Some(Value::Type(error_type))),
    ));
    super::protocol::seed_wellknown(&mut namespace, heap, id, protocols);
    for (i, intrinsic) in intrinsics.iter().enumerate() {
        let cal = heap.alloc_callable(crate::heap::CalObj {
            module: id,
            target: crate::heap::CallableTarget::Intrinsic(i as u32),
            captures: Vec::new(),
        });
        namespace.push((
            intrinsic.name.clone(),
            heap.alloc_cell(CellKind::Const, Some(Value::Callable(cal))),
        ));
    }
    super::native::synthetic_module(id, namespace)
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

/// The **prelude-shadowing** warnings for one module (L§5.1, D-M5-6/S-63): a module-level
/// declaration whose name also names a **prelude** export hides that built-in (the module's
/// own declaration beats the implicit prelude wildcard, S-60), which is allowed but worth
/// flagging. `prelude_ns` is the prelude module's namespace (its export names); the lookup
/// is a small linear scan (no hashing — determinism is by construction, and the sets are
/// tiny). Returned unordered; the caller sorts each module's contribution by span (S-63).
/// User-*wildcard* shadowing is not covered here — a wildcard's export names are known only
/// when it loads, so that stays the linter's/import-time job.
pub(super) fn prelude_shadowing(
    module: &ResolvedModule,
    prelude_ns: &[(Box<str>, CellIdx)],
    id: ModuleId,
) -> Vec<Diagnostic> {
    module
        .globals
        .iter()
        .filter(|g| {
            prelude_ns
                .iter()
                .any(|(name, _)| name.as_ref() == g.name.as_ref())
        })
        .map(|g| {
            Diagnostic::warning(
                DiagnosticCode::Shadowing,
                id,
                module.ast.span(g.decl),
                format!(
                    "this `{name}` hides the built-in `{name}` from the prelude — that's \
                     allowed, but check you meant to",
                    name = g.name
                ),
            )
        })
        .collect()
}
