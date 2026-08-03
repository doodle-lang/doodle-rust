//! The resumable machine: the value representation and the instance that holds
//! and drives execution state.
//!
//! [`Value`] is the `Copy` value representation (machine-design §3). [`Instance`]
//! is a running program (engine spec E§3): it owns the resolved module, the
//! [`Heap`], and the [`Machine`] state (the frame stack + result register), and
//! the drive loop ([`crate::drive`]) advances it one [`step`](Instance::step) at a
//! time.
//!
//! **Scope.** The CESK machine over the demo subset: frames, a continuation stack,
//! `step`, operators, binding, control flow, calls, PTC, safe-point resource limits
//! (the fused counter, `machine/limits.rs`), the mark-sweep GC (`machine/gc.rs`), and
//! host handles + the config surface (`machine/handle.rs`). The dynamic-parameter and
//! drive stacks join with the features that need them (`plan/plan-m2a.md`).

mod arith;
mod block;
mod boundary;
mod call;
mod compare;
mod cont;
mod control;
mod error;
mod frame;
mod gc;
mod handle;
mod intrinsic;
mod lifecycle;
mod limits;
mod local;
mod ring;
mod step;
mod types;
mod unwind;
mod value;

pub use boundary::{Kind, ValueError};
pub(crate) use error::Halt;
pub use error::{Exception, ExceptionKind, Trace};
pub use handle::{Handle, HandleError};
pub use intrinsic::{
    HostError, Intrinsic, IntrinsicCtx, Registry, each as each_intrinsic, print as print_intrinsic,
    read_line as read_line_intrinsic,
};
pub(crate) use types::BuiltinType;
pub use value::{
    BigIntIdx, BytesIdx, CalIdx, CellIdx, DictIdx, FrnIdx, ListIdx, RecIdx, StrIdx, TypeIdx, Value,
};

use crate::drive::{Config, ConfigError, Directive, EngineFault, Limits};
use crate::heap::Heap;
use crate::resolve::ResolvedModule;
use crate::unicode::{UNICODE_VERSION, UnicodeVersion};
use cont::Cont;
use frame::Frame;
use handle::HandleTable;
use limits::FusedCounter;
use std::sync::Arc;

/// The maximum **reentrant-drive nesting depth** (MD §14). Each level runs a nested
/// drive on the host's Rust stack (~10 KB/level), so this caps native recursion well
/// below the smallest realistic host stack (~1 MiB): a program that recurses through a
/// native block-consumer faults with `StackDepth` here rather than overflowing the Rust
/// stack (which would abort the host process). A **provisional** flat bound — a
/// stack-size-aware or host-configured reentrancy limit is future work (M3/M7, when the
/// wasm/C-ABI host stack sizes are known); a genuinely-deep native `each`/`map` nest of
/// this magnitude is pathological, so no real program trips it.
const MAX_REENTRY_DEPTH: u32 = 64;

/// The lifecycle state of an [`Instance`] (engine spec E§3.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InstanceState {
    /// Loaded, not yet started, or between top-level statements.
    Ready,
    /// Inside a drive call.
    Running,
    /// Awaiting a capability resolution (E§7.5).
    Suspended,
    /// Stopped at a safe point for observation (E§7.4).
    Paused,
    /// Finished (E§7.2 `Completed`).
    Completed,
    /// An uncaught Doodle exception reached the outermost drive boundary (E§3.3/§9).
    /// Terminal and distinct from `Faulted`, so `state()` alone tells a program's own
    /// error from an engine fault; the exception + trace stay observable post-mortem.
    Raised,
    /// Stopped by a limit, cancellation, or internal fault (E§9, §10).
    Faulted,
}

impl InstanceState {
    /// Whether this is a terminal state — the driven unit is finished and the
    /// instance is not re-drivable (E§3.3; REPL re-drive is S-33/M9b).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            InstanceState::Completed | InstanceState::Raised | InstanceState::Faulted
        )
    }
}

