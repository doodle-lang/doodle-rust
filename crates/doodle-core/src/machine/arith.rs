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
use crate::machine::error::{ExceptionKind, Raise};
use crate::machine::{Machine, Value};
use crate::span::Span;
use num_bigint::BigInt;
use num_integer::Integer;
use num_traits::{Pow, Signed, ToPrimitive, Zero};

/// A numeric operand resolved for computation: an exact integer or a float.
/// Shared with [`super::compare`] (the comparison/equality operators).
pub(crate) enum Num {
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
    machine: &mut Machine,
    span: Span,
) -> Result<Value, Raise> {
    let a = as_num(lhs, heap).ok_or_else(|| type_error(op, lhs, heap, span))?;
    let b = as_num(rhs, heap).ok_or_else(|| type_error(op, rhs, heap, span))?;
    match op {
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul => add_sub_mul(op, a, b, heap, machine, span),
        BinaryOp::Div => divide(a, b, span),
        BinaryOp::FloorDiv | BinaryOp::Rem => floor_div_rem(op, a, b, heap, span),
        BinaryOp::Pow => power(a, b, heap, machine, span),
        _ => unreachable!("non-arithmetic binary op reached arith::binary: {op:?}"),
    }
}

/// Applies a **numeric** unary operator (`-`/`+`). `not` is M2a.3b.
pub(crate) fn unary(op: UnaryOp, v: Value, heap: &mut Heap, span: Span) -> Result<Value, Raise> {
    match op {
        UnaryOp::Neg => {
            match as_num(v, heap).ok_or_else(|| unary_type_error("-", v, heap, span))? {
                Num::Int(i) => Ok(int_value(-i, heap)),
                Num::Float(x) => finite(-x, span),
            }
        }
        // `+` is identity on a number, but its float result still obeys the
        // finite-float invariant (S-56), matching `-`: Int/BigInt pass through
        // (already canonical), a float is finiteness-checked, non-numbers raise.
        UnaryOp::Pos => match v {
            Value::Int(_) | Value::BigInt(_) => Ok(v),
            Value::Float(x) => finite(x, span),
            _ => Err(unary_type_error("+", v, heap, span)),
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
pub(crate) fn as_num(v: Value, heap: &Heap) -> Option<Num> {
    match v {
        Value::Int(n) => Some(Num::Int(BigInt::from(n))),
        Value::BigInt(idx) => Some(Num::Int(heap.bigint(idx).value.clone())),
        Value::Float(x) => Some(Num::Float(x)),
        _ => None,
    }
}

fn add_sub_mul(
    op: BinaryOp,
    a: Num,
    b: Num,
    heap: &mut Heap,
    machine: &mut Machine,
    span: Span,
) -> Result<Value, Raise> {
    match (a, b) {
        (Num::Int(x), Num::Int(y)) => {
            let r = match op {
                BinaryOp::Add => x + y,
                BinaryOp::Sub => x - y,
                // R8: bound the product's size before multiplying — a bignum `*` is
                // otherwise one unbounded, uninterruptible allocation (MD §9). The result
                // is at most `x.bits() + y.bits()` bits.
                BinaryOp::Mul => {
                    let est_bytes = (u128::from(x.bits()) + u128::from(y.bits())) / 8 + 1;
                    if !machine.admit_op_result(est_bytes) {
                        return Ok(Value::Nil); // placeholder; the parked fault surfaces first
                    }
                    x * y
                }
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
///
/// The float path computes `pow` with the deterministic bundled `libm`, **not**
/// `f64::powf`: `pow` is transcendental (not IEEE correctly-rounded), so the platform
/// math library differs in the last bit(s) across targets, which would break replay and
/// the cross-surface conformance gate (E§11, same reason as `sin`/`cos` — see
/// `intrinsic/builtins.rs`).
fn power(
    a: Num,
    b: Num,
    heap: &mut Heap,
    machine: &mut Machine,
    span: Span,
) -> Result<Value, Raise> {
    match (a, b) {
        (Num::Int(base), Num::Int(exp)) if !exp.is_negative() => {
            // A base of magnitude <= 1 yields 0, 1, or ±1 whatever the exponent, so even a
            // huge exponent is harmless — compute it directly.
            if base.bits() <= 1 {
                return Ok(int_value(small_base_pow(&base, &exp), heap));
            }
            // Otherwise the result grows with the exponent. Bound its size before computing
            // (R8): an exponent past the engine's computable range (u32), or a result over a
            // limit, is a magnitude *fault* (via `admit_op_result`), not a raise — the same
            // "too big" story as `"a" * 10**9` (a limit is a fault, uncatchable, E§10.2).
            if !machine.admit_op_result(pow_result_bytes(base.bits(), &exp)) {
                return Ok(Value::Nil); // placeholder; the parked fault surfaces first
            }
            let e = exp
                .to_u32()
                .expect("admit_op_result passes only when the exponent fits u32");
            Ok(int_value(Pow::pow(base, e), heap))
        }
        (a, b) => {
            let x = num_to_f64(a, span)?;
            let y = num_to_f64(b, span)?;
            finite(libm::pow(x, y), span)
        }
    }
}

/// `base ** exp` where `|base| <= 1` (`base` is `0`, `1`, or `-1`) and `exp >= 0`: the
/// result depends only on the base and the exponent's parity, so a huge exponent still
/// computes in constant work. Matches `Pow::pow`'s conventions (`0 ** 0 == 1`).
fn small_base_pow(base: &BigInt, exp: &BigInt) -> BigInt {
    if base.is_zero() {
        // 0 ** 0 == 1; 0 ** n == 0 for n > 0.
        if exp.is_zero() {
            BigInt::from(1)
        } else {
            BigInt::from(0)
        }
    } else if base.is_negative() {
        // (-1) ** exp: 1 if even, -1 if odd.
        if exp.is_even() {
            BigInt::from(1)
        } else {
            BigInt::from(-1)
        }
    } else {
        BigInt::from(1) // 1 ** exp == 1
    }
}

/// An upper bound on the byte-length of `base ** exp`'s magnitude (with `|base| >= 2`), for
/// the R8 size guard: the magnitude is at most `exp * base.bits()` bits. An exponent beyond
/// the engine's computable range (`u32`) is reported as unboundedly large (`u128::MAX`), so
/// `admit_bignum` faults rather than the operation attempting the impossible. Widened to
/// `u128` so the product cannot overflow the estimate itself.
fn pow_result_bytes(base_bits: u64, exp: &BigInt) -> u128 {
    match exp.to_u32() {
        Some(e) => u128::from(e) * u128::from(base_bits) / 8 + 1,
        None => u128::MAX,
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

fn type_error(op: BinaryOp, got: Value, heap: &Heap, span: Span) -> Raise {
    let sym = binary_symbol(op);
    Raise::new(
        ExceptionKind::TypeMismatch,
        format!("`{sym}` needs two numbers"),
        span,
    )
    .with_details(super::exception::type_mismatch_details(
        sym,
        &["Number"],
        got,
        heap,
    ))
}

fn unary_type_error(sym: &str, got: Value, heap: &Heap, span: Span) -> Raise {
    Raise::new(
        ExceptionKind::TypeMismatch,
        format!("`{sym}` needs a number"),
        span,
    )
    .with_details(super::exception::type_mismatch_details(
        sym,
        &["Number"],
        got,
        heap,
    ))
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

/// Unit tests for the numeric tower and operators, in a sibling `tests.rs` to keep
/// this file within the hygiene length limit.
#[cfg(test)]
mod tests;
