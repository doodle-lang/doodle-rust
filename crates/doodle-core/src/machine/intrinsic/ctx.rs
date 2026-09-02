//! The synchronous-intrinsic **activation** (engine spec E§5.2, MD §14) and the `apply`
//! entry point: reading bound arguments, emitting output, driving a received block
//! **reentrantly** (§5.4), and dispatching a call to its callback or parking a suspending
//! capability. Split from `mod.rs` (the intrinsic data types + [`Registry`](super::Registry))
//! for length.

use super::binding;
use super::{ForeignBody, HostReply, PendingRequest};
use crate::ast::NodeId;
use crate::drive::{EngineFault, LimitKind};
use crate::heap::Heap;
use crate::machine::error::Raise;
use crate::machine::frame::BlockDescriptor;
use crate::machine::modload::Suspension;
use crate::machine::{Halt, Handle, LoadedModule, Machine, Value, block, observe, step, unwind};
use crate::resolve::{BodyKind, ResolvedModule};
use crate::span::{ModuleId, Span};
use std::sync::Arc;

/// The activation of a synchronous intrinsic call (E§5.2, MD §14): the bound
/// arguments, the machine/heap state (to read arguments, emit output, and drive a
/// received block **reentrantly**), and the received block argument, if any. Fields
/// are private; a callback reaches them through the crate-internal methods.
pub struct IntrinsicCtx<'a> {
    resolved: &'a ResolvedModule,
    heap: &'a mut Heap,
    machine: &'a mut Machine,
    /// The module table (AD5): a reentrant nested drive ([`invoke_block`]) steps the
    /// machine, so it needs the same multi-module context the top drive has — deriving the
    /// executing frame's module's AST per step and reaching cross-module callees.
    modules: &'a mut [LoadedModule],
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

