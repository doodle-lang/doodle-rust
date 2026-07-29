//! Comparison, equality, and boolean operators (L§4.13, L§6.6; S-28).
//!
//! **Equality** (`==`/`!=`) is structural and **total** — it never raises and is
//! reflexive (L§6.6). Numbers compare by **exact mathematical value** across
//! `Int`/`Float`, never by lossy int→float widening (S-28): e.g. `Int(2^53 + 1)`
//! is `!=` and `>` the `Float` `2^53`, which a `to_f64` round would wrongly call
//! equal. The two IEEE special cases follow the value principle: `-0.0 == 0.0`
//! (both zero), and the single canonical NaN (E§4.3) equals itself.
//!
//! **Ordering** (`< > <= >=`) is defined for numbers and strings only (L§6.6);
//! applying it elsewhere — a NaN operand, or a non-number/non-string — raises.
//!
//! **Boolean** `and`/`or` short-circuit and their operands must be `Bool`;
//! `not` likewise. (`and`/`or` short-circuit control lives in the `step` loop;
//! this module supplies the strict-`Bool` check and `not`.)
//!
//! **Scope (M2a.3b).** Numbers, bools, nil, bytes (all constructible now) and
//! strings (helper-tested; string *values* arrive with string construction).
//! Structural equality of lists/dicts/records is cycle-safe and lands at M4 —
//! those values are not constructible yet, so that arm is unreachable here.

use super::arith::{Num, as_num};
use super::error::{ExceptionKind, Raise};
use crate::ast::BinaryOp;
use crate::heap::Heap;
use crate::machine::Value;
use crate::span::Span;
use num_bigint::BigInt;
use std::cmp::Ordering;

/// Applies a **comparison or equality** operator (`== != < > <= >=`). Arithmetic
/// and logical operators do not reach here.
pub(crate) fn binary(
    op: BinaryOp,
    lhs: Value,
    rhs: Value,
    heap: &Heap,
    span: Span,
) -> Result<Value, Raise> {
    let result = match op {
        BinaryOp::Eq => equal(lhs, rhs, heap),
        BinaryOp::Ne => !equal(lhs, rhs, heap),
        BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Ge => {
            let ord = order(lhs, rhs, heap, span)?;
            match op {
                BinaryOp::Lt => ord == Ordering::Less,
                BinaryOp::Gt => ord == Ordering::Greater,
                BinaryOp::Le => ord != Ordering::Greater,
                BinaryOp::Ge => ord != Ordering::Less,
                _ => unreachable!(),
            }
        }
        _ => unreachable!("non-comparison op reached compare::binary: {op:?}"),
    };
    Ok(Value::Bool(result))
}

/// Boolean `not` (L§6.6): its operand must be a `Bool`.
pub(crate) fn not(v: Value, span: Span) -> Result<Value, Raise> {
    Ok(Value::Bool(!as_bool(v, "not", span)?))
}

/// Reads a value as a `Bool`, raising a type error otherwise (strict booleans,
/// no truthiness — L§4.3). Shared with the `and`/`or` short-circuit in `step`.
pub(crate) fn as_bool(v: Value, op: &str, span: Span) -> Result<bool, Raise> {
    match v {
        Value::Bool(b) => Ok(b),
        other => Err(Raise::new(
            ExceptionKind::TypeMismatch,
            format!("`{op}` needs true or false, not {}", kind_name(other)),
            span,
        )),
    }
}

/// Structural equality (L§4.13) — total and reflexive, never raises.
pub(crate) fn equal(a: Value, b: Value, heap: &Heap) -> bool {
    // Numbers compare across kinds by exact value (S-28).
    if let (Some(na), Some(nb)) = (as_num(a, heap), as_num(b, heap)) {
        return numeric_equal(&na, &nb);
    }
    match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Nil, Value::Nil) => true,
        (Value::Bytes(x), Value::Bytes(y)) => {
            heap.byte_string(x).bytes == heap.byte_string(y).bytes
        }
        // Strings are stored NFC (L§4.4), so canonical equivalence is a content
        // comparison of the normalized bytes.
        (Value::Str(x), Value::Str(y)) => heap.string(x).utf8 == heap.string(y).utf8,
        // Reference-identity kinds (L§4.9): equal iff the same object.
        (Value::Callable(x), Value::Callable(y)) => x == y,
        (Value::Module(x), Value::Module(y)) => x == y,
        (Value::Type(x), Value::Type(y)) => x == y,
        (Value::Foreign(x), Value::Foreign(y)) => x == y,
        // Structural, cycle-safe equality of these is M4 — and they are not
        // constructible before then, so this arm is unreachable at M2a.3b.
        (Value::List(_), Value::List(_))
        | (Value::Dict(_), Value::Dict(_))
        | (Value::Record(_), Value::Record(_)) => {
            unimplemented!("structural equality of lists/dicts/records is M4")
        }
        // Different types (including a number vs a non-number) are never equal.
        _ => false,
    }
}

