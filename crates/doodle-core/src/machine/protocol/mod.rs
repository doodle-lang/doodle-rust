//! Protocols: the registry and single-dispatch resolution (L§10, plan AD5, S-31).
//!
//! **Scope (M5.5a).** `protocol`/`implement` load-time registration, dispatcher cells,
//! single dispatch on the first argument's runtime type as an ordinary driven call,
//! protocol defaults, the qualified form `P.member`, and the L§10.3 errors
//! (`protocol-not-implemented`, `ambiguous-member`). The `extends` chain and the static
//! conformance checks (first-parameter-no-default, arity/block conformance,
//! missing-required-member) land in M5.5b.
//!
//! **The model.** A `protocol P` declaration binds `P` to a [protocol value](TypeKind::Protocol)
//! and binds each of its member names to a **dispatcher cell** (a callable whose target is
//! [`CallableTarget::Dispatcher`](crate::heap::CallableTarget)). `implement P for T` records
//! the `(protocol, type, member) → callable` associations. A bare member call resolves the
//! member name to its dispatcher, binds the call's arguments against the member's declared
//! signature, and dispatches on the runtime type of the value bound to the **first
//! parameter** (S-31) — over every protocol declaring the name, so two unrelated protocols
//! that both supply it make the unqualified call ambiguous (L§10.3).
//!
//! **Determinism.** The registry is index-addressed `Vec`s scanned linearly (no hashing,
//! no address identity); protocols, members, and implementations are numbered in load
//! order, which is host replay input (MD §6 replay note). Candidate protocols are
//! reported in ascending id (load) order, so ambiguity messages are replay-stable.

mod load;

pub(crate) use load::{define_implement, define_protocol, dispatch_call};

use crate::heap::{CallableTarget, Heap};
use crate::machine::value::{CalIdx, TypeIdx};
use crate::machine::{BuiltinType, TypeKind, Value};
use crate::resolve::BodyKind;
use crate::span::ModuleId;

/// A value's **runtime type** for dispatch (L§10.3): the concrete leaf type an
/// `implement … for T` registers against and a call dispatches on. Built-in umbrella
/// types (`Number`, `Callable`) are not runtime types — a value is always one leaf — so
/// an `implement … for Number` expands to `Int` and `Float` at registration
/// ([`Registry::type_keys`]) and a lookup by the value's leaf finds it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DispatchType {
    Nil,
    Bool,
    Int,
    Float,
    Str,
    Bytes,
    List,
    Dict,
    /// A `to` value.
    Procedure,
    /// An `fn` value.
    Function,
    /// A record type, by its nominal type-value identity (L§6.5).
    Record(TypeIdx),
}

/// One member of a protocol declaration (L§10.1): its dispatch name, `to`/`fn` kind,
/// and — captured from the declaration's AST so a *required* member (which has no
/// resolved callable) still has a signature to bind against — its ordinary parameter
/// names in order and whether it takes a block parameter. `default` is the member's
/// default-body callable, if the member declared one.
struct MemberDecl {
    member: u32,
    /// The member's `to`/`fn` kind — read by the conformance check and the dispatcher-value
    /// `is` refinement in M5.5b.
    #[allow(dead_code)]
    kind: BodyKind,
    /// Ordinary (non-block) parameter names, in declaration order; `params[0]` is the
    /// dispatch parameter (`self` by convention, L§10.1).
    params: Vec<Box<str>>,
    /// The block parameter's name (`do body`), if the member takes one (last, L§8.5).
    block_param: Option<Box<str>>,
    /// The default-implementation callable, or `None` for a required member.
    default: Option<CalIdx>,
}

/// A protocol definition (L§10): its name, the module it was declared in, its single
/// `extends` parent (forming a chain to a root, S-61), and its own members. A member
/// inherited from an ancestor is *not* copied here — the chain is walked on demand.
struct ProtocolDef {
    name: Box<str>,
    #[allow(dead_code)] // consulted for cross-module diagnostics
    module: ModuleId,
    /// The parent protocol's id (`protocol Child extends Parent`), or `None` at a root.
    /// One parent per protocol, so the graph is a set of linear chains (S-61).
    extends: Option<u32>,
    members: Vec<MemberDecl>,
}

