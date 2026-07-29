//! The numeric tower and arithmetic operators (L§4.2, L§6.5/§6.6).
//!
//! Integer arithmetic is exact and arbitrary-precision: an `Int` that overflows
//! `i64` **promotes** to a heap bignum, and any result fitting `i64` **demotes**
//! back — the canonical-int invariant (MD §3), so a value fitting `i64` is always
//! a [`Value::Int`]. Mixed `Int`/`Float` widens the integer to `f64`. Every
//! `Float`-producing path enforces the **finite-float invariant** (S-56, L§4.2):
//! a result (including an integer→float widening) that is ±∞ or NaN raises rather
//! than yielding a nonfinite value. Division/modulo by zero raises (L§4.2).
//!
//! **Scope (M2a.3a).** Arithmetic (`+ - * / // % **`) and unary `- +`. The
//! comparison, equality, and logical operators — and unary `not` — are M2a.3b;
//! routing one of those here is a bug (`unreachable!`).

use crate::ast::{BinaryOp, UnaryOp};
use crate::heap::Heap;
use crate::machine::Value;
use crate::machine::error::{ExceptionKind, Raise};
use crate::span::Span;
use num_bigint::BigInt;
use num_integer::Integer;
use num_traits::{Pow, Signed, ToPrimitive, Zero};

/// A numeric operand resolved for computation: an exact integer or a float.
enum Num {
    Int(BigInt),
    Float(f64),
}

/// Applies an **arithmetic** binary operator. Comparison/equality/logical ops do
/// not reach here (M2a.3b handles them).
pub(crate) fn binary(
    op: BinaryOp,
    lhs: Value,
    rhs: Value,
    heap: &mut Heap,
    span: Span,
) -> Result<Value, Raise> {
    let a = as_num(lhs, heap).ok_or_else(|| type_error(op, span))?;
    let b = as_num(rhs, heap).ok_or_else(|| type_error(op, span))?;
    match op {
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul => add_sub_mul(op, a, b, heap, span),
        BinaryOp::Div => divide(a, b, span),
        BinaryOp::FloorDiv | BinaryOp::Rem => floor_div_rem(op, a, b, heap, span),
        BinaryOp::Pow => power(a, b, heap, span),
        _ => unreachable!("non-arithmetic binary op reached arith::binary: {op:?}"),
    }
}

/// Applies a **numeric** unary operator (`-`/`+`). `not` is M2a.3b.
pub(crate) fn unary(op: UnaryOp, v: Value, heap: &mut Heap, span: Span) -> Result<Value, Raise> {
    match op {
        UnaryOp::Neg => match as_num(v, heap).ok_or_else(|| unary_type_error("-", span))? {
            Num::Int(i) => Ok(int_value(-i, heap)),
            Num::Float(x) => finite(-x, span),
        },
        // `+` is identity on a number, but its float result still obeys the
        // finite-float invariant (S-56), matching `-`: Int/BigInt pass through
        // (already canonical), a float is finiteness-checked, non-numbers raise.
        UnaryOp::Pos => match v {
            Value::Int(_) | Value::BigInt(_) => Ok(v),
            Value::Float(x) => finite(x, span),
            _ => Err(unary_type_error("+", span)),
        },
        UnaryOp::Not => unreachable!("`not` reaches arith::unary; it is handled in M2a.3b"),
    }
}

/// Normalizes an exact integer to a `Value`, upholding the canonical-int
/// invariant (MD §3): a magnitude fitting `i64` is an `Int`, else a heap bignum.
pub(crate) fn int_value(n: BigInt, heap: &mut Heap) -> Value {
    match n.to_i64() {
        Some(i) => Value::Int(i),
        None => Value::BigInt(heap.alloc_bigint(n)),
    }
}

/// Reads a value as a numeric operand, cloning a bignum's magnitude; non-numbers
/// return `None` (the caller raises a type error).
fn as_num(v: Value, heap: &Heap) -> Option<Num> {
    match v {
        Value::Int(n) => Some(Num::Int(BigInt::from(n))),
        Value::BigInt(idx) => Some(Num::Int(heap.bigint(idx).value.clone())),
        Value::Float(x) => Some(Num::Float(x)),
        _ => None,
    }
}

fn add_sub_mul(op: BinaryOp, a: Num, b: Num, heap: &mut Heap, span: Span) -> Result<Value, Raise> {
    match (a, b) {
        (Num::Int(x), Num::Int(y)) => {
            let r = match op {
                BinaryOp::Add => x + y,
                BinaryOp::Sub => x - y,
                BinaryOp::Mul => x * y,
                _ => unreachable!(),
            };
            Ok(int_value(r, heap))
        }
        (a, b) => {
            let x = num_to_f64(a, span)?;
            let y = num_to_f64(b, span)?;
            let r = match op {
                BinaryOp::Add => x + y,
                BinaryOp::Sub => x - y,
                BinaryOp::Mul => x * y,
                _ => unreachable!(),
            };
            finite(r, span)
        }
    }
}

