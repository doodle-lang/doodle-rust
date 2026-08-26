//! Unit tests for the numeric tower and arithmetic operators (`arith.rs`). Split into a
//! sibling `tests.rs` so `arith.rs` stays within the hygiene length limit.

use super::*;

const S: Span = Span::DUMMY;

/// These tests exercise `binary`'s arithmetic; the R8 size guard it now consults needs a
/// `&mut Machine`, so route every call through a throwaway one with default (generous)
/// limits — shadowing the real [`super::binary`] so the call sites stay unchanged.
fn binary(
    op: BinaryOp,
    lhs: Value,
    rhs: Value,
    heap: &mut Heap,
    span: Span,
) -> Result<Value, Raise> {
    super::binary(op, lhs, rhs, heap, &mut Machine::for_test(), span)
}

fn big(n: &str) -> BigInt {
    n.parse().unwrap()
}

fn int_of(v: Value, heap: &Heap) -> BigInt {
    match v {
        Value::Int(i) => BigInt::from(i),
        Value::BigInt(idx) => heap.bigint(idx).value.clone(),
        other => panic!("expected an integer, got {other:?}"),
    }
}

/// Asserts `v` is a float bit-identical to `expected`. Bit comparison is
/// exact (avoiding clippy's float-`==` lint); the expected values are exactly
/// representable, so the operations under test are exact.
fn assert_float(v: Value, expected: f64) {
    match v {
        Value::Float(x) => assert_eq!(x.to_bits(), expected.to_bits()),
        other => panic!("expected a float, got {other:?}"),
    }
}

#[test]
fn int_arithmetic_stays_int_when_it_fits() {
    let mut h = Heap::new();
    let r = binary(BinaryOp::Add, Value::Int(2), Value::Int(3), &mut h, S).unwrap();
    assert!(matches!(r, Value::Int(5)));
}

#[test]
fn overflow_promotes_to_bigint_and_demotes_back() {
    let mut h = Heap::new();
    // i64::MAX + 1 does not fit i64 -> BigInt.
    let up = binary(
        BinaryOp::Add,
        Value::Int(i64::MAX),
        Value::Int(1),
        &mut h,
        S,
    )
    .unwrap();
    assert!(matches!(up, Value::BigInt(_)));
    assert_eq!(int_of(up, &h), big("9223372036854775808"));
    // ...and subtracting back down fits i64 again -> Int (canonical-int invariant).
    let down = binary(BinaryOp::Sub, up, Value::Int(1), &mut h, S).unwrap();
    assert!(matches!(down, Value::Int(i64::MAX)));
}

#[test]
fn mixed_int_float_widens_to_float() {
    let mut h = Heap::new();
    let r = binary(BinaryOp::Mul, Value::Int(2), Value::Float(1.5), &mut h, S).unwrap();
    assert_float(r, 3.0);
}

#[test]
fn division_always_yields_float() {
    let mut h = Heap::new();
    let r = binary(BinaryOp::Div, Value::Int(4), Value::Int(2), &mut h, S).unwrap();
    assert_float(r, 2.0);
}

#[test]
fn division_by_zero_raises() {
    let mut h = Heap::new();
    let e = binary(BinaryOp::Div, Value::Int(1), Value::Int(0), &mut h, S).unwrap_err();
    assert_eq!(e.exception.kind, ExceptionKind::DivisionByZero);
    let e = binary(BinaryOp::Rem, Value::Int(1), Value::Int(0), &mut h, S).unwrap_err();
    assert_eq!(e.exception.kind, ExceptionKind::DivisionByZero);
}

#[test]
fn floored_division_and_modulo_round_toward_negative_infinity() {
    let mut h = Heap::new();
    let q = binary(BinaryOp::FloorDiv, Value::Int(-7), Value::Int(2), &mut h, S).unwrap();
    assert_eq!(int_of(q, &h), big("-4"));
    let r = binary(BinaryOp::Rem, Value::Int(-7), Value::Int(2), &mut h, S).unwrap();
    assert_eq!(int_of(r, &h), big("1"));
}