/// One `implement P for T … end` block (L§10.2): the protocol and runtime type it covers,
/// and the callable each member name it provides resolves to.
struct ImplBlock {
    protocol: u32,
    ty: DispatchType,
    methods: Vec<(u32, CalIdx)>,
}

/// The instance's protocol registry (L§10): interned member names, protocol definitions,
/// and implementation blocks — all index-addressed and scanned linearly for determinism.
#[derive(Default)]
pub(crate) struct Registry {
    /// Interned member names; the index is the member-name id.
    members: Vec<Box<str>>,
    /// Protocol definitions; the index is the protocol id.
    protocols: Vec<ProtocolDef>,
    /// Every registered `implement` block.
    impls: Vec<ImplBlock>,
}

/// How a member call resolves for a given runtime type (L§10.3).
pub(crate) enum Dispatch {
    /// Call this implementation (a member impl or a protocol default) with the bound args.
    Call(CalIdx),
    /// The type implements no protocol supplying the member — `protocol-not-implemented`.
    NotImplemented {
        type_name: String,
        protocol: Box<str>,
        member: Box<str>,
    },
    /// The member is supplied by two unrelated implemented protocols — `ambiguous-member`.
    Ambiguous {
        member: Box<str>,
        protocols: (Box<str>, Box<str>),
        type_name: String,
    },
}

impl Registry {
    /// Interns a member name, returning its id (existing id if already interned). Member
    /// names are numbered in load order, which is deterministic replay input.
    fn intern_member(&mut self, name: &str) -> u32 {
        if let Some(i) = self.members.iter().position(|m| m.as_ref() == name) {
            return i as u32;
        }
        self.members.push(name.into());
        (self.members.len() - 1) as u32
    }

    /// The interned name for a member id.
    fn member_name(&self, id: u32) -> &str {
        &self.members[id as usize]
    }

    /// The member-name id for `name`, if any protocol has declared it. (Used by the
    /// dispatcher-value `is` refinement and the assign-to-member check in M5.5b.)
    #[allow(dead_code)]
    pub(crate) fn member_id(&self, name: &str) -> Option<u32> {
        self.members
            .iter()
            .position(|m| m.as_ref() == name)
            .map(|i| i as u32)
    }

    /// The declared name of a protocol.
    pub(crate) fn protocol_name(&self, id: u32) -> &str {
        &self.protocols[id as usize].name
    }

    /// The `extends` chain from `protocol` up to its root (`[protocol, parent, …]`, S-61).
    /// A parent is always registered before its child (parent-first load ordering, so a
    /// parent's id is lower), which also guarantees the walk terminates; the length bound
    /// is a defensive backstop.
    fn chain(&self, protocol: u32) -> Vec<u32> {
        let mut ids = Vec::new();
        let mut cur = Some(protocol);
        while let Some(p) = cur {
            if ids.contains(&p) || ids.len() > self.protocols.len() {
                break;
            }
            ids.push(p);
            cur = self.protocols[p as usize].extends;
        }
        ids
    }

    /// Whether `protocol` declares `member` — its own members **or** any ancestor's
    /// (requirements are transitive along the `extends` chain, S-61).
    fn transitively_declares(&self, protocol: u32, member: u32) -> bool {
        self.chain(protocol).into_iter().any(|p| {
            self.protocols[p as usize]
                .members
                .iter()
                .any(|m| m.member == member)
        })
    }

    /// Whether protocol `from`'s chain reaches `target` (`from == target`, or `from`
    /// transitively `extends` `target`) — the transitivity behind `x is Parent` holding
    /// for a type that implements a `Child` (S-61).
    fn extends_reaches(&self, from: u32, target: u32) -> bool {
        self.chain(from).contains(&target)
    }

