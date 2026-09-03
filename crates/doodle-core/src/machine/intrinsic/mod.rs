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
//! M2b.5. A foreign default is a [`ConstValue`](super::native::ConstValue) **recipe**
//! (the registry predates the heap, so it holds no heap ref), materialized per call in
//! [`binding`]; `ConstValue` spans only immutable values, so a mutable foreign default is
//! unrepresentable — L§8.3-equivalent by construction (S-42, D-M7-8).

use super::error::Raise;
use super::{Handle, Value};
use crate::resolve::BodyKind;
use crate::span::Span;
use std::sync::Arc;

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

/// A parameter of an intrinsic foreign function (E§5.1). Its `default`, if present, is a
/// [`ConstValue`] **recipe** — not a heap [`Value`] — because the registry is built before
/// any heap exists (S-42). The recipe is materialized into the instance heap **when the
/// default fills a missing argument** (`binding.rs`); for the immutable values `ConstValue`
/// can express, materialize-per-call and materialize-once are indistinguishable (identity
/// is observable only for reference types, L§4.13/§4.14), so this is L§8.3-equivalent
/// (D-M7-8). `ConstValue` has no list/dict/record variant, so a **mutable** foreign default
/// is unrepresentable — the immutability restriction is enforced by construction, keeping
/// host code out of Python's mutable-default-argument footgun.
#[derive(Clone, Debug)]
pub(crate) struct ForeignParam {
    /// The parameter name (for keyword binding and diagnostics).
    pub name: Box<str>,
    /// A default-value recipe (materialized per call), or `None` for a required parameter.
    pub default: Option<super::native::ConstValue>,
    /// Whether this is the trailing block parameter (bound reentrantly, M2b.5).
    pub is_block: bool,
}

/// The callback of an **engine-authored** synchronous intrinsic (E§5.2): given the bound
/// arguments and the instance's output sink, it returns the call's result (`Some` for a
/// `fn`, `None` for a `to`'s Void) or raises. A plain `fn` pointer, not a closure — an
/// engine intrinsic's behavior has no captured Rust state, and this keeps the callback
/// [`Copy`] so reading it out of the registry does not borrow the machine while the call
/// mutates the output sink. Crate-internal (it names the machine's `Raise`); a **host**
/// registers a general callback through [`ForeignBody::Host`] (the C ABI, the only `unsafe`
/// crate), which speaks in handles instead so it never names an engine `Value`/`Raise`.
pub(crate) type IntrinsicFn = fn(&mut IntrinsicCtx) -> Result<Option<Value>, Raise>;

/// The result a **host** foreign callback ([`ForeignBody::Host`]) reports (E§5.2): the
/// call's value (or Void for a `to`), or a raise — both as host [`Handle`]s, so the C ABI
/// never names an engine `Value`/`Raise`. The engine resolves the handle against the same
/// table [`IntrinsicCtx::arg_handle`] mints from, then (for `Raise`) arms the value at the
/// call site exactly like a capability's `resolve(Raise)` (E§7.5).
#[derive(Clone, Copy, Debug)]
pub enum HostReply {
    /// A `fn` result value (host handle), or Void for a `to` (`None`).
    Value(Option<Handle>),
    /// Raise the value named by this handle at the call site.
    Raise(Handle),
}

/// A **host-registered** synchronous foreign callback (E§5.2): given the call activation
/// ([`IntrinsicCtx`]) it reads the bound arguments (as handles), may invoke a received block
/// reentrantly, and reports a [`HostReply`]. `Fn` (re-callable) and `Send + Sync` (an
/// instance is `Send`; the callback is only ever run on the single driving thread). Built by
/// the C ABI; engine intrinsics use the [`Sync`](ForeignBody::Sync) fn-pointer body instead.
pub type HostCallback = Arc<dyn Fn(&mut IntrinsicCtx) -> HostReply + Send + Sync>;

/// How a foreign function is fulfilled (engine spec E§5.1): a **synchronous** callback
/// run inline (§5.2, engine-authored or host), or a **suspending capability** that yields to
/// the host (§5.3).
#[derive(Clone)]
pub(crate) enum ForeignBody {
    /// Run an engine-authored callback inline and continue (E§5.2).
    Sync(IntrinsicFn),
    /// Run a host-registered callback inline and continue (E§5.2); it reports a
    /// [`HostReply`] the engine maps to a result or a raise.
    Host(HostCallback),
    /// Suspend: park a capability request and return `Suspended`; the host supplies the
    /// result via `resolve` (E§5.3/§7.5). The capability identity is the registry index.
    Capability,
}

