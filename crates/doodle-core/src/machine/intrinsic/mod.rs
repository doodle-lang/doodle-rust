//! Provisional pre-module intrinsic foreign functions (engine spec E§5.1/§5.2,
//! S-43): host-registered native callables (`print`, …) that exist as global names
//! before the module system and standard-library prelude land.
//!
//! **The provisional mechanism (S-43).** A host builds a [`Registry`] and registers
//! intrinsics into it **before** the instance's first load; the registry is then
//! moved into the instance, so registration cannot happen late. Each intrinsic is
//! seeded into the global namespace as a **read-only** cell appended *after* the
//! program's own module-level declarations (namespace order: globals → built-in
//! type values → intrinsics), so a program that declares its own `print` **shadows**
//! the host's — exactly the relationship the M5 prelude star-import will have, which
//! replaces this seeding with no program-observable change. Registration order is
//! part of replay identity (E§11). This whole mechanism is retired at M5.
//!
//! **Synchronous invocation (E§5.2).** A call to an intrinsic runs its callback
//! **inline** — it never becomes a callable frame. The engine binds the call-site
//! arguments (positional/keyword/defaults, L§8.3) exactly as for a Doodle call, then
//! invokes the callback with an [`IntrinsicCtx`] granting read access to the bound
//! arguments and the heap plus the instance's output sink. Reentrant callbacks
//! (invoking a Doodle callable / a block argument) and foreign block parameters are
//! M2b.5; foreign *heap-backed* defaults are S-42/M7 — a default here is an inline
//! value (the registry is built before the heap exists, so it can hold no heap ref).

use super::control::Namespace;
use super::error::Raise;
use super::frame::BlockDescriptor;
use super::{Halt, Machine, Value, block, step, unwind};
use crate::ast::NodeId;
use crate::drive::{EngineFault, LimitKind};
use crate::heap::Heap;
use crate::resolve::{BodyKind, ResolvedModule};
use crate::span::Span;

/// A parked capability request (engine spec E§7.5, MD §14): a call to a **suspending
/// capability** reached `apply`, so the machine stores the capability's identity + its
/// bound arguments here and the drive loop returns `Suspended`. **No state is torn
/// down** — the caller's continuation waits for the result the host supplies via
/// `resolve`. A GC root while parked (its `args` stay reachable, MD §15).
pub(crate) struct PendingRequest {
    /// The capability's identity — its registry index, stable across runs so
    /// resolutions record and replay (E§7.5/§11, S-43 registration order).
    pub capability: u32,
    /// The bound argument values, in parameter order (surfaced to the host as handles).
    pub args: Vec<Value>,
    /// The capability call site, for the `resolve(Raise)` trace.
    pub span: Span,
}

/// A parameter of an intrinsic foreign function (E§5.1). Its `default`, if present,
/// is an **inline** value (no heap reference — the registry is built before the heap
/// exists); a heap-backed foreign default is deferred to S-42/M7.
#[derive(Clone, Debug)]
pub(crate) struct ForeignParam {
    /// The parameter name (for keyword binding and diagnostics).
    pub name: Box<str>,
    /// An inline default value, or `None` for a required parameter.
    pub default: Option<Value>,
    /// Whether this is the trailing block parameter (bound reentrantly, M2b.5).
    pub is_block: bool,
}

/// The callback of a synchronous intrinsic (E§5.2): given the bound arguments and
/// the instance's output sink, it returns the call's result (`Some` for a `fn`,
/// `None` for a `to`'s Void) or raises. A plain `fn` pointer, not a closure — an
/// intrinsic's behavior is engine/host code with no captured Rust state, and this
/// keeps the callback [`Copy`] so reading it out of the registry does not borrow the
/// machine while the call mutates the output sink. Crate-internal (it names the
/// machine's `Raise`); hosts register engine-provided intrinsics (e.g. [`print`]),
/// not hand-written callbacks — the general host-callback FFI is the C ABI (M7).
pub(crate) type IntrinsicFn = fn(&mut IntrinsicCtx) -> Result<Option<Value>, Raise>;