/// The core execution state (machine-design §8): the walkable frame stack and the
/// result register. The additional state pinned in §8 — ring buffer, fuel,
/// in-flight unwind, dynamic-parameter stack, drive stack — is added in the
/// chunks that first need it.
pub(crate) struct Machine {
    /// The frame stack (E§8.2); top = innermost active body. Empty once halted.
    frames: Vec<Frame>,
    /// The result register (L§6.11): `None` = Void.
    reg: Option<Value>,
    /// Monotonic frame-identity counter (machine-design §8): stamped into each
    /// pushed frame's `serial`, so a frame activation is distinguishable from a
    /// later reuse of the same stack slot (integrity for static links / consumers).
    frame_serial: u64,
    /// An in-flight non-local transfer (machine-design §12): while `Some`, `step`
    /// unwinds toward the exit's resolver-annotated target instead of running
    /// continuations normally. `None` in normal execution. A GC root once it can
    /// carry an exception value (M4).
    unwind: Option<unwind::Unwind>,
    /// Bounded history of frames elided by tail-call reuse (E§8.3, §11).
    ring: ring::RingBuffer,
    /// The configured resource limits (E§10.2): the heap and non-tail stack-depth
    /// thresholds the safe points check against (`machine/limits.rs`).
    limits: Limits,
    /// The fused safe-point counter (machine-design §9): decremented once per
    /// statement-level safe point; exhaustion is the step budget.
    fuel: FusedCounter,
    /// The accounted-bytes level at which the next collection triggers (machine-design
    /// §15). Starts at [`limits::GC_MIN_BYTES`] and is re-armed after each collect to
    /// the surviving set's next doubling, so GC stays cheap when little is live.
    gc_threshold: u64,
    /// The host handle table (machine-design §16): each live handle's value is a GC
    /// root, so a value the host retains survives collection. Lives here (rather than
    /// on the `Instance`) so `gc::collect` roots it alongside the other machine state.
    handles: HandleTable,
    /// The host-registered intrinsic foreign functions (E§5.1, S-43), frozen at load.
    /// Consulted when a call's callee is an intrinsic (`call.rs` → `intrinsic::apply`).
    /// Its inline defaults hold no heap references, so it is not a GC root.
    intrinsics: intrinsic::Registry,
    /// The captured output sink (the instance's "standard output"): `print` and other
    /// output intrinsics append here, and the host reads it via [`Instance::output`].
    /// Bytes, not heap values — not a GC root.
    output: Vec<u8>,
    /// The parked capability request while the instance is `Suspended` (E§7.5, MD §14);
    /// `None` in normal execution. Set by a capability call's `apply` (`intrinsic.rs`)
    /// and consumed by `resolve` (`drive.rs`). Its `args` are GC roots while parked.
    pending: Option<intrinsic::PendingRequest>,
    /// The directive the current drive runs under, remembered across a suspend so
    /// `resolve` resumes under the same directive (E§7.3).
    directive: Directive,
    /// A fault raised **inside a reentrant nested drive** (a limit tripped while a
    /// native block-consumer ran its block, or the deferred S-15 nested-suspend), parked
    /// here because the Raise-typed intrinsic `apply` chain cannot carry an
    /// `EngineFault`. `step` surfaces it as its `Err(Halt::Fault)` after the outer
    /// transition returns. `None` in normal execution.
    reentry_fault: Option<EngineFault>,
    /// The bound arguments of **in-flight synchronous foreign calls** (MD §15): while a
    /// callback runs — and, for a native block-consumer, while its reentrant nested drive
    /// runs and may collect — its arguments are held only on the host's Rust stack, so
    /// they are rooted here (a flat stack; a call pushes on entry and pops on return) or
    /// a collection during the nested drive would free them.
    foreign_roots: Vec<Value>,
    /// The current **reentrant-drive nesting depth** (MD §14): each reentrant block
    /// invocation (`intrinsic::invoke_block`) runs a nested drive on the **host's Rust
    /// stack**, so a program that recurses through a native block-consumer grows the Rust
    /// stack, not just `frames`. This bounds that recursion below the Rust stack limit so
    /// it faults with `StackDepth` rather than overflowing the native stack (a host abort).
    reentry_depth: u32,
    /// GC-stress test knob (machine-design §15): when set, `safe_point` collects at
    /// **every** safe point — including those inside a reentrant nested drive — so a test
    /// can force a collection at the exact window a value is rooted only transiently
    /// (e.g. a native `each`'s list via `foreign_roots`). Always `false` in production.
    gc_every_safe_point: bool,
}

impl Machine {
    /// The next frame serial (post-increment): a fresh, monotonic frame identity.
    pub(crate) fn next_frame_serial(&mut self) -> u64 {
        let serial = self.frame_serial;
        self.frame_serial += 1;
        serial
    }

