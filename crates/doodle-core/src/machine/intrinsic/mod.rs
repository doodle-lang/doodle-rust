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

use super::Value;
use super::error::Raise;
use crate::resolve::BodyKind;
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

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Registry {
            intrinsics: Vec::new(),
            modules: Vec::new(),
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
pub use ctx::IntrinsicCtx;
pub(crate) use ctx::apply;

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