/// How a foreign function is fulfilled (engine spec E§5.1): a **synchronous** callback
/// run inline (§5.2), or a **suspending capability** that yields to the host (§5.3).
#[derive(Clone, Debug)]
pub(crate) enum ForeignBody {
    /// Run the callback inline and continue (E§5.2).
    Sync(IntrinsicFn),
    /// Suspend: park a capability request and return `Suspended`; the host supplies the
    /// result via `resolve` (E§5.3/§7.5). The capability identity is the registry index.
    Capability,
}

/// A registered intrinsic foreign function (E§5.1): its name, kind (`to`/`fn`),
/// parameters, and body (a synchronous callback or a suspending capability). Opaque to
/// hosts — built via an engine-provided constructor like [`print`]/[`read_line`] and
/// handed to [`Registry::register`].
#[derive(Clone, Debug)]
pub struct Intrinsic {
    /// The global name it is seeded under.
    pub(crate) name: Box<str>,
    /// Procedure (`to`, yields Void) or function (`fn`, yields a value), honoring
    /// L§8.4 — using a `to` intrinsic's call in expression position raises (the Void
    /// runtime backstop).
    pub(crate) kind: BodyKind,
    /// Ordinary parameters, then at most one trailing block parameter (L§8.2).
    pub(crate) params: Vec<ForeignParam>,
    /// The synchronous callback or suspending-capability body.
    pub(crate) body: ForeignBody,
}

/// Why registering an intrinsic failed (a host-API error, E§5.5/§5.1 S-43). Loud by
/// design: a mis-set-up host is a bug, not a program error.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HostError {
    /// A second intrinsic was registered under a name already registered.
    DuplicateIntrinsic(Box<str>),
    /// An intrinsic name collides with a built-in type value (`Int`, `List`, …),
    /// which also seeds the global namespace (S-43 namespace order).
    CollidesWithBuiltin(Box<str>),
}

/// The set of intrinsic foreign functions a host registers **before** the first load
/// (E§5.5 S-43). Built by the host, then moved into the instance; its registration
/// order is replay-identity input (E§11).
#[derive(Clone, Debug, Default)]
pub struct Registry {
    intrinsics: Vec<Intrinsic>,
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Registry {
            intrinsics: Vec::new(),
        }
    }

    /// Registers `intrinsic`, or a [`HostError`] if its name duplicates a prior
    /// registration or a built-in type value (S-43). The registry is consumed into
    /// the instance at load, so there is no "after load" registration to reject here.
    pub fn register(&mut self, intrinsic: Intrinsic) -> Result<(), HostError> {
        if super::types::BUILTINS
            .iter()
            .any(|(n, _)| *n == &*intrinsic.name)
        {
            return Err(HostError::CollidesWithBuiltin(intrinsic.name.clone()));
        }
        if self.intrinsics.iter().any(|i| i.name == intrinsic.name) {
            return Err(HostError::DuplicateIntrinsic(intrinsic.name.clone()));
        }
        self.intrinsics.push(intrinsic);
        Ok(())
    }

    /// The registered intrinsics, in registration order (for namespace seeding).
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Intrinsic> {
        self.intrinsics.iter()
    }

    /// The intrinsic at registration index `id` (its `CallableTarget::Intrinsic` id).
    fn get(&self, id: u32) -> &Intrinsic {
        &self.intrinsics[id as usize]
    }

    /// The kind (`to`/`fn`) of the capability at index `id` — read on `resolve` to
    /// decide whether the resolution value becomes the call's result or a `to`'s Void.
    pub(crate) fn kind_of(&self, id: u32) -> BodyKind {
        self.intrinsics[id as usize].kind
    }
}

/// The activation of a synchronous intrinsic call (E§5.2, MD §14): the bound
/// arguments, the machine/heap state (to read arguments, emit output, and drive a
/// received block **reentrantly**), and the received block argument, if any. Fields
/// are private; a callback reaches them through the crate-internal methods.
pub struct IntrinsicCtx<'a> {
    resolved: &'a ResolvedModule,
    heap: &'a mut Heap,
    machine: &'a mut Machine,
    namespace: &'a Namespace,
    args: Vec<Value>,
    block: Option<BlockDescriptor>,
    call_span: Span,
}

