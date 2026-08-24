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
//! **Scope.** Numbers, bools, nil, bytes, strings, and the reference-identity
//! kinds compare directly. **Lists** compare structurally and **cycle-safely**
//! (M4.0). Dicts and records join the same cycle-safe walk at M4.4 (they are not
//! constructible before M4.1/M4.2, so their arm is currently unreachable).

use super::arith::{Num, as_num};
use super::error::{ExceptionKind, Raise};
use crate::ast::BinaryOp;
use crate::heap::Heap;
use crate::machine::{ListIdx, Value};
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

/// Structural equality (L§4.13) — total and reflexive, never raises. Aggregates
/// (lists now; dicts/records at M4.4) compare by structure, cycle-safely.
pub(crate) fn equal(a: Value, b: Value, heap: &Heap) -> bool {
    equal_rec(a, b, heap, &mut Vec::new())
}

/// The structural walk. `in_progress` holds the list-index pairs currently being
/// compared on this path; **re-meeting a pair means a cycle**, and unrolling the
/// two structures would agree forever, so the pair is equal co-inductively. This
/// makes equality terminate on self-referential structures (reachable once
/// mutation/place-chains land at M4.3). The pair stack is a plain `Vec` — no
/// hasher — so nothing here can perturb determinism.
fn equal_rec(a: Value, b: Value, heap: &Heap, in_progress: &mut Vec<(ListIdx, ListIdx)>) -> bool {
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
        (Value::List(x), Value::List(y)) => list_equal(x, y, heap, in_progress),
        // Dicts and records join this walk at M4.4; not constructible before
        // M4.1/M4.2, so this arm is currently unreachable.
        (Value::Dict(_), Value::Dict(_)) | (Value::Record(_), Value::Record(_)) => {
            unimplemented!("structural equality of dicts/records is M4.4")
        }
        // Different types (including a number vs a non-number) are never equal.
        _ => false,
    }
}

/// Structural equality of two lists (L§4.6/§4.13): same length and pairwise-equal
/// elements, walked cycle-safely via `in_progress`.
fn list_equal(
    x: ListIdx,
    y: ListIdx,
    heap: &Heap,
    in_progress: &mut Vec<(ListIdx, ListIdx)>,
) -> bool {
    // The same list object is equal to itself without a walk — also the base case
    // that terminates a list reachable from its own elements.
    if x == y {
        return true;
    }
    // This exact pair is already being compared further up the walk: a cycle.
    if in_progress.contains(&(x, y)) {
        return true;
    }
    let xs = &heap.list(x).items;
    let ys = &heap.list(y).items;
    if xs.len() != ys.len() {
        return false;
    }
    in_progress.push((x, y));
    let equal = xs
        .iter()
        .zip(ys.iter())
        .all(|(&ea, &eb)| equal_rec(ea, eb, heap, in_progress));
    in_progress.pop();
    equal
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
/// exactly (signed mantissa). `±0.0` gives mantissa `0`. Shared with value hashing
/// (`super::hash`), which needs the same exact integer value a float compares as.
pub(super) fn decompose(x: f64) -> (BigInt, i32) {
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

pub(super) fn kind_name(v: Value) -> &'static str {
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

    fn list(items: Vec<Value>, h: &mut Heap) -> Value {
        Value::List(h.alloc_list(items))
    }

    #[test]
    fn lists_compare_structurally() {
        let mut h = Heap::new();
        let one = int("1", &mut h);
        let two = int("2", &mut h);
        let three = int("3", &mut h);
        // Same length, pairwise-equal elements.
        let a = list(vec![one, two, three], &mut h);
        let b = list(vec![one, two, three], &mut h);
        assert!(eq(a, b, &h));
        // A differing element.
        let l1 = list(vec![one], &mut h);
        let l2 = list(vec![two], &mut h);
        assert!(!eq(l1, l2, &h));
        // Different lengths.
        let l12 = list(vec![one, two], &mut h);
        assert!(!eq(l12, l1, &h));
        // Empty lists are equal.
        let e1 = list(vec![], &mut h);
        let e2 = list(vec![], &mut h);
        assert!(eq(e1, e2, &h));
        // Nested, and element equality recurses.
        let inner1 = list(vec![one], &mut h);
        let inner23 = list(vec![two, three], &mut h);
        let nested_a = list(vec![inner1, inner23], &mut h);
        let inner1b = list(vec![one], &mut h);
        let inner23b = list(vec![two, three], &mut h);
        let nested_b = list(vec![inner1b, inner23b], &mut h);
        assert!(eq(nested_a, nested_b, &h));
        // Elements compare cross-kind by exact value (S-28) through the walk.
        let li = list(vec![one], &mut h);
        let lf = list(vec![Value::Float(1.0)], &mut h);
        assert!(eq(li, lf, &h));
        // A list is never equal to a non-list, and never raises.
        assert!(!eq(l1, one, &h));
        assert!(!eq(l1, Value::Nil, &h));
    }

    #[test]
    fn list_equality_is_cycle_safe() {
        let mut h = Heap::new();
        // `a = [a]` and `b = [b]` — self-referential lists (source can't build these
        // until M4.3's mutation, but the heap can). Equality must terminate.
        let ai = h.alloc_list(vec![]);
        h.list_push(ai, Value::List(ai));
        let bi = h.alloc_list(vec![]);
        h.list_push(bi, Value::List(bi));
        // The same cyclic object equals itself.
        assert!(eq(Value::List(ai), Value::List(ai), &h));
        // Two distinct one-cycle lists are structurally equal (co-induction) — and this
        // returns rather than looping.
        assert!(eq(Value::List(ai), Value::List(bi), &h));
        // `c = [1, c]` differs from `a = [a]` by length; still terminates.
        let one = int("1", &mut h);
        let ci = h.alloc_list(vec![one]);
        h.list_push(ci, Value::List(ci));
        assert!(!eq(Value::List(ai), Value::List(ci), &h));
    }
}
