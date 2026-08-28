//! Built-in type values and the `is` membership test (L§4.12, L§6.5).
//!
//! **Scope (M2a.5).** The built-in type values (`Int`, `Float`, `Number`, …) and
//! `x is T` for those cases. Record types and protocol values — and protocol
//! membership (`x is SomeProtocol`) — join at M4/M5.
//!
//! **Provisional prelude.** L§11.4 builds no names into the language: type-value
//! names are ordinary standard-library (prelude) names. The standard library does
//! not exist yet (its hooks are pinned in L§15; the platform-primitive mechanism
//! in E§13), so until it does the instance seeds a small **built-in type-value
//! prelude** directly ([`BUILTINS`]) so `is` is testable now. When the real
//! prelude lands this seeding is removed. Tracked as a provisional in claude-todo.

use super::Value;
use super::error::{ExceptionKind, Raise};
use super::intrinsic::Registry;
use crate::heap::{CallableTarget, Heap};
use crate::resolve::{BodyKind, ResolvedModule};
use crate::span::Span;

/// A type value (L§4.12): a **built-in** type, a user-declared **record** type, or a
/// **protocol** value (L§10). Stored in a [`TypeObj`](crate::heap::TypeObj); a
/// `Value::Type` names it.
#[derive(Clone, Debug)]
pub(crate) enum TypeKind {
    /// A built-in type (`Int`, `List`, …).
    Builtin(BuiltinType),
    /// A record type declared with `record …` (L§9).
    Record(RecordType),
    /// A protocol declared with `protocol …` (L§10): the value bound to the protocol
    /// name, used with `is` (`x is P`, L§6.5) and the qualified form `P.member` (L§10.3).
    Protocol(ProtocolType),
}

/// A protocol value's schema (L§10): its declared name (for `is`, `P.member`, and
/// messages) and its id into the machine's protocol [`Registry`](super::protocol::Registry),
/// through which dispatch, defaults, and the `extends` chain are resolved.
#[derive(Clone, Debug)]
pub(crate) struct ProtocolType {
    /// The declared protocol name.
    pub name: Box<str>,
    /// The protocol's id in the registry.
    pub id: u32,
}

/// A record type's schema (L§9): its name, field names in declaration order, and
/// whether it is a `ref record`. The schema lives on the type value; an instance
/// ([`RecObj`](crate::heap::RecObj)) stores only its field values positionally and a
/// reference back to this type. The value-vs-reference distinction (`ref record`,
/// L§4.14) is read by [`copy_on_bind`](super::record::copy_on_bind): a value record
/// is copied on binding, a `ref` record shared.
#[derive(Clone, Debug)]
pub(crate) struct RecordType {
    /// The declared type name (for reflection and error messages).
    pub name: Box<str>,
    /// Field names, in declaration order — the order an instance's values follow.
    pub fields: Box<[Box<str>]>,
    /// Whether this is a `ref record` (L§4.14): its instances are *shared* on
    /// binding/assignment/argument-passing, where a value record is *copied*.
    pub is_ref: bool,
}

/// A built-in type value (L§4.12). The spellings are provisional (L Appendix D).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BuiltinType {
    /// `Int` — matches any integer (`Int` or a promoted `BigInt`).
    Int,
    /// `Float`.
    Float,
    /// `Number` — either `Int` or `Float` (L§6.5).
    Number,
    /// `Bool`.
    Bool,
    /// `String`.
    String,
    /// `Bytes`.
    Bytes,
    /// `Nil`.
    Nil,
    /// `List`.
    List,
    /// `Dict`.
    Dict,
    /// `Procedure` — a `to` (yields no value). Concrete member of the callable
    /// trio (L§4.12, S-37): the language's `to`/`fn` distinction is load-bearing
    /// (L§8), so `is` distinguishes them; a `Function` is **not** a `Procedure`.
    Procedure,
    /// `Function` — an `fn` (named or anonymous; yields a value). The other
    /// concrete member of the callable trio.
    Function,
    /// `Callable` — the umbrella over `Procedure` and `Function` (any callable),
    /// parallel to `Number` over `Int`/`Float`: `x is Callable` iff `x is Procedure`
    /// or `x is Function`. A record **type** value is not `Callable` (it is
    /// constructed with call syntax but is a Type, L§4.1).
    Callable,
}