    /// The member-name id `P.member` selects, if protocol `id` declares a member named
    /// `name` (for the qualified form `P.member`, L§10.3). `None` if it does not.
    pub(crate) fn qualified_member(&self, id: u32, name: &str) -> Option<u32> {
        self.protocols[id as usize]
            .members
            .iter()
            .find(|m| self.members[m.member as usize].as_ref() == name)
            .map(|m| m.member)
    }

    /// The `to`/`fn` kind and block-parameter presence of a member, and its ordinary
    /// parameter names — from the first protocol that declares it (the signature the
    /// unqualified call binds against; unrelated protocols sharing a name are the
    /// ambiguity case and never reach binding).
    fn member_signature(&self, member: u32, protocol_filter: Option<u32>) -> Option<&MemberDecl> {
        self.protocols
            .iter()
            .enumerate()
            .filter(|(p, _)| protocol_filter.is_none_or(|f| *p as u32 == f))
            .find_map(|(_, def)| def.members.iter().find(|m| m.member == member))
    }

    /// Whether `(protocol, ty)` has a registered `implement` block.
    fn is_implemented(&self, protocol: u32, ty: DispatchType) -> bool {
        self.impls
            .iter()
            .any(|b| b.protocol == protocol && b.ty == ty)
    }

    /// The callable `(protocol, ty)` provides for `member`, if its impl block supplies it.
    fn impl_method(&self, protocol: u32, ty: DispatchType, member: u32) -> Option<CalIdx> {
        self.impls
            .iter()
            .filter(|b| b.protocol == protocol && b.ty == ty)
            .find_map(|b| {
                b.methods
                    .iter()
                    .find(|(m, _)| *m == member)
                    .map(|(_, c)| *c)
            })
    }

    /// The **nearest** default-body callable for `member` walking `protocol`'s chain
    /// (self, then parent, then grandparent …): the nearest declaring protocol's default
    /// wins (S-61). `None` if no protocol in the chain declares `member` with a default.
    fn nearest_default(&self, protocol: u32, member: u32) -> Option<CalIdx> {
        self.chain(protocol).into_iter().find_map(|p| {
            self.protocols[p as usize]
                .members
                .iter()
                .find(|m| m.member == member)
                .and_then(|m| m.default)
        })
    }

    /// Whether the runtime type `ty` implements protocol `id` (`x is P`, L§6.5 / §10.4).
    /// Transitive along `extends` (S-61): implementing a `Child` implies implementing every
    /// protocol in its chain, so `ty` implements `id` iff some registered `implement Q for
    /// ty` has `Q`'s chain reach `id`.
    pub(crate) fn type_implements(&self, ty: DispatchType, id: u32) -> bool {
        self.impls
            .iter()
            .any(|b| b.ty == ty && self.extends_reaches(b.protocol, id))
    }

    /// Resolves a member call (L§10.3): the candidate protocols are those declaring
    /// `member` that `ty` implements (restricted to `protocol_filter` for a qualified
    /// `P.member`). Zero → not-implemented; two or more → ambiguous; exactly one →
    /// its impl method, falling back to that protocol's default (an implemented block
    /// that supplies neither is itself not-implemented for this member).
    pub(crate) fn resolve(
        &self,
        member: u32,
        ty: DispatchType,
        protocol_filter: Option<u32>,
        heap: &Heap,
    ) -> Dispatch {
        // A candidate is a protocol that (transitively) declares `member` and is (directly)
        // implemented for `ty`. Candidacy is by direct `implement` block, not transitive
        // implementation — so a member reached through one chain contributes its one
        // implemented protocol once, and chain-related protocols never make each other
        // ambiguous (S-61); two *unrelated* implemented protocols still do (L§10.3).
        let mut candidates: Vec<u32> = Vec::new();
        for p in 0..self.protocols.len() as u32 {
            if protocol_filter.is_some_and(|f| f != p) {
                continue;
            }
            if self.transitively_declares(p, member) && self.is_implemented(p, ty) {
                candidates.push(p);
            }
        }
        // A directly-implemented **ancestor** is subsumed by a directly-implemented
        // **descendant** (the more-derived protocol's chain already covers the member, so
        // dispatching through it is unambiguous, S-61): keep only the maximal candidates.
        // What remains and still numbers two or more are genuinely *unrelated* protocols
        // (L§10.3 ambiguity).
        let maximal: Vec<u32> = candidates
            .iter()
            .copied()
            .filter(|&p| {
                !candidates
                    .iter()
                    .any(|&q| q != p && self.extends_reaches(q, p))
            })
            .collect();
        match maximal.as_slice() {
            [] => Dispatch::NotImplemented {
                type_name: dispatch_type_name(ty, heap),
                protocol: self.declaring_protocol(member, protocol_filter).into(),
                member: self.member_name(member).into(),
            },
            [p] => match self
                .impl_method(*p, ty, member)
                .or_else(|| self.nearest_default(*p, member))
            {
                Some(cal) => Dispatch::Call(cal),
                None => Dispatch::NotImplemented {
                    type_name: dispatch_type_name(ty, heap),
                    protocol: self.protocol_name(*p).into(),
                    member: self.member_name(member).into(),
                },
            },
            [a, b, ..] => Dispatch::Ambiguous {
                member: self.member_name(member).into(),
                protocols: (self.protocol_name(*a).into(), self.protocol_name(*b).into()),
                type_name: dispatch_type_name(ty, heap),
            },
        }
    }