// `HostCallback` is an opaque `dyn Fn`, so `ForeignBody` cannot derive `Debug` (an
// `Intrinsic` must stay `Debug`); render each variant as its name.
impl std::fmt::Debug for ForeignBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ForeignBody::Sync(_) => "Sync(..)",
            ForeignBody::Host(_) => "Host(..)",
            ForeignBody::Capability => "Capability",
        })
    }
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

/// Builds a host [`Intrinsic`] with a [`Host`](ForeignBody::Host) callback (E§5.1/§5.2) —
/// the constructor the C ABI drives, so a host never touches [`Intrinsic`]'s private fields
/// or the [`ForeignParam`] recipe directly. Ordinary parameters (required or with an
/// immutable default) then at most one trailing block parameter (L§8.2/§8.3), in the order
/// added; [`host`](Self::host) finishes with the callback.
pub struct ForeignBuilder {
    name: Box<str>,
    kind: BodyKind,
    params: Vec<ForeignParam>,
}

impl ForeignBuilder {
    /// A builder for a foreign function named `name` of `kind` (`Proc` = `to`, `Func` = `fn`).
    pub fn new(name: impl Into<Box<str>>, kind: BodyKind) -> Self {
        ForeignBuilder {
            name: name.into(),
            kind,
            params: Vec::new(),
        }
    }

    /// Adds a required ordinary parameter `name` (L§8.3).
    #[must_use]
    pub fn param(mut self, name: impl Into<Box<str>>) -> Self {
        self.params.push(ForeignParam {
            name: name.into(),
            default: None,
            is_block: false,
        });
        self
    }

    /// Adds an ordinary parameter `name` with an immutable `default` (D-M7-8): the recipe is
    /// materialized per call when the argument is omitted. `ConstValue` spans only immutable
    /// values, so a mutable default is unrepresentable — L§8.3-equivalent by construction.
    #[must_use]
    pub fn default_param(
        mut self,
        name: impl Into<Box<str>>,
        default: super::native::ConstValue,
    ) -> Self {
        self.params.push(ForeignParam {
            name: name.into(),
            default: Some(default),
            is_block: false,
        });
        self
    }

    /// Adds the trailing block parameter `name` (L§8.2): a `do … end` the callback invokes
    /// reentrantly ([`IntrinsicCtx::invoke_block_handles`]).
    #[must_use]
    pub fn block_param(mut self, name: impl Into<Box<str>>) -> Self {
        self.params.push(ForeignParam {
            name: name.into(),
            default: None,
            is_block: true,
        });
        self
    }

    /// Finishes the builder into an [`Intrinsic`] with `callback` as its host body.
    pub fn host(self, callback: HostCallback) -> Intrinsic {
        Intrinsic {
            name: self.name,
            kind: self.kind,
            params: self.params,
            body: ForeignBody::Host(callback),
        }
    }
}

/// Why registering an intrinsic or native module failed (a host-API error, E§5.5/§5.1
/// S-43). Loud by design: a mis-set-up host is a bug, not a program error.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HostError {
    /// A second intrinsic was registered under a name already registered.
    DuplicateIntrinsic(Box<str>),
    /// An intrinsic name collides with a built-in type value (`Int`, `List`, …),
    /// which also seeds the global namespace (S-43 namespace order).
    CollidesWithBuiltin(Box<str>),
    /// A second native module was registered under a name already registered (E§5.5).
    DuplicateModule(Box<str>),
}

/// The intrinsic foreign functions and **native modules** a host registers **before** the
/// first load (E§5.5, S-43/S-32). Built by the host, then moved into the instance; the
/// registration order of both is replay-identity input (E§11, MD §6).
#[derive(Default)]
pub struct Registry {
    intrinsics: Vec<Intrinsic>,
    modules: Vec<super::native::NativeModule>,
}