/// Total order of two values where ordering is defined (numbers, strings);
/// raises otherwise (L§6.6).
fn order(a: Value, b: Value, heap: &Heap, span: Span) -> Result<Ordering, Raise> {
    if let (Some(na), Some(nb)) = (as_num(a, heap), as_num(b, heap)) {
        // `None` means a NaN operand — ordering is undefined for NaN (L§6.6).
        return numeric_cmp(&na, &nb).ok_or_else(|| {
            Raise::new(
                ExceptionKind::UndefinedOrdering,
                "you can't compare with a NaN (it isn't a real number)",
                span,
            )
        });
    }
    if let (Value::Str(x), Value::Str(y)) = (a, b) {
        // Code-point-lexicographic over NFC = UTF-8 byte order (L§6.6).
        return Ok(heap
            .string(x)
            .utf8
            .as_bytes()
            .cmp(heap.string(y).utf8.as_bytes()));
    }
    Err(Raise::new(
        ExceptionKind::UndefinedOrdering,
        format!(
            "you can't compare {} with {} using < or >",
            kind_name(a),
            kind_name(b)
        ),
        span,
    ))
}

fn numeric_equal(a: &Num, b: &Num) -> bool {
    match (a, b) {
        (Num::Float(x), Num::Float(y)) => {
            // The single canonical NaN (E§4.3) equals itself; otherwise IEEE
            // equality via `partial_cmp` (Equal for `-0.0 == 0.0`, None for NaN),
            // which sidesteps clippy's float-`==` lint while being exact.
            (x.is_nan() && y.is_nan()) || x.partial_cmp(y) == Some(Ordering::Equal)
        }
        // Every other numeric pair is exactly ordered, so equality is Ordering::Equal.
        _ => numeric_cmp(a, b) == Some(Ordering::Equal),
    }
}

/// Exact numeric comparison across kinds. `None` iff a NaN operand makes the
/// order undefined.
fn numeric_cmp(a: &Num, b: &Num) -> Option<Ordering> {
    match (a, b) {
        (Num::Int(x), Num::Int(y)) => Some(x.cmp(y)),
        (Num::Float(x), Num::Float(y)) => x.partial_cmp(y),
        (Num::Int(n), Num::Float(x)) => cmp_int_float(n, *x),
        (Num::Float(x), Num::Int(n)) => cmp_int_float(n, *x).map(Ordering::reverse),
    }
}