    /// A protocol declaring `member` (for a not-implemented message that points at the
    /// fix `implement P for T`): the filtered protocol if qualified, else the first
    /// declarer in load order.
    fn declaring_protocol(&self, member: u32, protocol_filter: Option<u32>) -> &str {
        if let Some(f) = protocol_filter {
            return self.protocol_name(f);
        }
        self.protocols
            .iter()
            .find(|def| def.members.iter().any(|m| m.member == member))
            .map_or("?", |def| def.name.as_ref())
    }

    /// The signature (ordinary param names, block flag) a bare member call binds against,
    /// plus the member-name id — for [`super::call`] to determine the dispatch argument.
    /// `None` if `name` is not a member name.
    pub(crate) fn member_call_signature(
        &self,
        member: u32,
        protocol_filter: Option<u32>,
    ) -> Option<(&[Box<str>], bool)> {
        self.member_signature(member, protocol_filter)
            .map(|m| (m.params.as_slice(), m.block_param.is_some()))
    }

    /// The `to`/`fn` kind a bare protocol dispatcher value reports for `x is Procedure` /
    /// `Function` — the member's declared kind (from its first declarer).
    pub(crate) fn member_kind(&self, member: u32) -> Option<BodyKind> {
        self.member_signature(member, None).map(|m| m.kind)
    }

    /// The callables the registry holds (member defaults and impl methods) that no
    /// namespace cell references — GC roots for the collector (`machine/gc.rs`).
    pub(crate) fn rooted_callables(&self) -> impl Iterator<Item = CalIdx> + '_ {
        let defaults = self
            .protocols
            .iter()
            .flat_map(|p| p.members.iter().filter_map(|m| m.default));
        let methods = self
            .impls
            .iter()
            .flat_map(|b| b.methods.iter().map(|(_, c)| *c));
        defaults.chain(methods)
    }

    /// The runtime-type keys a type *value* covers for `implement … for T`: a concrete
    /// built-in is one leaf; an umbrella (`Number`, `Callable`) expands to its leaves; a
    /// record type is its nominal identity (L§6.5). A protocol value has no keys (you
    /// cannot implement *for* a protocol).
    pub(crate) fn type_keys(idx: TypeIdx, heap: &Heap) -> Vec<DispatchType> {
        match &heap.type_value(idx).kind {
            TypeKind::Record(_) => vec![DispatchType::Record(idx)],
            TypeKind::Protocol(_) => Vec::new(),
            TypeKind::Builtin(b) => match b {
                BuiltinType::Int => vec![DispatchType::Int],
                BuiltinType::Float => vec![DispatchType::Float],
                BuiltinType::Number => vec![DispatchType::Int, DispatchType::Float],
                BuiltinType::Bool => vec![DispatchType::Bool],
                BuiltinType::String => vec![DispatchType::Str],
                BuiltinType::Bytes => vec![DispatchType::Bytes],
                BuiltinType::Nil => vec![DispatchType::Nil],
                BuiltinType::List => vec![DispatchType::List],
                BuiltinType::Dict => vec![DispatchType::Dict],
                BuiltinType::Procedure => vec![DispatchType::Procedure],
                BuiltinType::Function => vec![DispatchType::Function],
                BuiltinType::Callable => vec![DispatchType::Procedure, DispatchType::Function],
            },
        }
    }

    /// Records a protocol definition (L§10.1), returning its id. `members` carries each
    /// member's signature and its default-body callable (already interned by the caller);
    /// `extends` is the parent's id, resolved parent-first by the caller (S-61).
    fn add_protocol(
        &mut self,
        name: Box<str>,
        module: ModuleId,
        extends: Option<u32>,
        members: Vec<MemberDecl>,
    ) -> u32 {
        self.protocols.push(ProtocolDef {
            name,
            module,
            extends,
            members,
        });
        (self.protocols.len() - 1) as u32
    }

    /// Records an `implement P for T` block (L§10.2) for one runtime-type key.
    fn add_impl(&mut self, protocol: u32, ty: DispatchType, methods: Vec<(u32, CalIdx)>) {
        self.impls.push(ImplBlock {
            protocol,
            ty,
            methods,
        });
    }
}