/// Whether `name` is a **reserved prelude name** a host intrinsic may not shadow: a built-in
/// type value ([`types::BUILTINS`](super::types::BUILTINS)) or one of the fixed non-type prelude
/// bindings the engine installs — `Error` (the built-in error record) and the well-known
/// `Stringable`/`Hashable` protocols + the `to_string` dispatcher (`machine::load::build_prelude`
/// / `protocol::seed_wellknown`). All share the one prelude module, and a module's namespace is
/// a first-match linear scan, so a same-named intrinsic (appended last) would bind a second,
/// permanently shadowed cell — rejected at registration rather than silently dead.
fn reserved_prelude_name(name: &str) -> bool {
    const FIXED: &[&str] = &["Error", "Stringable", "Hashable", "to_string"];
    super::types::BUILTINS.iter().any(|(n, _)| *n == name) || FIXED.contains(&name)
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Registry {
            intrinsics: Vec::new(),
            modules: Vec::new(),
        }
    }

    /// Registers `intrinsic`, or a [`HostError`] if its name duplicates a prior
    /// registration or a reserved prelude name (S-43). The registry is consumed into
    /// the instance at load, so there is no "after load" registration to reject here.
    pub fn register(&mut self, intrinsic: Intrinsic) -> Result<(), HostError> {
        if reserved_prelude_name(&intrinsic.name) {
            return Err(HostError::CollidesWithBuiltin(intrinsic.name.clone()));
        }
        if self.intrinsics.iter().any(|i| i.name == intrinsic.name) {
            return Err(HostError::DuplicateIntrinsic(intrinsic.name.clone()));
        }
        self.intrinsics.push(intrinsic);
        Ok(())
    }

    /// Registers a native module (E§5.5), or a [`HostError`] if its name duplicates a prior
    /// native module. Native module names live in the module-path namespace (distinct from
    /// the intrinsic/type-value global namespace), so a native module may share a name with
    /// an intrinsic. Registration happens before the instance's first load (S-32); the
    /// registry is consumed at load, so there is no "after load" case to reject.
    pub fn register_module(
        &mut self,
        module: super::native::NativeModule,
    ) -> Result<(), HostError> {
        if self.modules.iter().any(|m| m.name == module.name) {
            return Err(HostError::DuplicateModule(module.name.clone()));
        }
        self.modules.push(module);
        Ok(())
    }

    /// Consumes the registry into its parts: the flat intrinsics (the prelude module's own
    /// functions, S-43/S-60) and the native modules (pre-loaded as their own modules, E§5.5).
    /// Called once at load.
    pub(crate) fn into_parts(self) -> (Vec<Intrinsic>, Vec<super::native::NativeModule>) {
        (self.intrinsics, self.modules)
    }

    /// Builds the instance's runtime intrinsic registry from the flat intrinsic list — the
    /// host's registered intrinsics (bound in the prelude module) followed by every native
    /// module's function members, in a single id space `CallableTarget::Intrinsic` indexes
    /// (E§5.5). Load-time only.
    pub(crate) fn from_intrinsics(intrinsics: Vec<Intrinsic>) -> Self {
        Registry {
            intrinsics,
            modules: Vec::new(),
        }
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

/// The synchronous-intrinsic activation ([`IntrinsicCtx`]) and the `apply` entry point,
/// split out for length.
mod ctx;
pub(crate) use ctx::BlockResult;
pub(crate) use ctx::apply;
pub use ctx::{BlockOutcome, IntrinsicCtx};

/// Argument binding for an intrinsic call (`param_infos`, `bind_foreign_arguments`), split
/// out for length.
mod binding;

/// The value/handle boundary methods on [`IntrinsicCtx`] (`make_*`/`as_*`/`release`) a host
/// callback uses inside a synchronous call (M7.2b), split out for length.
mod values;

/// The provisional demo intrinsics (`print`, `each`, `read_line`) and the value
/// renderer, built on the mechanism above. Split out for length.
mod builtins;
pub use builtins::{cos, decode, each, encode, length, print, read_line, sin};

/// The M3 platform primitives (`draw_line`/`set_turtle`/`clear_canvas`) the turtle
/// library draws through — suspending capabilities with no engine-side drawing logic
/// (E§13).
mod platform;
pub use platform::{clear_canvas, draw_line, set_turtle};

/// The ambient nondeterministic capabilities `time`/`random` (E§5.3/§11) — clock/RNG reads
/// that suspend so the host resolves them across the recordable boundary (S-19).
mod ambient;
pub use ambient::{random, time};

#[cfg(test)]
mod tests;