#[test]
fn power_is_int_for_nonnegative_int_exponent_else_float() {
    let mut h = Heap::new();
    let i = binary(BinaryOp::Pow, Value::Int(2), Value::Int(10), &mut h, S).unwrap();
    assert_eq!(int_of(i, &h), big("1024"));
    let f = binary(BinaryOp::Pow, Value::Int(2), Value::Int(-1), &mut h, S).unwrap();
    assert_float(f, 0.5);
}

#[test]
fn power_with_a_small_base_ignores_a_huge_exponent() {
    // A base of magnitude <= 1 gives a trivial result whatever the exponent, so a huge
    // (u32-overflowing) exponent computes directly rather than faulting — including the old
    // `1 ** huge`, which once wrongly raised `exponent-too-large` (now retired).
    let mut h = Heap::new();
    let huge = int_value(big("10000000000"), &mut h); // 1e10 > u32::MAX, even
    let one = binary(BinaryOp::Pow, Value::Int(1), huge, &mut h, S).unwrap();
    assert_eq!(int_of(one, &h), big("1"));
    let zero = binary(BinaryOp::Pow, Value::Int(0), huge, &mut h, S).unwrap();
    assert_eq!(int_of(zero, &h), big("0"));
    let even = binary(BinaryOp::Pow, Value::Int(-1), huge, &mut h, S).unwrap();
    assert_eq!(int_of(even, &h), big("1"));
    let odd_exp = int_value(big("10000000001"), &mut h);
    let odd = binary(BinaryOp::Pow, Value::Int(-1), odd_exp, &mut h, S).unwrap();
    assert_eq!(int_of(odd, &h), big("-1"));
}

#[test]
fn float_power_is_deterministic_via_libm() {
    // The `**` float path uses the bundled `libm::pow` (not `f64::powf`), so this
    // exact-bit golden must hold identically on every target (E§11). `2 ** 3.5`
    // (Int base, Float exp) takes the float path (only `Int ** nonneg-Int` is exact).
    let mut h = Heap::new();
    // libm's `pow(2, 3.5)` is one ULP below 8·√2 (11.313708498984761) — the
    // not-correctly-rounded transcendental result, pinned exactly so a divergent
    // platform `pow` would fail here rather than silently.
    let f = binary(BinaryOp::Pow, Value::Int(2), Value::Float(3.5), &mut h, S).unwrap();
    assert_float(f, 11.31370849898476);
}

#[test]
fn nonfinite_float_result_raises() {
    let mut h = Heap::new();
    // 1e308 * 10 overflows binary64 -> +inf -> raises (S-56).
    let e = binary(
        BinaryOp::Mul,
        Value::Float(1e308),
        Value::Float(10.0),
        &mut h,
        S,
    )
    .unwrap_err();
    assert_eq!(e.exception.kind, ExceptionKind::NonFiniteFloat);
    // 0 ** -1 is +inf on the float path -> raises.
    let e = binary(BinaryOp::Pow, Value::Int(0), Value::Int(-1), &mut h, S).unwrap_err();
    assert_eq!(e.exception.kind, ExceptionKind::NonFiniteFloat);
}

#[test]
fn non_number_operand_is_a_type_error() {
    let mut h = Heap::new();
    let e = binary(BinaryOp::Add, Value::Int(1), Value::Bool(true), &mut h, S).unwrap_err();
    assert_eq!(e.exception.kind, ExceptionKind::TypeMismatch);
    let e = unary(UnaryOp::Neg, Value::Nil, &mut h, S).unwrap_err();
    assert_eq!(e.exception.kind, ExceptionKind::TypeMismatch);
}

#[test]
fn negation_promotes_i64_min() {
    let mut h = Heap::new();
    // -(i64::MIN) does not fit i64 -> BigInt.
    let r = unary(UnaryOp::Neg, Value::Int(i64::MIN), &mut h, S).unwrap();
    assert_eq!(int_of(r, &h), big("9223372036854775808"));
}

