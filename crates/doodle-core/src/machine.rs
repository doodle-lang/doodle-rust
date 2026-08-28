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
mod assign;
mod block;
mod boundary;
mod call;
mod cancel;
mod compare;
mod cont;
mod control;
mod dict;
mod dynamic;
mod error;
mod eval;
mod exception;
mod foreign;
mod frame;
mod gc;
mod handle;
mod hash;
mod import;
mod intrinsic;
mod lifecycle;
mod limits;
mod load;
mod local;
mod modload;
mod native;
mod observe;
mod ops;
mod protect;
mod protocol;
mod record;
mod ring;
mod step;
mod stmt;
mod stringify;
mod strop;
mod types;
mod unwind;
mod value;

pub use crate::heap::Finalizer;
pub use boundary::{Kind, ValueError};
pub use cancel::CancelToken;
pub(crate) use error::Halt;
pub use error::{Exception, ExceptionKind, Trace, TraceFrame};
pub use handle::{Handle, HandleError};
pub use intrinsic::{
    HostError, Intrinsic, IntrinsicCtx, Registry, clear_canvas as clear_canvas_intrinsic,
    cos as cos_intrinsic, decode as decode_intrinsic, draw_line as draw_line_intrinsic,
    each as each_intrinsic, encode as encode_intrinsic, length as length_intrinsic,
    print as print_intrinsic, read_line as read_line_intrinsic, set_turtle as set_turtle_intrinsic,
    sin as sin_intrinsic,
};
pub use native::{ConstValue, NativeMember, NativeModule};
pub use observe::{FrameObservation, Position};
pub(crate) use types::{BuiltinType, ProtocolType, RecordType, TypeKind};
pub use value::{
    BigIntIdx, BytesIdx, CalIdx, CellIdx, DictIdx, FrnIdx, ListIdx, RecIdx, StrIdx, TypeIdx, Value,
};

use crate::drive::{Config, ConfigError, Directive, EngineFault, Limits};
use crate::heap::Heap;
use crate::resolve::ResolvedModule;
use crate::span::ModuleId;
use crate::unicode::{UNICODE_VERSION, UnicodeVersion};
use cont::Cont;
use frame::Frame;
use handle::HandleTable;
use limits::FusedCounter;
use modload::{ModuleLoad, Suspension};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
    /// The parked suspension while the instance is `Suspended` (E§7.5, §6, MD §14);
    /// `None` in normal execution. A suspending capability call's `apply`
    /// (`intrinsic/mod.rs`) parks a [`Suspension::Capability`], an `import` of an
    /// unloaded module (`step`) parks a [`Suspension::Import`]; `resolve` /
    /// `resolve_import` (`drive.rs`) consume it. A parked capability's `args` are GC
    /// roots while parked.
    pending: Option<Suspension>,
    /// Module-load bookkeeping (E§6, L§11.3): per-module load state + the path/canonical
    /// singleton caches. Lives here (not on the `Instance`) so the frame-lifecycle sites
    /// that flip a module `loading → loaded/failed` reach it through `&mut machine`. Holds
    /// no heap references, so it is not a GC root.
    load: ModuleLoad,
    /// The protocol registry (L§10, plan AD5): interned member names, protocol definitions,
    /// and `implement` blocks, populated as `protocol`/`implement` declarations load. Its
    /// member-default and impl-method callables are GC roots (`machine/gc.rs`) — no
    /// namespace cell references them.
    protocols: protocol::Registry,
    /// The binding cells of **every loaded module's** namespace (machine-design §6/§15,
    /// AD5): each module's globals live for the instance's life (a module is a singleton,
    /// never unloaded in v0.1), so their cells are **permanent GC roots**. Every module
    /// appends its namespace cells here at load — the main module at construction, each
    /// imported module when it loads — so a collection during *any* module's step roots
    /// *all* modules' globals, not just the executing one's (a sub-module load must not
    /// sweep the importer's globals).
    module_root_cells: Vec<CellIdx>,
    /// The directive the current drive runs under, remembered across a suspend so
    /// `resolve` resumes under the same directive (E§7.3).
    directive: Directive,
    /// A fault parked during a transition because the Raise-typed dispatch/`apply` chain
    /// cannot carry an `EngineFault`: a fault raised **inside a reentrant nested drive** (a
    /// limit tripped while a native block-consumer ran its block, or the deferred S-15
    /// nested-suspend), or a **resource limit hit mid-transition** (a string `*` whose
    /// result would exceed the heap limit). `step` surfaces it as its `Err(Halt::Fault)`
    /// after the transition returns. `None` in normal execution.
    pending_fault: Option<EngineFault>,
    /// The bound arguments of **in-flight synchronous foreign calls** (MD §15): while a
    /// callback runs — and, for a native block-consumer, while its reentrant nested drive
    /// runs and may collect — its arguments are held only on the host's Rust stack, so
    /// they are rooted here (a flat stack; a call pushes on entry and pops on return) or
    /// a collection during the nested drive would free them.
    foreign_roots: Vec<Value>,
    /// The **dynamic-binding save stack** (machine-design §13): while a `with p = v`
    /// body runs, its `(cell, old_value)` is pushed here and a
    /// [`WithRestore`](cont::Cont::WithRestore) cont is pushed on the frame; restoring
    /// (on normal completion or any unwind, §12) pops back to the cont's mark, writing
    /// each saved value back into its cell. Its saved values are GC roots. The `with`/
    /// `parameter` producers are M4.6; the save stack and restore mechanism are M4.5a.
    dyn_stack: Vec<(CellIdx, Value)>,
    /// The **handling stack** (L§12.2): the exceptions whose rescue bodies are currently
    /// running, innermost last, as `(value, trace)`. A bare `raise` re-raises the top
    /// entry with its original trace; the entry is pushed on catch and popped (a
    /// [`PopHandler`](cont::Cont::PopHandler)) as the rescue body finishes. Its values
    /// are GC roots.
    handling: Vec<(Value, error::Trace)>,
    /// The built-in `Error` record type (L§12.1, S-58): the type index of the value
    /// record `Error(kind, message, details)` the engine raises. Seeded at load (also
    /// bound as the `Error` global), remembered here so an engine raise can materialize
    /// an instance without a namespace scan.
    error_type: TypeIdx,
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
    /// The host's cancellation flag (E§10.1): the "stop button", shared with the
    /// [`CancelToken`]s the host holds. Set from anywhere (another thread, a signal
    /// handler) and **polled at each safe point** ([`poll_cancel`](Self::poll_cancel));
    /// once set, the drive arms the cancel unwind (§12) and faults `Cancelled`.
    cancel: Arc<AtomicBool>,
}