/// A value's runtime type for dispatch (L§10.3). Every value has exactly one; a callable's
/// `to`/`fn` split reads the callable's own module (a cross-module dispatch is still keyed
/// by the concrete kind). A bare dispatcher value classifies as a `Function` (provisional,
/// M5.5b — matching [`super::types`]).
pub(crate) fn dispatch_type_of(
    value: Value,
    heap: &Heap,
    modules: &[super::LoadedModule],
    intrinsics: &super::intrinsic::Registry,
) -> DispatchType {
    match value {
        Value::Nil => DispatchType::Nil,
        Value::Bool(_) => DispatchType::Bool,
        Value::Int(_) | Value::BigInt(_) => DispatchType::Int,
        Value::Float(_) => DispatchType::Float,
        Value::Str(_) => DispatchType::Str,
        Value::Bytes(_) => DispatchType::Bytes,
        Value::List(_) => DispatchType::List,
        Value::Dict(_) => DispatchType::Dict,
        Value::Record(r) => DispatchType::Record(heap.record(r).type_idx),
        Value::Callable(cal) => {
            let kind = match heap.callable(cal).target {
                CallableTarget::Source(id) => {
                    let m = heap.callable(cal).module.0 as usize;
                    modules[m].resolved.callables[id as usize].kind
                }
                CallableTarget::Intrinsic(iid) => intrinsics.kind_of(iid),
                CallableTarget::Dispatcher { .. } => BodyKind::Func,
            };
            match kind {
                BodyKind::Proc => DispatchType::Procedure,
                _ => DispatchType::Function,
            }
        }
        // A module, a type value, or a foreign value is not a record/scalar; it has no
        // useful dispatch leaf. No `implement … for` form targets these, so such a value
        // never resolves a member — dispatch reports not-implemented via an unmatched key.
        Value::Module(_) | Value::Type(_) | Value::Foreign(_) => DispatchType::Nil,
    }
}

/// A display name for a runtime type, for diagnostics (L§10.3 messages).
fn dispatch_type_name(ty: DispatchType, heap: &Heap) -> String {
    match ty {
        DispatchType::Nil => "Nil".into(),
        DispatchType::Bool => "Bool".into(),
        DispatchType::Int => "Int".into(),
        DispatchType::Float => "Float".into(),
        DispatchType::Str => "String".into(),
        DispatchType::Bytes => "Bytes".into(),
        DispatchType::List => "List".into(),
        DispatchType::Dict => "Dict".into(),
        DispatchType::Procedure => "Procedure".into(),
        DispatchType::Function => "Function".into(),
        DispatchType::Record(idx) => match &heap.type_value(idx).kind {
            TypeKind::Record(rt) => rt.name.to_string(),
            _ => "record".into(),
        },
    }
}