/// A `Value` for the (necessarily bignum) integer `10^exp`.
fn ten_pow(exp: u32, heap: &mut Heap) -> Value {
    int_value(Pow::pow(BigInt::from(10), exp), heap)
}

#[test]
fn int_to_float_widening_overflow_raises() {
    let mut h = Heap::new();
    let huge = ten_pow(400, &mut h); // 10^400 far exceeds f64::MAX (~1.8e308)
    assert!(matches!(huge, Value::BigInt(_)));
    // Widening 10^400 to f64 is +inf, so any float-producing op over it raises
    // (the headline S-56 "widening included" case), whichever operand it is.
    for op in [BinaryOp::Mul, BinaryOp::Div, BinaryOp::FloorDiv] {
        let e = binary(op, huge, Value::Float(1.0), &mut h, S).unwrap_err();
        assert_eq!(e.exception.kind, ExceptionKind::NonFiniteFloat);
    }
}

#[test]
fn mixed_type_floored_division_and_modulo_widen_to_float() {
    let mut h = Heap::new();
    // A Float operand forces the floored *float* path (L§4.2 widening).
    let r = binary(BinaryOp::Rem, Value::Float(-7.0), Value::Int(2), &mut h, S).unwrap();
    assert_float(r, 1.0);
    let q = binary(
        BinaryOp::FloorDiv,
        Value::Int(-7),
        Value::Float(2.0),
        &mut h,
        S,
    )
    .unwrap();
    assert_float(q, -4.0);
}

#[test]
fn floored_ops_follow_the_divisor_sign() {
    let mut h = Heap::new();
    // With a negative divisor the remainder is negative (floored, not
    // truncated): 7 // -2 == -4, 7 % -2 == -1; -7 // -2 == 3, -7 % -2 == -1.
    for (x, y, fq, fr) in [(7i64, -2i64, -4i64, -1i64), (-7, -2, 3, -1)] {
        let q = binary(BinaryOp::FloorDiv, Value::Int(x), Value::Int(y), &mut h, S).unwrap();
        assert_eq!(int_of(q, &h), BigInt::from(fq));
        let r = binary(BinaryOp::Rem, Value::Int(x), Value::Int(y), &mut h, S).unwrap();
        assert_eq!(int_of(r, &h), BigInt::from(fr));
    }
}

#[test]
fn float_division_and_modulo_by_zero_raises() {
    let mut h = Heap::new();
    let e = binary(
        BinaryOp::Div,
        Value::Float(1.0),
        Value::Float(0.0),
        &mut h,
        S,
    )
    .unwrap_err();
    assert_eq!(e.exception.kind, ExceptionKind::DivisionByZero);
    // Negative zero is still zero.
    let e = binary(
        BinaryOp::Rem,
        Value::Float(1.0),
        Value::Float(-0.0),
        &mut h,
        S,
    )
    .unwrap_err();
    assert_eq!(e.exception.kind, ExceptionKind::DivisionByZero);
}

#[test]
fn float_floored_modulo_is_robust_at_large_magnitude() {
    // 1e16 = 3 * 3333333333333333 + 1, so the remainder is exactly 1.0. The
    // naive `x - y*(x/y).floor()` cancels to 0.0 here; `fmod` gets it right.
    let mut h = Heap::new();
    let r = binary(
        BinaryOp::Rem,
        Value::Float(1e16),
        Value::Float(3.0),
        &mut h,
        S,
    )
    .unwrap();
    assert_float(r, 1.0);
}

#[test]
fn unary_ops_reject_a_nonfinite_float() {
    // A nonfinite float can't arise from arithmetic (S-56) or a literal, but a
    // host may inject one (M2b); both unary `+` and `-` must still catch it.
    let mut h = Heap::new();
    let e = unary(UnaryOp::Pos, Value::Float(f64::INFINITY), &mut h, S).unwrap_err();
    assert_eq!(e.exception.kind, ExceptionKind::NonFiniteFloat);
    let e = unary(UnaryOp::Neg, Value::Float(f64::INFINITY), &mut h, S).unwrap_err();
    assert_eq!(e.exception.kind, ExceptionKind::NonFiniteFloat);
}