/// Compares an exact integer to an `f64` **exactly** (S-28) — never via a lossy
/// widening of either side. `None` iff `x` is NaN.
///
/// A finite `f64` is a dyadic rational `mantissa * 2^exp`; the comparison is done
/// in exact `BigInt` arithmetic by scaling the integer side by the same power of
/// two, so magnitudes beyond `f64`'s 53-bit exact range compare correctly.
fn cmp_int_float(n: &BigInt, x: f64) -> Option<Ordering> {
    if x.is_nan() {
        return None;
    }
    if x.is_infinite() {
        // Any finite integer is between −∞ and +∞.
        return Some(if x > 0.0 {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    let (mant, exp) = decompose(x);
    Some(if exp >= 0 {
        // x is the integer `mant << exp`.
        n.cmp(&(mant << exp as usize))
    } else {
        // n vs mant / 2^|exp|  ⇔  n * 2^|exp| vs mant.
        (n << (-exp) as usize).cmp(&mant)
    })
}

/// Decomposes a finite `f64` into `(mantissa, exp)` with `x == mantissa * 2^exp`
/// exactly (signed mantissa). `±0.0` gives mantissa `0`.
fn decompose(x: f64) -> (BigInt, i32) {
    let bits = x.to_bits();
    let negative = bits >> 63 == 1;
    let biased_exp = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & 0x000f_ffff_ffff_ffff;
    let (significand, exp) = if biased_exp == 0 {
        // Subnormal (and zero): value = frac * 2^(−1074).
        (frac, -1074)
    } else {
        // Normal: value = (2^52 + frac) * 2^(biased_exp − 1075).
        (frac | (1u64 << 52), biased_exp - 1075)
    };
    let mag = BigInt::from(significand);
    (if negative { -mag } else { mag }, exp)
}

fn kind_name(v: Value) -> &'static str {
    match v {
        Value::Nil => "nil",
        Value::Bool(_) => "a boolean",
        Value::Int(_) | Value::BigInt(_) => "an integer",
        Value::Float(_) => "a float",
        Value::Str(_) => "a string",
        Value::Bytes(_) => "a byte string",
        Value::List(_) => "a list",
        Value::Dict(_) => "a dict",
        Value::Record(_) => "a record",
        Value::Callable(_) => "a procedure or function",
        Value::Module(_) => "a module",
        Value::Type(_) => "a type",
        Value::Foreign(_) => "a foreign value",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::arith::int_value;

    const S: Span = Span::DUMMY;

    fn int(n: &str, heap: &mut Heap) -> Value {
        int_value(n.parse().unwrap(), heap)
    }

    fn eq(a: Value, b: Value, heap: &Heap) -> bool {
        equal(a, b, heap)
    }

    fn ord(a: Value, b: Value, heap: &Heap) -> Ordering {
        order(a, b, heap, S).expect("ordering should be defined")
    }

    #[test]
    fn same_type_equality_by_content() {
        let mut h = Heap::new();
        assert!(eq(Value::Bool(true), Value::Bool(true), &h));
        assert!(!eq(Value::Bool(true), Value::Bool(false), &h));
        assert!(eq(Value::Nil, Value::Nil, &h));
        let a = h.alloc_bytes(vec![1, 2].into());
        let b = h.alloc_bytes(vec![1, 2].into());
        let c = h.alloc_bytes(vec![1, 3].into());
        assert!(eq(Value::Bytes(a), Value::Bytes(b), &h));
        assert!(!eq(Value::Bytes(a), Value::Bytes(c), &h));
    }

    #[test]
    fn different_types_are_never_equal_and_never_raise() {
        let h = Heap::new();
        assert!(!eq(Value::Int(1), Value::Bool(true), &h));
        assert!(!eq(Value::Nil, Value::Int(0), &h));
        assert!(!eq(Value::Bool(false), Value::Nil, &h));
    }

    #[test]
    fn cross_kind_numeric_equality_is_exact() {
        let mut h = Heap::new();
        assert!(eq(Value::Int(1), Value::Float(1.0), &h));
        assert!(!eq(Value::Int(1), Value::Float(1.5), &h));
        // -0.0 == 0.0 == 0 (all three directions).
        assert!(eq(Value::Float(-0.0), Value::Float(0.0), &h));
        assert!(eq(Value::Int(0), Value::Float(-0.0), &h));
        assert!(eq(Value::Float(-0.0), Value::Int(0), &h));
        // Beyond 2^53, exact — a lossy widen would call these equal.
        let big = int("9007199254740993", &mut h); // 2^53 + 1
        let two53 = Value::Float(9007199254740992.0); // 2^53 (exact f64)
        assert!(!eq(big, two53, &h));
        assert!(eq(int("9007199254740992", &mut h), two53, &h)); // 2^53 == 2^53
    }

    #[test]
    fn bigint_vs_float_equality_is_exact() {
        let mut h = Heap::new();
        // 2^70 is exactly representable; 2^70 + 1 is not, and is strictly larger.
        let p70 = int("1180591620717411303424", &mut h); // 2^70
        let p70f = Value::Float(1180591620717411303424.0);
        assert!(eq(p70, p70f, &h));
        let p70_1 = int("1180591620717411303425", &mut h); // 2^70 + 1
        assert!(!eq(p70_1, p70f, &h));
        assert_eq!(ord(p70_1, p70f, &h), Ordering::Greater);
    }

    #[test]
    fn nan_equals_only_itself_and_never_raises() {
        let h = Heap::new();
        let nan = Value::Float(f64::NAN);
        assert!(eq(nan, nan, &h)); // reflexive (S-28)
        assert!(!eq(nan, Value::Float(1.0), &h));
        assert!(!eq(nan, Value::Int(1), &h));
        assert!(!eq(Value::Int(1), nan, &h));
    }

    #[test]
    fn ordering_of_numbers_including_cross_kind() {
        let mut h = Heap::new();
        assert_eq!(ord(Value::Int(1), Value::Int(2), &h), Ordering::Less);
        assert_eq!(ord(Value::Int(1), Value::Float(1.5), &h), Ordering::Less);
        assert_eq!(ord(Value::Float(2.0), Value::Int(1), &h), Ordering::Greater);
        assert_eq!(ord(Value::Int(5), int("5", &mut h), &h), Ordering::Equal);
        // Neither zero is less than the other.
        assert_eq!(
            ord(Value::Float(-0.0), Value::Float(0.0), &h),
            Ordering::Equal
        );
    }

    #[test]
    fn ordering_a_nan_raises() {
        let h = Heap::new();
        let nan = Value::Float(f64::NAN);
        let e = order(nan, Value::Int(1), &h, S).unwrap_err();
        assert_eq!(e.exception.kind, ExceptionKind::UndefinedOrdering);
        let e = order(Value::Float(1.0), nan, &h, S).unwrap_err();
        assert_eq!(e.exception.kind, ExceptionKind::UndefinedOrdering);
    }

    #[test]
    fn ordering_non_numbers_raises() {
        let mut h = Heap::new();
        let e = order(Value::Bool(true), Value::Bool(false), &h, S).unwrap_err();
        assert_eq!(e.exception.kind, ExceptionKind::UndefinedOrdering);
        let bytes = Value::Bytes(h.alloc_bytes(vec![1].into()));
        let e = order(bytes, bytes, &h, S).unwrap_err();
        assert_eq!(e.exception.kind, ExceptionKind::UndefinedOrdering);
        // A number vs a non-number is also undefined.
        let e = order(Value::Int(1), Value::Nil, &h, S).unwrap_err();
        assert_eq!(e.exception.kind, ExceptionKind::UndefinedOrdering);
    }

    #[test]
    fn strings_compare_by_content_and_code_point_order() {
        // String values are stored NFC by construction, so equality is a content
        // compare of the normalized bytes and ordering is UTF-8 byte (= code
        // point) order.
        let mut h = Heap::new();
        let a = Value::Str(h.alloc_string("caf\u{e9}".into())); // "café" (NFC)
        let b = Value::Str(h.alloc_string("caf\u{e9}".into()));
        assert!(eq(a, b, &h));
        let c = Value::Str(h.alloc_string("cafe".into()));
        assert!(!eq(a, c, &h));
        let apple = Value::Str(h.alloc_string("apple".into()));
        let banana = Value::Str(h.alloc_string("banana".into()));
        assert_eq!(ord(apple, banana, &h), Ordering::Less);
    }

    #[test]
    fn not_negates_booleans_and_rejects_others() {
        assert!(matches!(not(Value::Bool(true), S), Ok(Value::Bool(false))));
        assert!(matches!(not(Value::Bool(false), S), Ok(Value::Bool(true))));
        let e = not(Value::Int(1), S).unwrap_err();
        assert_eq!(e.exception.kind, ExceptionKind::TypeMismatch);
    }

    #[test]
    fn comparison_operators_return_booleans() {
        let h = Heap::new();
        assert!(matches!(
            binary(BinaryOp::Lt, Value::Int(1), Value::Int(2), &h, S),
            Ok(Value::Bool(true))
        ));
        assert!(matches!(
            binary(BinaryOp::Ge, Value::Int(1), Value::Int(2), &h, S),
            Ok(Value::Bool(false))
        ));
        assert!(matches!(
            binary(BinaryOp::Ne, Value::Int(1), Value::Float(1.0), &h, S),
            Ok(Value::Bool(false))
        ));
    }
}
