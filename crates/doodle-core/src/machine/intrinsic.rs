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
use super::error::{ExceptionKind, Raise};
use super::{Machine, Value};
use crate::ast::{Node, NodeId};
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

/// The activation of a synchronous intrinsic call (E§5.2): the bound arguments, the
/// heap (to read them), and the instance's output sink. Reentrant driving and block
/// invocation join here at M2b.5.
pub struct IntrinsicCtx<'a> {
    /// The bound argument values, in parameter order.
    pub args: &'a [Value],
    /// The heap, for reading argument values (e.g. a string's bytes).
    pub heap: &'a Heap,
    /// The instance's captured output (host stdout); `print` appends here.
    pub output: &'a mut Vec<u8>,
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
    call: NodeId,
    id: u32,
    arg_values: Vec<Value>,
) -> Result<(), Raise> {
    let span = resolved.ast.span(call);
    // Read the body (Copy) and the binding shape out of the registry first, so the
    // registry borrow ends before the callback mutates the output sink below.
    let intrinsic = machine.intrinsics.get(id);
    let body = intrinsic.body.clone();
    let kind = intrinsic.kind;
    let has_block_param = intrinsic.params.iter().any(|p| p.is_block);
    let args = bind_foreign_arguments(resolved, call, &intrinsic.params, &arg_values, span)?;

    // Parity with source callables (block.rs): a `do … end` block passed to a callee
    // that takes no block argument raises. Intrinsic block parameters are invoked
    // reentrantly (M2b.5), so at M2b.2 no intrinsic has one and any block reaches this.
    let Node::Call { block, .. } = resolved.ast.node(call) else {
        unreachable!("intrinsic::apply over a non-Call node");
    };
    if block.is_some() && !has_block_param {
        return Err(Raise::new(
            ExceptionKind::ArgumentError,
            "this call passes a `do … end` block, but the callee takes no block",
            span,
        ));
    }

    match body {
        ForeignBody::Sync(callback) => {
            let mut ctx = IntrinsicCtx {
                args: &args,
                heap,
                output: &mut machine.output,
            };
            let result = callback(&mut ctx)?;
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
    debug_assert!(
        !params.iter().any(|p| p.is_block),
        "a foreign block parameter is invoked reentrantly (M2b.5), not bound as a value"
    );
    // ParamInfo drives `bind_arguments`; slot = parameter index, so `slots` comes back
    // in parameter order.
    let param_infos: Vec<ParamInfo> = params
        .iter()
        .enumerate()
        .map(|(i, p)| ParamInfo {
            name: p.name.clone(),
            slot: i as u16,
            is_block: p.is_block,
            has_default: p.default.is_some(),
        })
        .collect();
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
            let text = render(ctx.heap, ctx.args[0]);
            ctx.output.extend_from_slice(text.as_bytes());
            ctx.output.push(b'\n');
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
mod tests {
    use super::*;
    use crate::drive::{Directive, Outcome, run};
    use crate::machine::Instance;
    use crate::span::ModuleId;

    /// An `fn` intrinsic returning `42`, for testing value-yielding foreign calls.
    fn answer() -> Intrinsic {
        Intrinsic {
            name: "answer".into(),
            kind: BodyKind::Func,
            params: Vec::new(),
            body: ForeignBody::Sync(|_ctx| Ok(Some(Value::Int(42)))),
        }
    }

    /// Loads `src` (which must load clean) with `registry`, driving it to completion
    /// and returning the instance so the caller can read its output/outcome.
    fn run_with(src: &str, registry: Registry) -> (Instance, Outcome) {
        use crate::diag::Severity;
        let nfc = crate::source::normalize(src);
        let parsed = crate::parse::parse_program(nfc.as_ref(), ModuleId(0));
        assert!(
            !parsed
                .diagnostics
                .iter()
                .any(|d| d.severity == Severity::Error),
            "parse error(s): {:?}",
            parsed.diagnostics
        );
        let resolved = crate::resolve::resolve(parsed.ast, parsed.root, ModuleId(0));
        assert!(
            resolved.diagnostics.is_empty(),
            "resolve diagnostic(s): {:?}",
            resolved.diagnostics
        );
        let mut inst = Instance::load_with_intrinsics(resolved.module, registry);
        let outcome = run(&mut inst, Directive::RunToCompletion);
        (inst, outcome)
    }

    fn registry_with(intrinsics: Vec<Intrinsic>) -> Registry {
        let mut r = Registry::new();
        for i in intrinsics {
            r.register(i).unwrap();
        }
        r
    }

    #[test]
    fn register_rejects_a_duplicate_name() {
        let mut r = Registry::new();
        r.register(print()).unwrap();
        assert_eq!(
            r.register(print()),
            Err(HostError::DuplicateIntrinsic("print".into()))
        );
    }

    #[test]
    fn register_rejects_a_builtin_type_value_name() {
        let mut r = Registry::new();
        let shadow_int = Intrinsic {
            name: "Int".into(),
            kind: BodyKind::Func,
            params: Vec::new(),
            body: ForeignBody::Sync(|_| Ok(Some(Value::Nil))),
        };
        assert_eq!(
            r.register(shadow_int),
            Err(HostError::CollidesWithBuiltin("Int".into()))
        );
    }

    #[test]
    fn print_renders_its_argument_and_appends_a_newline() {
        let (inst, outcome) = run_with("print(1 + 2)\n", registry_with(vec![print()]));
        assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
        assert_eq!(inst.output(), b"3\n");
    }

    #[test]
    fn print_renders_each_demo_scalar_kind() {
        let (inst, _) = run_with(
            "print(nil)\nprint(true)\nprint(-7)\nprint(1.5)\nprint(\"hi\")\n",
            registry_with(vec![print()]),
        );
        assert_eq!(inst.output(), b"nil\ntrue\n-7\n1.5\nhi\n");
    }

    #[test]
    fn an_fn_intrinsic_yields_a_value_the_call_consumes() {
        let (inst, _) = run_with("print(answer())\n", registry_with(vec![print(), answer()]));
        assert_eq!(inst.output(), b"42\n");
    }

    #[test]
    fn a_user_declaration_shadows_an_intrinsic() {
        // The program declares its own `print` (a no-op `to`), so the intrinsic never
        // runs — a user global is found first in the namespace scan (S-43 order).
        let (inst, outcome) = run_with(
            "to print(x)\nx\nend\nprint(\"hi\")\n",
            registry_with(vec![print()]),
        );
        assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
        assert_eq!(inst.output(), b"", "the intrinsic print was shadowed");
    }

    #[test]
    fn a_missing_argument_raises() {
        let (_, outcome) = run_with("print()\n", registry_with(vec![print()]));
        assert!(matches!(outcome, Outcome::Raised(..)), "{outcome:?}");
    }

    #[test]
    fn a_block_passed_to_a_block_less_intrinsic_raises() {
        // Parity with a source callable that takes no block (block.rs): passing a
        // `do … end` to `print` raises rather than silently dropping the block.
        let (_, outcome) = run_with("print(1) do\n1\nend\n", registry_with(vec![print()]));
        assert!(matches!(outcome, Outcome::Raised(..)), "{outcome:?}");
    }

    #[test]
    fn a_to_intrinsic_result_used_as_a_value_raises() {
        // `print` is a `to` (Void); consuming its result in an expression raises at
        // the consuming site (the runtime Void backstop — the resolver's static
        // voidcheck only knows current-module `to`s, not intrinsics).
        let (_, outcome) = run_with("print(1) + 1\n", registry_with(vec![print()]));
        assert!(matches!(outcome, Outcome::Raised(..)), "{outcome:?}");
    }
}