    /// Takes any fault parked by a reentrant nested drive (`step` surfaces it, MD §14).
    pub(crate) fn take_reentry_fault(&mut self) -> Option<EngineFault> {
        self.reentry_fault.take()
    }

    /// Roots the arguments of an entering synchronous foreign call (MD §15), returning
    /// the prior stack length to [`pop_foreign_roots`](Self::pop_foreign_roots) on return.
    pub(crate) fn push_foreign_roots(&mut self, values: &[Value]) -> usize {
        let base = self.foreign_roots.len();
        self.foreign_roots.extend_from_slice(values);
        base
    }

    /// Un-roots a returning foreign call's arguments (truncating to `base`).
    pub(crate) fn pop_foreign_roots(&mut self, base: usize) {
        self.foreign_roots.truncate(base);
    }

    /// Whether entering another reentrant drive would exceed the native-stack nesting
    /// bound (MD §14) — a program recursing through a native block-consumer must fault,
    /// not overflow the Rust stack.
    pub(crate) fn reentry_would_overflow(&self) -> bool {
        self.reentry_depth >= MAX_REENTRY_DEPTH
    }

    /// Enters a reentrant drive (increments the nesting depth); pair with [`exit_reentry`].
    pub(crate) fn enter_reentry(&mut self) {
        self.reentry_depth += 1;
    }

    /// Leaves a reentrant drive (decrements the nesting depth).
    pub(crate) fn exit_reentry(&mut self) {
        self.reentry_depth -= 1;
    }

    /// Records a tail-elided frame in the ring (machine-design §11).
    pub(crate) fn record_elided(&mut self, callable: CalIdx, consuming_serial: u64) {
        self.ring.record(ring::ElidedFrame {
            callable,
            consuming_serial,
        });
    }
}