/// A loaded module (L§11.3, E§6, AD5): its resolved AST and its module-scope
/// namespace (each module-level name bound to its binding cell). Modules are indexed
/// by [`ModuleId`]; the main module is `ModuleId(0)`. The `{loading, loaded, failed}`
/// load-state machine and multi-module *loading* join at M5.1 — at M5.0 an instance
/// holds the single main module, and this table (with the per-frame `ModuleId`) makes
/// the machine module-aware so a name read hits the executing frame's module.
struct LoadedModule {
    resolved: Arc<ResolvedModule>,
    /// The module namespace (machine-design §6/§18): a small ordered list scanned
    /// linearly by `find_cell`, so lookup is deterministic and hashing-free. Holds the
    /// module's own globals + prelude + its **explicit** imports (module values, `as`
    /// aliases, member aliases). Wildcard-imported names are *not* here — they resolve on
    /// use through `wildcards` (AD5).
    namespace: Vec<(Box<str>, CellIdx)>,
    /// The modules this one wildcard-imported (`import m.*`), in import order (AD5, S-13).
    /// A free name not found in `namespace` is looked up across these modules' exports on
    /// use: one match binds it (a live alias of the exporter's cell), two or more raise an
    /// ambiguity naming the sources. Kept deduplicated.
    wildcards: Vec<ModuleId>,
}

/// A running program: the machine state the host drives (engine spec E§3).
///
/// Owns the loaded-module table (each module's immutable resolved AST + namespace,
/// shareable with tooling, machine-design §2), the [`Heap`], the [`Machine`] state, and
/// the lifecycle [`InstanceState`]. The drive loop advances it via [`step`](Self::step),
/// which reads the executing frame's module (AD5). At M5.0 the table holds the single
/// main module; multi-module loading is M5.1.
pub struct Instance {
    /// The loaded modules, indexed by [`ModuleId`] (the main module is `ModuleId(0)`).
    modules: Vec<LoadedModule>,
    heap: Heap,
    machine: Machine,
    state: InstanceState,
}

impl Instance {
    /// The module the top frame is executing in — whose resolved AST and namespace the
    /// next transition reads (AD5). `ModuleId(0)` (the main module) when no frame is
    /// active (before the first / after the last transition).
    fn current_module(&self) -> ModuleId {
        self.machine.frames.last().map_or(ModuleId(0), |f| f.module)
    }

    /// The resolved AST of the module the top frame is executing in (AD5). At M5.0 the
    /// single main module; the observation surface's per-frame module resolution (a
    /// trace's frames may span modules) joins with cross-module calls at M5.1.
    fn current_resolved(&self) -> &ResolvedModule {
        &self.modules[self.current_module().0 as usize].resolved
    }
}

