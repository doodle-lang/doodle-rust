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
use crate::heap::Heap;
use crate::span::Span;

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
    /// `Procedure` — any callable (whether `Procedure` distinguishes procedures
    /// from functions is provisional, L Appendix D; here it matches both).
    Procedure,
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
    ("Procedure", BuiltinType::Procedure),
];

/// Whether `value`'s type is `ty` (L§6.5 membership, built-in cases). Numbers:
/// `Int` matches an integer of either representation (the canonical-int invariant
/// keeps small integers as `Int` and larger ones as `BigInt`, MD §3), `Float`
/// matches a float, and `Number` matches either.
pub(crate) fn value_is(value: Value, ty: BuiltinType) -> bool {
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
        BuiltinType::Procedure => matches!(value, Value::Callable(_)),
    }
}

/// Applies `lhs is rhs` (L§6.5): the right operand must be a **type value**; the
/// result is whether `lhs`'s type is that type. A non-type right operand raises
/// (protocol values — the other legal right operand — arrive at M5).
pub(crate) fn is_op(lhs: Value, rhs: Value, heap: &Heap, span: Span) -> Result<Value, Raise> {
    let Value::Type(idx) = rhs else {
        return Err(Raise::new(
            ExceptionKind::TypeMismatch,
            "the right side of `is` must be a type",
            span,
        ));
    };
    let ty = heap.type_value(idx).builtin;
    Ok(Value::Bool(value_is(lhs, ty)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::{BigIntIdx, CalIdx, ListIdx, StrIdx};

    #[test]
    fn int_matches_both_integer_representations_but_not_float() {
        assert!(value_is(Value::Int(3), BuiltinType::Int));
        assert!(value_is(Value::BigInt(BigIntIdx(0)), BuiltinType::Int));
        assert!(!value_is(Value::Float(3.0), BuiltinType::Int));
    }

    #[test]
    fn number_matches_any_numeric_but_not_others() {
        assert!(value_is(Value::Int(1), BuiltinType::Number));
        assert!(value_is(Value::BigInt(BigIntIdx(0)), BuiltinType::Number));
        assert!(value_is(Value::Float(1.0), BuiltinType::Number));
        assert!(!value_is(Value::Bool(true), BuiltinType::Number));
        assert!(!value_is(Value::Nil, BuiltinType::Number));
    }

    #[test]
    fn each_leaf_type_matches_only_its_own_kind() {
        assert!(value_is(Value::Bool(false), BuiltinType::Bool));
        assert!(value_is(Value::Nil, BuiltinType::Nil));
        assert!(value_is(Value::Str(StrIdx(0)), BuiltinType::String));
        assert!(value_is(Value::List(ListIdx(0)), BuiltinType::List));
        assert!(value_is(Value::Callable(CalIdx(0)), BuiltinType::Procedure));
        assert!(!value_is(Value::Callable(CalIdx(0)), BuiltinType::List));
        assert!(!value_is(Value::Nil, BuiltinType::Bool));
    }
}