/// The built-in type-value prelude: each name and the type it denotes, seeded
/// into a fresh instance's namespace (see the module note). Fixed order, so cell
/// and serial assignment is deterministic (MD §6 replay note).
pub(crate) const BUILTINS: &[(&str, BuiltinType)] = &[
    ("Int", BuiltinType::Int),
    ("Float", BuiltinType::Float),
    ("Number", BuiltinType::Number),
    ("Bool", BuiltinType::Bool),
    ("String", BuiltinType::String),
    ("Bytes", BuiltinType::Bytes),
    ("Nil", BuiltinType::Nil),
    ("List", BuiltinType::List),
    ("Dict", BuiltinType::Dict),
    // The callable trio: concrete members first, then the umbrella (mirroring
    // `Int`, `Float`, `Number`).
    ("Procedure", BuiltinType::Procedure),
    ("Function", BuiltinType::Function),
    ("Callable", BuiltinType::Callable),
];

/// Whether `value`'s type is `ty` (L§6.5 membership, built-in cases). Numbers:
/// `Int` matches an integer of either representation (the canonical-int invariant
/// keeps small integers as `Int` and larger ones as `BigInt`, MD §3), `Float`
/// matches a float, and `Number` matches either.
///
/// The callable trio (`Procedure`/`Function`/`Callable`) needs the value's `to`/`fn`
/// kind, which isn't derivable from `value` alone (a source callable's kind lives in
/// the resolver, an intrinsic's in the registry). The caller passes `callable_kind`
/// = `Some(kind)` **iff** `value` is a callable — see [`callable_kind_of`] — so those
/// arms read it rather than `value`.
pub(crate) fn value_is(value: Value, ty: BuiltinType, callable_kind: Option<BodyKind>) -> bool {
    match ty {
        BuiltinType::Int => matches!(value, Value::Int(_) | Value::BigInt(_)),
        BuiltinType::Float => matches!(value, Value::Float(_)),
        BuiltinType::Number => {
            matches!(value, Value::Int(_) | Value::BigInt(_) | Value::Float(_))
        }
        BuiltinType::Bool => matches!(value, Value::Bool(_)),
        BuiltinType::String => matches!(value, Value::Str(_)),
        BuiltinType::Bytes => matches!(value, Value::Bytes(_)),
        BuiltinType::Nil => matches!(value, Value::Nil),
        BuiltinType::List => matches!(value, Value::List(_)),
        BuiltinType::Dict => matches!(value, Value::Dict(_)),
        BuiltinType::Callable => callable_kind.is_some(),
        BuiltinType::Procedure => callable_kind == Some(BodyKind::Proc),
        BuiltinType::Function => callable_kind == Some(BodyKind::Func),
    }
}

/// The `to`/`fn` kind of a callable **value**, or `None` if `value` is not callable.
/// A callable value is always a `to` ([`BodyKind::Proc`]) or an `fn`
/// ([`BodyKind::Func`]) — a block is not a value and the module top level is not a
/// callable value — so the result is never `Block`/`ModuleTopLevel`. A foreign
/// (intrinsic) callable classifies by its declared descriptor (S-42-lite): a
/// value-yielding intrinsic is a `Function`, a void one a `Procedure` (so `print` is
/// a `Procedure`), consistent with the `to`/`fn` distinction everywhere else.
fn callable_kind_of(
    value: Value,
    heap: &Heap,
    resolved: &ResolvedModule,
    protocols: &super::protocol::Registry,
    intrinsics: &Registry,
) -> Option<BodyKind> {
    let Value::Callable(idx) = value else {
        return None;
    };
    let kind = match heap.callable(idx).target {
        CallableTarget::Source(id) => resolved.callables[id as usize].kind,
        CallableTarget::Intrinsic(iid) => intrinsics.kind_of(iid),
        // A bare protocol **dispatcher** value is a `Callable`; its `to`/`fn` split is the
        // member's declared kind (from its first declarer). A name declared by protocols of
        // differing kinds is inherently ambiguous — the first-declarer's kind is the
        // deterministic answer; `x is Callable` holds regardless (kind is `Some`).
        CallableTarget::Dispatcher { member, .. } => {
            protocols.member_kind(member).unwrap_or(BodyKind::Func)
        }
    };
    Some(kind)
}

