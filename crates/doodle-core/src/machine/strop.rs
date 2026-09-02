//! String `+` (concatenation) and `*` (repetition) — the two arithmetic operators L§4.4
//! also defines on strings (S-59). Both branch off `arith`'s number-only path: the
//! dispatcher tries this first, and only a pair with no string operand falls through to
//! numeric arithmetic. Concatenation renormalizes only the seam (plan AD4,
//! [`unicode::seam_concat`](crate::unicode::seam_concat)); repetition builds the repeated
//! string and renormalizes it. Every result is NFC (MD §5).
//!
//! `+` is String+String only — `"a" + 1` would need an implicit conversion, the line
//! Doodle draws. `*` takes its `Int` count on either side (`"ab" * 3` == `3 * "ab"`); a
//! `Float` count raises (no narrowing), `0` yields `""`, a negative count raises
//! `negative-count`, and a result exceeding the heap limit faults `LimitExceeded` (the
//! full R8 magnitude cap is later — this bounds the single allocation so it cannot OOM).

use super::error::{ExceptionKind, Raise};
use super::{Machine, Value};
use crate::ast::BinaryOp;
use crate::heap::Heap;
use crate::span::Span;
use num_bigint::BigInt;
use num_traits::{Signed, ToPrimitive, Zero};

/// Handles `+`/`*` when a string is involved (L§4.4, S-59). Returns `Some(result)` for a
/// string operation, or `None` when neither operand is a string — the caller then falls
/// through to numeric [`arith::binary`](super::arith::binary). A string `*` whose result
/// would exceed the heap limit parks a `LimitExceeded` fault on `machine` and returns a
/// placeholder `Some(_)`; `step` surfaces the fault before the placeholder is used.
pub(crate) fn try_binary(
    op: BinaryOp,
    lhs: Value,
    rhs: Value,
    heap: &mut Heap,
    machine: &mut Machine,
    span: Span,
) -> Result<Option<Value>, Raise> {
    match op {
        BinaryOp::Add => concat(lhs, rhs, heap, span),
        BinaryOp::Mul => repeat(lhs, rhs, heap, machine, span),
        // Every other arithmetic operator is number-only; a string operand there falls to
        // `arith`, which raises the type error.
        _ => Ok(None),
    }
}

/// `+`: concatenate two strings (renormalizing only the seam). One string operand and one
/// non-string raises — `+` performs no conversion. Neither a string → numeric `+`.
fn concat(lhs: Value, rhs: Value, heap: &mut Heap, span: Span) -> Result<Option<Value>, Raise> {
    match (lhs, rhs) {
        (Value::Str(a), Value::Str(b)) => {
            let joined = crate::unicode::seam_concat(&heap.string(a).utf8, &heap.string(b).utf8);
            Ok(Some(Value::Str(heap.alloc_string(joined.into_boxed_str()))))
        }
        (Value::Str(_), _) | (_, Value::Str(_)) => {
            let got = if matches!(lhs, Value::Str(_)) {
                rhs
            } else {
                lhs
            };
            Err(Raise::new(
                ExceptionKind::TypeMismatch,
                "`+` joins a string to a string — it won't add a string and a non-string",
                span,
            )
            .with_details(super::exception::type_mismatch_details(
                "+",
                &["String"],
                got,
                heap,
            )))
        }
        _ => Ok(None),
    }
}

/// `*`: repeat a string by an `Int` count on either side (`"ab" * 3` == `3 * "ab"`).
fn repeat(
    lhs: Value,
    rhs: Value,
    heap: &mut Heap,
    machine: &mut Machine,
    span: Span,
) -> Result<Option<Value>, Raise> {
    let (s_idx, count_val) = match (lhs, rhs) {
        (Value::Str(s), c) => (s, c),
        (c, Value::Str(s)) => (s, c),
        _ => return Ok(None), // neither operand is a string → numeric `*`
    };
    // The count must be an `Int` — a `Float` count (or a second string) raises, no
    // narrowing (S-59).
    let count = match count_val {
        Value::Int(n) => BigInt::from(n),
        Value::BigInt(idx) => heap.bigint(idx).value.clone(),
        other => {
            return Err(Raise::new(
                ExceptionKind::TypeMismatch,
                "`*` repeats a string a whole number of times — the count must be an Int",
                span,
            )
            .with_details(super::exception::type_mismatch_details(
                "*",
                &["Int"],
                other,
                heap,
            )));
        }
    };
    if count.is_negative() {
        let count_value = super::arith::int_value(count.clone(), heap);
        return Err(Raise::new(
            ExceptionKind::NegativeCount,
            "`*` can't repeat a string a negative number of times",
            span,
        )
        .with_details(vec![(
            "count",
            super::exception::DetailVal::Value(count_value),
        )]));
    }
    let len = heap.string(s_idx).utf8.len();
    // Zero copies, or repeating the empty string, is `""` — regardless of the count's
    // magnitude, so this also sidesteps the size check for a huge count of `""`.
    if count.is_zero() || len == 0 {
        return Ok(Some(Value::Str(
            heap.alloc_string(String::new().into_boxed_str()),
        )));
    }
    // Bound the result before building it — the same three rails as a bignum op (heap / per-op
    // latency cap / step budget) — so a huge repetition faults before an allocation that could
    // overflow `usize`, exhaust memory, or freeze the host. `admit_op_result` parks the right
    // fault reason; a count beyond `u128` saturates to a certain over-limit estimate.
    let result_bytes = count
        .to_u128()
        .map_or(u128::MAX, |c| (len as u128).saturating_mul(c));
    if !machine.admit_op_result(result_bytes) {
        return Ok(Some(Value::Nil)); // placeholder; `step` surfaces the parked fault first
    }
    let count_usize = count
        .to_usize()
        .expect("admit_op_result passes ⇒ the result fits, so the count fits usize");
    let repeated = heap.string(s_idx).utf8.repeat(count_usize);
    // Each copy is NFC, but the internal seams may need renormalization; an ASCII string
    // (the common case) is already NFC, so this reallocates only when it must.
    let boxed = if crate::unicode::is_nfc(&repeated) {
        repeated.into_boxed_str()
    } else {
        crate::unicode::nfc(&repeated).into_owned().into_boxed_str()
    };
    Ok(Some(Value::Str(heap.alloc_string(boxed))))
}