/// Releasing an instance's heap runs the finalizer of **every live foreign value**
/// exactly once (E§3.1/§4.5), whether the host calls [`destroy`](Instance::destroy)
/// or simply drops the instance — so a host resource behind a foreign value is never
/// leaked. A foreign value already reclaimed by a GC sweep had its finalizer run then
/// (and taken), so it is not touched again here.
impl Drop for Instance {
    fn drop(&mut self) {
        self.heap.finalize_all();
    }
}

impl Instance {
    /// Destroys the instance (E§3.1): releases its heap, running the finalizer of every
    /// still-live foreign value (the work happens in [`Drop`], so a plain drop finalizes
    /// too). After this, all handles into the instance are invalid.
    pub fn destroy(self) {
        // `self` drops at the end of this scope, running `Drop` (the finalizers). Named
        // explicitly so the E§3.1 `destroy` surface exists on the public API.
    }

    /// The current lifecycle state (E§3.3).
    pub fn state(&self) -> InstanceState {
        self.state
    }

    /// A [`CancelToken`] for this instance (E§10.1): the host's stop button. The token
    /// is cloneable and thread-safe, so a host may hold it (or a clone) elsewhere — e.g.
    /// on another thread — and request cancellation while a drive is running. All tokens
    /// for one instance share its cancel flag.
    pub fn cancel_token(&self) -> CancelToken {
        CancelToken::new(Arc::clone(&self.machine.cancel))
    }

    /// Whether host cancellation has been requested (E§10.1) — a plain read of the cancel
    /// flag, distinct from the safe-point poll ([`Machine::poll_cancel`]) that *arms* the
    /// unwind. Lets `resolve` (E§7.5) reap a cancellation that arrived while the instance
    /// was suspended, so a host raise racing the stop button does not escape it (S-23).
    pub(crate) fn cancel_requested(&self) -> bool {
        self.machine.cancel.load(Ordering::Relaxed)
    }

    /// Discards a parked capability request and arms the cancel unwind (E§10.1, S-23):
    /// resuming the drive then tears the stack down to `Faulted(Cancelled)` **without**
    /// running the parked call's continuation, so a host resolution that lost to a pending
    /// cancellation has no program-visible effect. Only valid while suspended — a request is
    /// parked and the frame stack is non-empty (a suspend never empties it), which the
    /// caller establishes by checking [`cancel_requested`](Self::cancel_requested) at a
    /// `Suspended` instance.
    pub(crate) fn discard_pending_and_cancel(&mut self) {
        self.machine.pending = None;
        debug_assert!(
            self.machine.unwind.is_none() && !self.machine.frames.is_empty(),
            "cancel-reap requires a parked suspension with an intact stack"
        );
        self.machine.unwind = Some(unwind::Unwind::Cancel);
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
        // Every loaded module's namespace cells are permanent roots on the machine
        // (`module_root_cells`), so a collection roots all modules' globals regardless of
        // which module is executing (AD5).
        gc::collect(&mut self.heap, &self.machine);
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

    /// The described `kind` of the exception each `failed` module retains (S-8) — for
    /// asserting a failed load retained the right value (the re-raise itself is latent
    /// until a reload path exists, M9b).
    #[cfg(test)]
    pub(crate) fn failed_module_error_kinds(&self) -> Vec<String> {
        self.machine
            .load
            .failed_values()
            .map(|v| exception::describe(&self.heap, self.machine.error_type, v).0)
            .collect()
    }

    /// Performs one machine transition (machine-design §8). Precondition:
    /// `!self.is_halted()`. `Ok(Some(depth))` means the transition crossed a
    /// **statement-level safe point** (E§7.4) at that frame depth — where the drive
    /// loop may pause a `Step*` directive; `Ok(None)` means no safe point this
    /// transition. `Err` stopped the drive — an uncaught raise or an engine fault
    /// (the drive loop maps each to its [`Outcome`](crate::drive::Outcome)).
    pub(crate) fn step(&mut self) -> Result<Option<usize>, Halt> {
        // The transition reads the top frame's module's resolved AST (AD5). The whole
        // module table is threaded so the step can reach **another** module's resolved (a
        // cross-module call) and **mutate** the importer's namespace (an import binding).
        // Clone the current module's `resolved` Arc first (cheap — one atomic incref) so
        // the table is free to borrow mutably alongside.
        let cur = self.current_module();
        let resolved = Arc::clone(&self.modules[cur.0 as usize].resolved);
        step::step(
            &resolved,
            &mut self.modules,
            &mut self.heap,
            &mut self.machine,
        )
    }
}

#[cfg(test)]
mod tests;