/// The outcome of one reentrant block invocation ([`IntrinsicCtx::invoke_block`]).
pub(crate) enum BlockResult {
    /// The block completed (fell off its end, or `continue`d). Its yielded value is in
    /// the register; `each` discards it (a value-carrying payload for a `map`-style
    /// consumer joins later). Not a `NonLocalExit` — `continue`/fall-off target the
    /// block itself (E§7.6), so the callback runs its next iteration.
    Completed,
    /// A `break`/`return` exited the block to a target **outside** the native call
    /// (E§7.6, S-46): the unwind is parked in `machine.unwind`. The callback must stop
    /// and return promptly (no result, no further drives); the native call's apply site
    /// resumes the parked exit ([`unwind::resume_native_boundary`]).
    NonLocalExit,
    /// The nested drive parked a fault (a limit tripped inside it, or the S-15
    /// `NestedSuspend` — a suspending capability reached inside the native consumer):
    /// the caller must stop; `step` will surface the fault.
    Halted,
}

impl IntrinsicCtx<'_> {
    /// The bound argument values, in parameter order.
    pub(crate) fn args(&self) -> &[Value] {
        &self.args
    }

    /// The heap, for reading argument values (e.g. a string's bytes).
    pub(crate) fn heap(&self) -> &Heap {
        self.heap
    }

    /// Allocates a result string (`utf8` must be NFC, [`StrObj`](crate::heap::StrObj)) and
    /// returns its value — for an intrinsic that builds a string result (e.g. `each`
    /// yielding a grapheme). Freshly allocated, so it must be handed straight to
    /// [`invoke_block`](Self::invoke_block) or returned; there is no GC between here and the
    /// block's argument binding that roots it.
    pub(crate) fn alloc_string(&mut self, utf8: Box<str>) -> Value {
        Value::Str(self.heap.alloc_string(utf8))
    }

    /// Allocates a result byte string and returns its value — for an intrinsic that builds
    /// a `Bytes` result (e.g. `encode`). Handed straight back as the call's result.
    pub(crate) fn alloc_bytes(&mut self, bytes: Box<[u8]>) -> Value {
        Value::Bytes(self.heap.alloc_bytes(bytes))
    }

    /// The call site's span, for a diagnostic a callback raises.
    pub(crate) fn span(&self) -> Span {
        self.call_span
    }

    /// Appends `bytes` to the instance's captured output (`print`'s sink).
    pub(crate) fn emit(&mut self, bytes: &[u8]) {
        self.machine.output.extend_from_slice(bytes);
    }

    /// **Test-only:** requests cancellation of the instance (E§10.1) from inside this
    /// foreign call. Lets a test cancel a native consumer's reentrant drive *mid-block*
    /// (the cancel is polled at the next safe point), which is the only single-threaded
    /// way to reach the S-46 cancel-across-the-native-boundary teardown.
    #[cfg(test)]
    pub(crate) fn request_cancel(&self) {
        self.machine
            .cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Invokes this intrinsic's received block **reentrantly** with `args` (E§5.4/§7.6,
    /// MD §14): pushes the block frame at a native boundary and runs a nested drive on
    /// the shared heap stack to the block's completion. Returns [`BlockResult::Completed`]
    /// on a normal completion (fall-off or `continue`); [`BlockResult::NonLocalExit`]
    /// when a `break`/`return` exits the block across this native boundary (S-46), the
    /// unwind left parked for the apply site to resume; propagates a raise (`Err`); or
    /// reports a parked fault ([`BlockResult::Halted`]).
    pub(crate) fn invoke_block(&mut self, args: Vec<Value>) -> Result<BlockResult, Raise> {
        // A prior invocation exited across this native boundary (an S-46 `NonLocalExit`
        // is parked) and the callback drove the block **again** instead of returning
        // promptly. That is a host-contract violation (E§7.6, S-16 family): driving after
        // a non-local exit would run Doodle code the exit was leaving. Fault rather than
        // silently skip the block body. (Engine-authored intrinsics like `each` comply;
        // this backstops a misbehaving host callback once the C-ABI FFI lands, M7.)
        if self.machine.unwind.is_some() {
            self.machine.unwind = None;
            self.machine.pending_fault = Some(EngineFault::Internal);
            return Ok(BlockResult::Halted);
        }
        // A reentrant drive nests on the host's Rust stack (MD §14): bound the depth so a
        // program recursing through this native consumer faults (`StackDepth`) rather than
        // overflowing the native stack. Park the fault and return `Halted` (like a nested
        // limit) so the callback stops without pushing a deeper frame.
        if self.machine.reentry_would_overflow() {
            self.machine.pending_fault = Some(EngineFault::LimitExceeded(LimitKind::StackDepth));
            return Ok(BlockResult::Halted);
        }
        self.machine.enter_reentry();
        let result = self.invoke_block_inner(args);
        self.machine.exit_reentry();
        result
    }

    /// The reentrant nested-drive loop (guarded by [`invoke_block`]).
    fn invoke_block_inner(&mut self, args: Vec<Value>) -> Result<BlockResult, Raise> {
        let desc = self
            .block
            .expect("invoke_block: this intrinsic received no block");
        let block_span = self
            .resolved
            .ast
            .span(self.resolved.callables[desc.callable as usize].decl);
        let boundary = self.machine.frames.len();
        block::invoke_native(
            self.resolved,
            self.heap,
            self.machine,
            desc,
            &args,
            block_span,
        )?;
        loop {
            // The block frame (and anything it pushed) drained back to the boundary.
            if self.machine.frames.len() <= boundary {
                return Ok(match self.machine.unwind {
                    None => BlockResult::Completed,
                    // A `break`/`return` reached the native boundary (E§7.6, S-46): the
                    // unwind stays **parked** for the apply site to resume — either
                    // completing this call (a `break` targeting it) or unwinding past it
                    // (a `return`/outer break). The callback must return promptly.
                    Some(_) => BlockResult::NonLocalExit,
                });
            }
            match step::step(self.resolved, self.heap, self.machine, self.namespace) {
                Ok(_) => {
                    // A suspending capability was reached inside this native block-consumer's
                    // reentrant drive (S-15). The nested drive runs on the Rust stack, so the
                    // native consumer's progress cannot be frozen and resumed — the engine
                    // forbids it: clear the parked request and fault `NestedSuspend`
                    // (Decision #2, forbid-and-fault; the resumable "suspend-the-outer-drive"
                    // alternative is the deferred M7 C-ABI-yield extension, E§5.4). Terminal
                    // and deterministic.
                    if self.machine.pending.is_some() {
                        self.machine.pending = None;
                        self.machine.pending_fault = Some(EngineFault::NestedSuspend);
                        return Ok(BlockResult::Halted);
                    }
                }
                // A raise now unwinds through the frame channel (machine-design §12): it
                // pops toward this native `boundary` and is reported as `NonLocalExit`
                // (parked) at the `frames.len() <= boundary` check above, then continues
                // in the outer drive. Because `boundary >= 1`, it never drains the stack to
                // empty here, so `step` never returns `Halt::Raise` inside a nested drive.
                Err(Halt::Raise(..)) => {
                    unreachable!("a nested-drive raise unwinds to the boundary, not to Halt::Raise")
                }
                Err(Halt::Fault(fault)) => {
                    self.machine.pending_fault = Some(fault);
                    return Ok(BlockResult::Halted);
                }
            }
        }
    }
}