/// The public outcome of a **host**-invoked reentrant block
/// ([`IntrinsicCtx::invoke_block_handles`]), mirrored across the C ABI as
/// `DoodleBlockOutcome`. `NonLocalExit` and `Halted` both mean the host callback must return
/// promptly with no result (E§7.6): a `break`/`return`/raise crossed the boundary, or a
/// fault parked. The engine's apply-site backstop (S-46/S-15) enforces this — a host that
/// drives on regardless faults, never UB.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockOutcome {
    /// The block completed (fell off its end, or `continue`d).
    Completed,
    /// A `break`/`return`/raise exited the block across the host boundary; the unwind is
    /// parked for the apply site to resume.
    NonLocalExit,
    /// A fault parked (a limit, the S-15 `NestedSuspend`, or a stale block-argument handle).
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

    /// Appends `bytes` to the instance's captured output (`print`'s sink). Part of the
    /// curated host-call surface (E§5.2) the C ABI drives, alongside the engine `print`.
    pub fn emit(&mut self, bytes: &[u8]) {
        self.machine.output.extend_from_slice(bytes);
    }

    /// The number of bound (non-block) arguments (E§5.2) — the host reads each with
    /// [`arg_handle`](Self::arg_handle).
    pub fn arg_count(&self) -> usize {
        self.args.len()
    }

    /// Interns the `i`-th bound argument as a fresh **host-owned** handle (a GC root until
    /// released), or `None` if `i` is out of range. The host reads it with the ordinary
    /// boundary readers and releases it — like a `make_*` result (E§4.2/§4.3).
    pub fn arg_handle(&mut self, i: usize) -> Option<Handle> {
        let value = *self.args.get(i)?;
        Some(self.machine.handles.intern(value))
    }

    /// Invokes this call's received block **reentrantly** (E§5.4/§7.6) with the values named
    /// by `args` — [`invoke_block`](Self::invoke_block) with a Handle-in / [`BlockOutcome`]-out
    /// surface, so no engine `Value`/`Raise` crosses the C ABI. A stale handle in `args` is a
    /// host-contract violation → an `Internal` fault surfaced as `Halted`. A block-argument
    /// binding error (e.g. wrong block arity) arms its raise at the boundary and reports
    /// `NonLocalExit`, so it propagates like any block-body raise.
    pub fn invoke_block_handles(&mut self, args: &[Handle]) -> BlockOutcome {
        let mut values = Vec::with_capacity(args.len());
        for &handle in args {
            match self.machine.handles.resolve(handle) {
                Ok(value) => values.push(value),
                Err(_) => {
                    self.machine.pending_fault = Some(EngineFault::Internal);
                    return BlockOutcome::Halted;
                }
            }
        }
        match self.invoke_block(values) {
            Ok(BlockResult::Completed) => BlockOutcome::Completed,
            Ok(BlockResult::NonLocalExit) => BlockOutcome::NonLocalExit,
            Ok(BlockResult::Halted) => BlockOutcome::Halted,
            // A block-binding raise (a body raise instead comes back parked as
            // `NonLocalExit`): materialize + arm it at the boundary so the apply site
            // propagates it, then report the non-local exit the host must return on.
            Err(raise) => {
                step::arm_raise(self.resolved, self.machine, self.heap, raise);
                BlockOutcome::NonLocalExit
            }
        }
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
            // Like the top drive, derive the executing frame's module's AST each step (a
            // block may call across modules, AD5); clone the Arc so the table is free to
            // borrow mutably alongside.
            let cur = self.machine.frames.last().map_or(ModuleId(0), |f| f.module);
            let resolved = Arc::clone(&self.modules[cur.0 as usize].resolved);
            match step::step(&resolved, self.modules, self.heap, self.machine) {
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
    modules: &mut [LoadedModule],
    heap: &mut Heap,
    machine: &mut Machine,
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
                    modules,
                    heap,
                    machine,
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
        ForeignBody::Host(callback) => {
            // As `Sync`, but a host callback reports a `HostReply` (handles) the engine maps
            // to a result or a raise. Same boundary depth, argument-root pinning, and
            // S-46/S-15 backstops.
            let boundary = machine.frames.len();
            let root_base = machine.push_foreign_roots(&args);
            let reply = {
                let mut ctx = IntrinsicCtx {
                    resolved,
                    modules,
                    heap,
                    machine,
                    args,
                    block,
                    call_span: span,
                };
                callback(&mut ctx)
            };
            machine.pop_foreign_roots(root_base);
            // A reentrant nested drive parked a fault (`invoke_block_handles`): `step`
            // surfaces it — stop, superseding the callback's reply.
            if machine.pending_fault.is_some() {
                return Ok(());
            }
            // A `break`/`return`/raise in a block this callback drove crossed the native
            // boundary (S-46): the compliant reply is `Value(None)` (return promptly, no
            // result); a value or a raise instead is a host-contract violation (E§7.6) →
            // fault. Otherwise resume the parked exit at this apply site.
            if machine.unwind.is_some() {
                if !matches!(reply, HostReply::Value(None)) {
                    machine.unwind = None;
                    machine.pending_fault = Some(EngineFault::Internal);
                    return Ok(());
                }
                unwind::resume_native_boundary(machine, boundary, kind)?;
                return Ok(());
            }
            // No parked transfer: map the reply. A `to` yields Void; a `fn`'s value goes to
            // the register; a `Raise` arms the host's value at the call site (E§7.5), like a
            // capability's `resolve(Raise)`. A stale result/raise handle is a host-contract
            // violation → `Internal` fault.
            match reply {
                HostReply::Value(handle) => {
                    let value = match handle {
                        None => None,
                        Some(h) => match machine.handles.resolve(h) {
                            Ok(v) => Some(v),
                            Err(_) => {
                                machine.pending_fault = Some(EngineFault::Internal);
                                return Ok(());
                            }
                        },
                    };
                    debug_assert!(
                        kind != BodyKind::Func || value.is_some(),
                        "a `fn` host callback must reply with a value"
                    );
                    machine.reg = if kind == BodyKind::Proc { None } else { value };
                    Ok(())
                }
                HostReply::Raise(h) => {
                    let value = match machine.handles.resolve(h) {
                        Ok(v) => v,
                        Err(_) => {
                            machine.pending_fault = Some(EngineFault::Internal);
                            return Ok(());
                        }
                    };
                    let trace = observe::capture_trace(resolved, heap, machine, Some(span));
                    machine.arm_raise_value(value, trace);
                    Ok(())
                }
            }
        }
        // Suspend (E§5.3/§7.5, MD §14): park the request; the drive loop returns
        // `Suspended`. No state is torn down — the caller's continuation waits, and
        // `resolve` supplies the result (or a raise). The register is left untouched.
        ForeignBody::Capability => {
            machine.pending = Some(Suspension::Capability(PendingRequest {
                capability: id,
                args,
                span,
            }));
            Ok(())
        }
    }
}