/// A running program: the machine state the host drives (engine spec E§3).
///
/// Owns the immutable resolved module (shareable with tooling, machine-design §2),
/// the [`Heap`], the [`Machine`] state, and the lifecycle [`InstanceState`]. The
/// drive loop advances it via [`step`](Self::step); the module table for multiple
/// modules is M5, so an instance holds a single module at M1/M2a.
pub struct Instance {
    resolved: Arc<ResolvedModule>,
    heap: Heap,
    machine: Machine,
    /// The module namespace (machine-design §6/§18): each module-level name bound
    /// to its binding cell. A small ordered list (single module at M1/M2a);
    /// scanned linearly, so lookup is deterministic and hashing-free.
    namespace: Vec<(Box<str>, CellIdx)>,
    state: InstanceState,
}

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
        // Namespace order (S-43): module globals first (their `let`/`const`/`to`/`fn`
        // fill their cells in execution order — the temporal dead zone, M2a.4a), then
        // the built-in type-value prelude, then the host intrinsics — each later group
        // appended after, so an earlier binding of the same name wins the linear
        // `find_cell` scan (control.rs): a user global shadows a type value or an
        // intrinsic, matching the M5 prelude star-import this seeding will become.
        let mut namespace: Vec<(Box<str>, CellIdx)> = module
            .globals
            .iter()
            .map(|g| (g.name.clone(), heap.alloc_cell(None)))
            .collect();
        for &(name, builtin) in types::BUILTINS {
            let ty = Value::Type(heap.alloc_type(crate::heap::TypeObj { builtin }));
            namespace.push((name.into(), heap.alloc_cell(Some(ty))));
        }
        for (i, intrinsic) in intrinsics.iter().enumerate() {
            // Each intrinsic interns to one foreign `CalObj` (its registration index is
            // the `CallableTarget::Intrinsic` id) held by a read-only global cell.
            let cal = heap.alloc_callable(crate::heap::CalObj {
                module: canonical_id,
                target: crate::heap::CallableTarget::Intrinsic(i as u32),
                captures: Vec::new(),
            });
            namespace.push((
                intrinsic.name.clone(),
                heap.alloc_cell(Some(Value::Callable(cal))),
            ));
        }
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
            locals,
            Cont::Seq {
                block: root,
                next: 0,
            },
            0, // the module frame is frame serial 0; further frames count up
        );
        Instance {
            resolved,
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
                directive: Directive::RunToCompletion,
                reentry_fault: None,
                foreign_roots: Vec::new(),
                reentry_depth: 0,
                gc_every_safe_point: false,
                limits,
            },
            namespace,
            state: InstanceState::Ready,
        }
    }

    /// The current lifecycle state (E§3.3).
    pub fn state(&self) -> InstanceState {
        self.state
    }

    /// The result register: the last value produced, or `None` for Void
    /// (L§6.11). After a top-level drive completes this is `None` — a module runs
    /// for effect and yields Void.
    pub fn result(&self) -> Option<Value> {
        self.machine.reg
    }

    /// The instance's captured output — the bytes written by output intrinsics
    /// (`print`, E§5.2) in execution order. The host's view of "standard output";
    /// deterministic given deterministic execution (E§11).
    pub fn output(&self) -> &[u8] {
        &self.machine.output
    }

    /// Interns the current result value as a fresh host handle (engine spec E§4.2),
    /// keeping it reachable across collections and later drives; `None` when the
    /// result is Void. The host must [`release`](Self::release) it when done.
    pub fn retain_result(&mut self) -> Option<Handle> {
        self.machine
            .reg
            .map(|value| self.machine.handles.intern(value))
    }

    /// Adds a reference to `handle` (engine spec E§4.2). Errors on a stale handle
    /// (used after release) — the boundary generation check.
    pub fn retain(&mut self, handle: Handle) -> Result<Handle, HandleError> {
        self.machine.handles.retain(handle)
    }

    /// Releases a reference to `handle` (engine spec E§4.2); at zero references its
    /// value stops being a GC root. Errors on a stale handle.
    pub fn release(&mut self, handle: Handle) -> Result<(), HandleError> {
        self.machine.handles.release(handle)
    }

    /// The value a handle names, generation-checked (E§4.2). The public typed
    /// readers (`as_int`, `kind_of`, …) build on this at M2b; this crate-internal
    /// form is what the M2a handle tests read with.
    #[cfg(test)]
    pub(crate) fn resolve(&self, handle: Handle) -> Result<Value, HandleError> {
        self.machine.handles.resolve(handle)
    }

    /// Sets the lifecycle state (the drive loop drives the transitions).
    pub(crate) fn set_state(&mut self, state: InstanceState) {
        self.state = state;
    }

    /// Whether the machine has halted — no frames remain to run.
    pub(crate) fn is_halted(&self) -> bool {
        self.machine.frames.is_empty()
    }

    /// The current frame-stack depth. The drive loop reads it to anchor `Step*`
    /// directives by frame depth (E§8.5); a tail loop keeps it bounded (constant
    /// memory), which the PTC tests assert.
    pub(crate) fn frame_depth(&self) -> usize {
        self.machine.frames.len()
    }

    /// The top frame's tail-iteration counter (E§8.3), or `None` when halted.
    #[cfg(test)]
    pub(crate) fn top_frame_tail_count(&self) -> Option<u64> {
        self.machine.frames.last().map(|f| f.tail_count)
    }

    /// Forces a collection now (machine-design §15), independent of the trigger
    /// threshold — for tests that drive GC at chosen points to prove reachable state
    /// survives and garbage is reclaimed.
    #[cfg(test)]
    pub(crate) fn force_collect(&mut self) {
        gc::collect(&mut self.heap, &self.machine, &self.namespace);
    }

    /// Makes every safe point collect (machine-design §15) — including those inside a
    /// reentrant nested drive, which the between-`step` `force_collect` idiom cannot
    /// reach — so a GC-stress test can collect at a transiently-rooted window.
    #[cfg(test)]
    pub(crate) fn collect_at_every_safe_point(&mut self) {
        self.machine.gc_every_safe_point = true;
    }

    /// The number of live heap objects across all slabs (for GC tests).
    #[cfg(test)]
    pub(crate) fn live_object_count(&self) -> u32 {
        self.heap.live_objects()
    }

    /// Performs one machine transition (machine-design §8). Precondition:
    /// `!self.is_halted()`. `Ok(Some(depth))` means the transition crossed a
    /// **statement-level safe point** (E§7.4) at that frame depth — where the drive
    /// loop may pause a `Step*` directive; `Ok(None)` means no safe point this
    /// transition. `Err` stopped the drive — an uncaught raise or an engine fault
    /// (the drive loop maps each to its [`Outcome`](crate::drive::Outcome)).
    pub(crate) fn step(&mut self) -> Result<Option<usize>, Halt> {
        step::step(
            &self.resolved,
            &mut self.heap,
            &mut self.machine,
            &self.namespace,
        )
    }
}

#[cfg(test)]
mod tests;