/// Applies an intrinsic call (E§5.1): binds the call-site arguments to the intrinsic's
/// parameters (L§8.3), then either runs a **synchronous** callback inline and leaves its
/// result in the register (`None` = Void for a `to`, E§5.2), or **suspends** — parking a
/// capability request the drive loop surfaces as `Suspended` (E§5.3/§7.5). Called from
/// [`call::apply`](super::call) when the callee is a
/// [`CallableTarget::Intrinsic`](crate::heap::CallableTarget).
pub(crate) fn apply(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    namespace: &Namespace,
    call: NodeId,
    id: u32,
    arg_values: Vec<Value>,
) -> Result<(), Raise> {
    let span = resolved.ast.span(call);
    // Read the body (Copy) and the binding shape out of the registry first, so the
    // registry borrow ends before the callback mutates the machine below.
    let intrinsic = machine.intrinsics.get(id);
    let body = intrinsic.body.clone();
    let kind = intrinsic.kind;
    let param_infos = binding::param_infos(&intrinsic.params);
    let args = binding::bind_foreign_arguments(
        resolved,
        heap,
        call,
        &intrinsic.params,
        &arg_values,
        span,
    )?;
    // Bind the `do … end` block argument to the intrinsic's block parameter, checking
    // consistency (§8.3/§8.5) — reusing the source-callable path: a block passed to a
    // block-less intrinsic raises, and a block parameter with no block raises. `each`
    // (M2b.5) receives a block here and invokes it reentrantly (`invoke_block`).
    let block = block::bind_block_argument(resolved, machine, call, &param_infos, span)?;

    match body {
        ForeignBody::Sync(callback) => {
            // This native call's frame depth: a `break` targeting it (S-46) unwinds here
            // and parks (block::invoke_native records the same depth in Consumer::Native).
            let boundary = machine.frames.len();
            // Root the call's arguments while the callback runs (MD §15): a native
            // block-consumer's reentrant drive may collect, and the args are otherwise
            // held only on the Rust stack. Popped on return (including on a raise).
            let root_base = machine.push_foreign_roots(&args);
            let result = {
                let mut ctx = IntrinsicCtx {
                    resolved,
                    heap,
                    machine,
                    namespace,
                    args,
                    block,
                    call_span: span,
                };
                callback(&mut ctx)
            };
            machine.pop_foreign_roots(root_base);
            // A reentrant nested drive may have parked a fault (`invoke_block`): a nested
            // limit, or a host-contract violation. `step` surfaces it, so stop here — do
            // not propagate the callback's result (a raise it returned is superseded).
            if machine.pending_fault.is_some() {
                return Ok(());
            }
            // A `break`/`return` in a block this callback drove crossed the native
            // boundary (S-46): an unwind is parked. The compliant callback returned
            // promptly with no result (`Ok(None)`); a value, a raise, or a further drive
            // after the `NonLocalExit` is a host-contract violation (E§7.6) → fault.
            // Otherwise resume the parked exit at this apply site: a `break` targeting
            // *this* call completes it (its value becomes the result); a `return`/outer
            // break stays parked and unwinds past this call in the enclosing drive.
            if machine.unwind.is_some() {
                if !matches!(result, Ok(None)) {
                    machine.unwind = None;
                    machine.pending_fault = Some(EngineFault::Internal);
                    return Ok(());
                }
                unwind::resume_native_boundary(machine, boundary, kind)?;
                return Ok(());
            }
            let result = result?;
            debug_assert!(
                kind != BodyKind::Func || result.is_some(),
                "a `fn` intrinsic must return a value"
            );
            // A `to` yields Void (register cleared); a `fn`'s value goes to the register.
            machine.reg = if kind == BodyKind::Proc { None } else { result };
            Ok(())
        }
        // Suspend (E§5.3/§7.5, MD §14): park the request; the drive loop returns
        // `Suspended`. No state is torn down — the caller's continuation waits, and
        // `resolve` supplies the result (or a raise). The register is left untouched.
        ForeignBody::Capability => {
            machine.pending = Some(PendingRequest {
                capability: id,
                args,
                span,
            });
            Ok(())
        }
    }
}

/// Argument binding for an intrinsic call (`param_infos`, `bind_foreign_arguments`), split
/// out for length.
mod binding;

/// The provisional demo intrinsics (`print`, `each`, `read_line`) and the value
/// renderer, built on the mechanism above. Split out for length.
mod builtins;
pub use builtins::{cos, decode, each, encode, length, print, read_line, sin};

/// The M3 platform primitives (`draw_line`/`set_turtle`/`clear_canvas`) the turtle
/// library draws through — suspending capabilities with no engine-side drawing logic
/// (E§13).
mod platform;
pub use platform::{clear_canvas, draw_line, set_turtle};

#[cfg(test)]
mod tests;