/// Applies `lhs is rhs` (L§6.5): the right operand must be a **type value**; the
/// result is whether `lhs`'s type is that type. A non-type right operand raises
/// (protocol values — the other legal right operand — arrive at M5).
#[allow(clippy::too_many_arguments)]
pub(crate) fn is_op(
    lhs: Value,
    rhs: Value,
    heap: &Heap,
    resolved: &ResolvedModule,
    modules: &[super::LoadedModule],
    protocols: &super::protocol::Registry,
    intrinsics: &Registry,
    span: Span,
) -> Result<Value, Raise> {
    let Value::Type(idx) = rhs else {
        return Err(Raise::new(
            ExceptionKind::TypeMismatch,
            "the right side of `is` must be a type",
            span,
        ));
    };
    let matches = match &heap.type_value(idx).kind {
        TypeKind::Builtin(b) => value_is(
            lhs,
            *b,
            callable_kind_of(lhs, heap, resolved, protocols, intrinsics),
        ),
        // Records are **nominal** (L§6.5): `x is Point` holds iff `x` is a record whose
        // type is this exact declared type — compared by the type value's identity, so
        // two same-shaped records of different declarations are different types.
        TypeKind::Record(_) => {
            matches!(lhs, Value::Record(r) if heap.record(r).type_idx == idx)
        }
        // `x is P` for a protocol (L§6.5, §10.4): whether `x`'s runtime type implements `P`
        // (consults the registry — a registered `implement P for T`, plus the `extends`
        // chain). The two well-known protocols also count the engine's native default
        // (D-M5-1): every value is `Stringable` (the native renderer is total), and a value
        // is `Hashable` if it is natively hashable (a scalar or value record with hashable
        // fields — not a list/dict/`ref` record) even without an explicit `implement`.
        TypeKind::Protocol(pt) => {
            if protocols.is_stringable(pt.id) {
                true
            } else if protocols.is_hashable(pt.id) {
                let dt = super::protocol::dispatch_type_of(lhs, heap, modules, intrinsics);
                protocols.type_implements(dt, pt.id)
                    || super::hash::check_hashable(lhs, heap).is_ok()
            } else {
                let dt = super::protocol::dispatch_type_of(lhs, heap, modules, intrinsics);
                protocols.type_implements(dt, pt.id)
            }
        }
    };
    Ok(Value::Bool(matches))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::{BigIntIdx, CalIdx, ListIdx, StrIdx};

    #[test]
    fn int_matches_both_integer_representations_but_not_float() {
        assert!(value_is(Value::Int(3), BuiltinType::Int, None));
        assert!(value_is(
            Value::BigInt(BigIntIdx(0)),
            BuiltinType::Int,
            None
        ));
        assert!(!value_is(Value::Float(3.0), BuiltinType::Int, None));
    }

    #[test]
    fn number_matches_any_numeric_but_not_others() {
        assert!(value_is(Value::Int(1), BuiltinType::Number, None));
        assert!(value_is(
            Value::BigInt(BigIntIdx(0)),
            BuiltinType::Number,
            None
        ));
        assert!(value_is(Value::Float(1.0), BuiltinType::Number, None));
        assert!(!value_is(Value::Bool(true), BuiltinType::Number, None));
        assert!(!value_is(Value::Nil, BuiltinType::Number, None));
    }

    #[test]
    fn each_leaf_type_matches_only_its_own_kind() {
        assert!(value_is(Value::Bool(false), BuiltinType::Bool, None));
        assert!(value_is(Value::Nil, BuiltinType::Nil, None));
        assert!(value_is(Value::Str(StrIdx(0)), BuiltinType::String, None));
        assert!(value_is(Value::List(ListIdx(0)), BuiltinType::List, None));
        assert!(!value_is(Value::Nil, BuiltinType::Bool, None));
    }

    #[test]
    fn the_callable_trio_splits_procedures_from_functions() {
        let proc = Some(BodyKind::Proc);
        let func = Some(BodyKind::Func);
        // A `to` is a Procedure and a Callable, never a Function.
        assert!(value_is(
            Value::Callable(CalIdx(0)),
            BuiltinType::Procedure,
            proc
        ));
        assert!(value_is(
            Value::Callable(CalIdx(0)),
            BuiltinType::Callable,
            proc
        ));
        assert!(!value_is(
            Value::Callable(CalIdx(0)),
            BuiltinType::Function,
            proc
        ));
        // An `fn` is a Function and a Callable, never a Procedure.
        assert!(value_is(
            Value::Callable(CalIdx(0)),
            BuiltinType::Function,
            func
        ));
        assert!(value_is(
            Value::Callable(CalIdx(0)),
            BuiltinType::Callable,
            func
        ));
        assert!(!value_is(
            Value::Callable(CalIdx(0)),
            BuiltinType::Procedure,
            func
        ));
        // A non-callable is none of the trio.
        assert!(!value_is(
            Value::List(ListIdx(0)),
            BuiltinType::Callable,
            None
        ));
        assert!(!value_is(
            Value::List(ListIdx(0)),
            BuiltinType::Function,
            None
        ));
    }
}
