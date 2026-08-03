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

use super::call::bind_arguments;
use super::control::Namespace;
use super::error::{ExceptionKind, Raise};
use super::frame::BlockDescriptor;
use super::{Halt, Machine, Value, block, step};
use crate::ast::NodeId;
use crate::drive::{EngineFault, LimitKind};
use crate::heap::Heap;
use crate::resolve::{BodyKind, ParamInfo, ResolvedModule};
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
    /// the register; M2b.5a's only consumer (`each`) discards it (a value-carrying
    /// payload for a `map`-style consumer joins later).
    Completed,
    /// The nested drive parked a fault (a limit tripped inside it, or the deferred
    /// S-15 nested-suspend): the caller must stop; `step` will surface the fault.
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

    /// The call site's span, for a diagnostic a callback raises.
    pub(crate) fn span(&self) -> Span {
        self.call_span
    }

    /// Appends `bytes` to the instance's captured output (`print`'s sink).
    pub(crate) fn emit(&mut self, bytes: &[u8]) {
        self.machine.output.extend_from_slice(bytes);
    }

    /// Invokes this intrinsic's received block **reentrantly** with `args` (E§5.4/§7.6,
    /// MD §14): pushes the block frame at a native boundary and runs a nested drive on
    /// the shared heap stack to the block's completion. Returns the block's yielded
    /// value ([`BlockResult::Completed`]), propagates a raise (`Err`), or reports a
    /// parked fault ([`BlockResult::Halted`]). **M2b.5a:** a `break`/`return` that exits
    /// the block *across* this native boundary is not yet supported — it raises
    /// [`ExceptionKind::Unsupported`] pending the S-46 `NonLocalExit` mechanism (M2b.5b).
    pub(crate) fn invoke_block(&mut self, args: Vec<Value>) -> Result<BlockResult, Raise> {
        // A reentrant drive nests on the host's Rust stack (MD §14): bound the depth so a
        // program recursing through this native consumer faults (`StackDepth`) rather than
        // overflowing the native stack. Park the fault and return `Halted` (like a nested
        // limit) so the callback stops without pushing a deeper frame.
        if self.machine.reentry_would_overflow() {
            self.machine.reentry_fault = Some(EngineFault::LimitExceeded(LimitKind::StackDepth));
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
                return match self.machine.unwind {
                    None => Ok(BlockResult::Completed),
                    // A `break`/`return` reached the native boundary — S-46 (M2b.5b).
                    Some(_) => {
                        self.machine.unwind = None;
                        Err(Raise::new(
                            ExceptionKind::Unsupported,
                            "a `break`/`return` out of a native block-consuming function is not \
                             yet supported (S-46, arrives at M2b.5b)",
                            block_span,
                        ))
                    }
                };
            }
            match step::step(self.resolved, self.heap, self.machine, self.namespace) {
                Ok(_) => {
                    // A capability suspended inside the nested drive — S-15 (M3).
                    // Deferred: clear it and fault the drive.
                    if self.machine.pending.is_some() {
                        self.machine.pending = None;
                        self.machine.reentry_fault = Some(EngineFault::Internal);
                        return Ok(BlockResult::Halted);
                    }
                }
                Err(Halt::Raise(raise)) => return Err(raise),
                Err(Halt::Fault(fault)) => {
                    self.machine.reentry_fault = Some(fault);
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
    let param_infos = param_infos(&intrinsic.params);
    let args = bind_foreign_arguments(resolved, call, &intrinsic.params, &arg_values, span)?;
    // Bind the `do … end` block argument to the intrinsic's block parameter, checking
    // consistency (§8.3/§8.5) — reusing the source-callable path: a block passed to a
    // block-less intrinsic raises, and a block parameter with no block raises. `each`
    // (M2b.5) receives a block here and invokes it reentrantly (`invoke_block`).
    let block = block::bind_block_argument(resolved, machine, call, &param_infos, span)?;

    match body {
        ForeignBody::Sync(callback) => {
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
            let result = result?;
            // A reentrant nested drive may have parked a fault (`invoke_block`); `step`
            // surfaces it, so do not overwrite it with a spurious result.
            if machine.reentry_fault.is_none() {
                debug_assert!(
                    kind != BodyKind::Func || result.is_some(),
                    "a `fn` intrinsic must return a value"
                );
                // A `to` yields Void (register cleared); a `fn`'s value goes to the register.
                machine.reg = if kind == BodyKind::Proc { None } else { result };
            }
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

/// The [`ParamInfo`] view of an intrinsic's parameters (slot = index), for the shared
/// argument- and block-binding helpers.
fn param_infos(params: &[ForeignParam]) -> Vec<ParamInfo> {
    params
        .iter()
        .enumerate()
        .map(|(i, p)| ParamInfo {
            name: p.name.clone(),
            slot: i as u16,
            is_block: p.is_block,
            has_default: p.default.is_some(),
        })
        .collect()
}

/// Binds call-site arguments to an intrinsic's ordinary parameters (L§8.3), returning
/// the values in parameter order. Reuses [`bind_arguments`] for the positional/
/// keyword/too-many/unknown-keyword/duplicate logic (parity with Doodle calls), then
/// fills each unbound parameter from its inline default or raises a missing-argument
/// error. A block parameter is not a value here (invoked reentrantly, M2b.5).
fn bind_foreign_arguments(
    resolved: &ResolvedModule,
    call: NodeId,
    params: &[ForeignParam],
    arg_values: &[Value],
    span: Span,
) -> Result<Vec<Value>, Raise> {
    // ParamInfo drives `bind_arguments`; slot = parameter index, so `slots` comes back
    // in parameter order.
    let param_infos = param_infos(params);
    let (slots, filled) = bind_arguments(
        resolved,
        call,
        &param_infos,
        params.len() as u16,
        arg_values,
        span,
    )?;
    let mut args = Vec::with_capacity(params.len());
    for (i, p) in params.iter().enumerate() {
        // The trailing block parameter is bound separately (invoked reentrantly, MD §14),
        // never as an ordinary value here.
        if p.is_block {
            continue;
        }
        let value = match slots[i] {
            Some(v) => v,
            None => match (filled[i], p.default) {
                (false, Some(d)) => d,
                _ => {
                    return Err(Raise::new(
                        ExceptionKind::ArgumentError,
                        format!("missing argument `{}` for this call", p.name),
                        span,
                    ));
                }
            },
        };
        args.push(value);
    }
    Ok(args)
}

/// The demo intrinsic `print` (E§5.2, S-43): a `to` taking one value, rendering it
/// (the provisional [`render`] stand-in for L§15 Stringable, superseded at M4/M9a),
/// and appending it plus a newline to the instance's output sink.
pub fn print() -> Intrinsic {
    Intrinsic {
        name: "print".into(),
        kind: BodyKind::Proc,
        params: vec![ForeignParam {
            name: "value".into(),
            default: None,
            is_block: false,
        }],
        body: ForeignBody::Sync(|ctx| {
            let text = render(ctx.heap(), ctx.args()[0]);
            ctx.emit(text.as_bytes());
            ctx.emit(b"\n");
            Ok(None)
        }),
    }
}

/// The demo native block-consuming intrinsic `each` (E§5.4/§7.6, MD §14): a `to` taking
/// a `List` and a trailing block, invoking the block **reentrantly** once per element
/// (exit criterion 4). A raise inside the block propagates; a `break`/`return` out of it
/// is M2b.5b (S-46). The first native higher-order primitive — the shape `repeat`/`map`
/// take, and proof a native consumer is expressible over the reentrant-drive API.
pub fn each() -> Intrinsic {
    Intrinsic {
        name: "each".into(),
        kind: BodyKind::Proc,
        params: vec![
            ForeignParam {
                name: "list".into(),
                default: None,
                is_block: false,
            },
            ForeignParam {
                name: "body".into(),
                default: None,
                is_block: true,
            },
        ],
        body: ForeignBody::Sync(|ctx| {
            let Value::List(idx) = ctx.args()[0] else {
                return Err(Raise::new(
                    ExceptionKind::TypeMismatch,
                    "`each` needs a list to iterate",
                    ctx.span(),
                ));
            };
            // Iterate a **fixed count** (the length at entry) over the live heap list —
            // which stays rooted through `each`'s `foreign_roots` entry (MD §15), so the
            // block's reentrant drive may collect. The fixed count bounds a block that
            // appends; `.get` guards a block that shrinks the list.
            let count = ctx.heap().list(idx).items.len();
            for i in 0..count {
                let Some(&element) = ctx.heap().list(idx).items.get(i) else {
                    break; // the block shrank the list past here
                };
                match ctx.invoke_block(vec![element])? {
                    // The block completed (or `continue`d): go to the next element.
                    BlockResult::Completed => {}
                    // A nested fault was parked (a limit inside the block, or S-15): stop;
                    // `step` surfaces it after this call returns.
                    BlockResult::Halted => break,
                }
            }
            Ok(None)
        }),
    }
}

/// The demo suspending capability `read_line` (E§5.3, §7.5): a `fn` taking no arguments
/// that **suspends** — the host supplies the line via `resolve(Value)` (or fails it via
/// `resolve(Raise)`). The canonical scripted capability for the M2b drive tests.
pub fn read_line() -> Intrinsic {
    Intrinsic {
        name: "read_line".into(),
        kind: BodyKind::Func,
        params: Vec::new(),
        body: ForeignBody::Capability,
    }
}

/// A **provisional** value renderer for `print` over the demo subset — a stand-in for
/// the L§15 Stringable dispatcher (real `to_string` protocol dispatch is M4/M9a). It
/// must be **deterministic** (E§11): integers/bignums render exactly, floats use a
/// fixed shortest-round-trip format, and no address/ordering leaks in. Compound
/// values (list/bytes/records/…) get a provisional angle-bracket tag until the real
/// dispatcher lands. Crate-visible so `resolve(Raise)` can render a host-raised value
/// into its message (`machine.rs`).
pub(crate) fn render(heap: &Heap, value: Value) -> String {
    match value {
        Value::Nil => "nil".to_string(),
        Value::Bool(true) => "true".to_string(),
        Value::Bool(false) => "false".to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(idx) => heap.bigint(idx).value.to_string(),
        Value::Float(x) => render_float(x),
        Value::Str(idx) => heap.string(idx).utf8.to_string(),
        Value::Bytes(_) => "<bytes>".to_string(),
        Value::List(_) => "<list>".to_string(),
        Value::Dict(_) => "<dict>".to_string(),
        Value::Record(_) => "<record>".to_string(),
        Value::Callable(_) => "<callable>".to_string(),
        Value::Module(_) => "<module>".to_string(),
        Value::Type(_) => "<type>".to_string(),
        Value::Foreign(_) => "<foreign>".to_string(),
    }
}

/// Deterministic float rendering for the provisional `print` (E§11 fixed float
/// formatting). Every machine-produced float is finite (S-56); an integer-valued
/// float still shows a `.0` so it is not mistaken for an integer.
fn render_float(x: f64) -> String {
    if x == x.trunc() && x.is_finite() {
        format!("{x:.1}")
    } else {
        // Rust's `{}` for f64 is the shortest round-tripping decimal — deterministic.
        format!("{x}")
    }
}

#[cfg(test)]
mod tests;