/// `/` always yields a `Float` (L§4.2); division by zero raises.
fn divide(a: Num, b: Num, span: Span) -> Result<Value, Raise> {
    if is_zero(&b) {
        return Err(div_by_zero(span));
    }
    let x = num_to_f64(a, span)?;
    let y = num_to_f64(b, span)?;
    finite(x / y, span)
}

/// `//` and `%` follow **floored** semantics on operands of the same numeric
/// type after widening (L§4.2); division by zero raises.
fn floor_div_rem(
    op: BinaryOp,
    a: Num,
    b: Num,
    heap: &mut Heap,
    span: Span,
) -> Result<Value, Raise> {
    if is_zero(&b) {
        return Err(div_by_zero(span));
    }
    match (a, b) {
        (Num::Int(x), Num::Int(y)) => {
            let r = match op {
                BinaryOp::FloorDiv => x.div_floor(&y),
                BinaryOp::Rem => x.mod_floor(&y),
                _ => unreachable!(),
            };
            Ok(int_value(r, heap))
        }
        (a, b) => {
            let x = num_to_f64(a, span)?;
            let y = num_to_f64(b, span)?;
            let result = match op {
                BinaryOp::FloorDiv => (x / y).floor(),
                // Floored remainder: take the truncated remainder from `fmod`
                // (`%`, computed exactly by hardware) and adjust its sign to
                // follow the divisor. This is robust at large magnitudes, where
                // `x - y*(x/y).floor()` can cancel catastrophically (e.g.
                // `1e16 % 3.0` would round to `0.0` instead of `1.0`).
                BinaryOp::Rem => {
                    let m = x % y;
                    if m != 0.0 && (m < 0.0) != (y < 0.0) {
                        m + y
                    } else {
                        m
                    }
                }
                _ => unreachable!(),
            };
            finite(result, span)
        }
    }
}

/// `**`: `Int ** non-negative Int` is an exact `Int`; every other case is a
/// `Float` (L§4.2). `0 ** negative` yields ∞ on the float path, so it raises
/// (S-56) — the division-by-zero analog.
fn power(a: Num, b: Num, heap: &mut Heap, span: Span) -> Result<Value, Raise> {
    match (a, b) {
        (Num::Int(base), Num::Int(exp)) if !exp.is_negative() => {
            let e = exp.to_u32().ok_or_else(|| exponent_too_large(span))?;
            Ok(int_value(Pow::pow(base, e), heap))
        }
        (a, b) => {
            let x = num_to_f64(a, span)?;
            let y = num_to_f64(b, span)?;
            finite(x.powf(y), span)
        }
    }
}

/// Widens a numeric operand to `f64`, raising if an integer's magnitude rounds
/// to ±∞ (the widening is itself a nonfinite result — L§4.2). A `Float` operand
/// passes through unchecked: a *finite* result computed from a (host-injected)
/// nonfinite operand is allowed, and the caller's result check catches the rest.
fn num_to_f64(n: Num, span: Span) -> Result<f64, Raise> {
    match n {
        Num::Int(i) => match i.to_f64() {
            Some(x) if x.is_finite() => Ok(x),
            _ => Err(nonfinite(span)),
        },
        Num::Float(x) => Ok(x),
    }
}

/// Wraps a float result, enforcing the finite-float invariant (S-56).
fn finite(x: f64, span: Span) -> Result<Value, Raise> {
    if x.is_finite() {
        Ok(Value::Float(x))
    } else {
        Err(nonfinite(span))
    }
}

fn is_zero(n: &Num) -> bool {
    match n {
        Num::Int(i) => i.is_zero(),
        // Exact zero, matching both `+0.0` and `-0.0`.
        Num::Float(x) => *x == 0.0,
    }
}

fn div_by_zero(span: Span) -> Raise {
    Raise::new(
        ExceptionKind::DivisionByZero,
        "you can't divide by zero",
        span,
    )
}

fn nonfinite(span: Span) -> Raise {
    Raise::new(
        ExceptionKind::NonFiniteFloat,
        "that number got too big to be a real number",
        span,
    )
}

fn exponent_too_large(span: Span) -> Raise {
    Raise::new(
        ExceptionKind::ExponentTooLarge,
        "that power is too big to work out",
        span,
    )
}

fn type_error(op: BinaryOp, span: Span) -> Raise {
    Raise::new(
        ExceptionKind::TypeMismatch,
        format!("`{}` needs two numbers", binary_symbol(op)),
        span,
    )
}

fn unary_type_error(sym: &str, span: Span) -> Raise {
    Raise::new(
        ExceptionKind::TypeMismatch,
        format!("`{sym}` needs a number"),
        span,
    )
}

fn binary_symbol(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::FloorDiv => "//",
        BinaryOp::Rem => "%",
        BinaryOp::Pow => "**",
        _ => unreachable!("non-arithmetic op in binary_symbol: {op:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Span = Span::DUMMY;

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
}
