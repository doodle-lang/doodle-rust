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
//! **Aggregates.** Numbers, bools, nil, bytes, strings, and the reference-identity
//! kinds compare directly. Lists, dicts, and records compare structurally and
//! **cycle-safely** via a co-inductive walk ([`Agg`] pair stack): lists by length +
//! pairwise elements, **dicts order-independently** (same key set, equal values),
//! records **nominally** (same declared type, pairwise fields — value and `ref`
//! alike). The pair stack is a plain `Vec` (no hasher), so nothing here perturbs
//! determinism.

use super::arith::{Num, as_num};
use super::error::{ExceptionKind, Raise};
use crate::ast::BinaryOp;
use crate::heap::Heap;
use crate::machine::{DictIdx, ListIdx, RecIdx, Value};
use crate::span::Span;
use num_bigint::BigInt;
use std::cmp::Ordering;

/// An aggregate value's heap identity, tracked on the cycle-detection stack. Lists,
/// dicts, and reference records can each form cycles (through their elements, values,
/// and fields respectively), so all three take part in the co-inductive walk; a
/// re-met pair is a cycle and compares equal (L§4.13).
#[derive(Clone, Copy, PartialEq)]
enum Agg {
    List(ListIdx),
    Dict(DictIdx),
    Record(RecIdx),
}

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
/// (lists, dicts, records) compare by structure, cycle-safely.
pub(crate) fn equal(a: Value, b: Value, heap: &Heap) -> bool {
    equal_rec(a, b, heap, &mut Vec::new())
}

/// The structural walk. `in_progress` holds the aggregate-index pairs currently being
/// compared on this path; **re-meeting a pair means a cycle**, and unrolling the
/// two structures would agree forever, so the pair is equal co-inductively. This
/// makes equality terminate on self-referential structures (a cyclic list, a
/// self-referencing dict value, a `ref` record graph). The pair stack is a plain
/// `Vec` — no hasher — so nothing here can perturb determinism.
fn equal_rec(a: Value, b: Value, heap: &Heap, in_progress: &mut Vec<(Agg, Agg)>) -> bool {
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
        (Value::Dict(x), Value::Dict(y)) => dict_equal(x, y, heap, in_progress),
        (Value::Record(x), Value::Record(y)) => record_equal(x, y, heap, in_progress),
        // Different types (including a number vs a non-number) are never equal.
        _ => false,
    }
}

/// Structural equality of two lists (L§4.6/§4.13): same length and pairwise-equal
/// elements, walked cycle-safely via `in_progress`.
fn list_equal(x: ListIdx, y: ListIdx, heap: &Heap, in_progress: &mut Vec<(Agg, Agg)>) -> bool {
    // The same list object is equal to itself without a walk — also the base case
    // that terminates a list reachable from its own elements.
    if x == y {
        return true;
    }
    // This exact pair is already being compared further up the walk: a cycle.
    if in_progress.contains(&(Agg::List(x), Agg::List(y))) {
        return true;
    }
    let xs = &heap.list(x).items;
    let ys = &heap.list(y).items;
    if xs.len() != ys.len() {
        return false;
    }
    in_progress.push((Agg::List(x), Agg::List(y)));
    let equal = xs
        .iter()
        .zip(ys.iter())
        .all(|(&ea, &eb)| equal_rec(ea, eb, heap, in_progress));
    in_progress.pop();
    equal
}

/// Structural equality of two dicts (L§4.13) — **order-independent**: equal iff they
/// have the same number of entries and, for every key in `x`, `y` holds a
/// `==`-equal key mapping to a `==`-equal value. Insertion order is not compared
/// (`{a:1, b:2} == {b:2, a:1}`). Keys are hashable (so their own comparison
/// terminates); only the *values* can re-enter this walk, tracked via `in_progress`.
fn dict_equal(x: DictIdx, y: DictIdx, heap: &Heap, in_progress: &mut Vec<(Agg, Agg)>) -> bool {
    if x == y {
        return true;
    }
    if in_progress.contains(&(Agg::Dict(x), Agg::Dict(y))) {
        return true;
    }
    if heap.dict(x).entries.len() != heap.dict(y).entries.len() {
        return false;
    }
    in_progress.push((Agg::Dict(x), Agg::Dict(y)));
    // Equal lengths + every x-key present in y with an equal value ⇒ the key sets
    // and mappings coincide (dicts hold no duplicate keys, L§4.8).
    let equal =
        heap.dict(x)
            .entries
            .iter()
            .all(|&(k, vx)| match super::dict::value_for_key(heap, y, k) {
                Some(vy) => equal_rec(vx, vy, heap, in_progress),
                None => false,
            });
    in_progress.pop();
    equal
}

/// Structural equality of two records (L§4.13) — **nominal**: equal iff they are the
/// same declared type (so field count and order coincide) and every field is
/// pairwise `==`. Holds for both value and `ref` records; a `ref` record graph can
/// cycle, so the pair is tracked via `in_progress`.
fn record_equal(x: RecIdx, y: RecIdx, heap: &Heap, in_progress: &mut Vec<(Agg, Agg)>) -> bool {
    if x == y {
        return true;
    }
    if in_progress.contains(&(Agg::Record(x), Agg::Record(y))) {
        return true;
    }
    if heap.record(x).type_idx != heap.record(y).type_idx {
        return false;
    }
    in_progress.push((Agg::Record(x), Agg::Record(y)));
    let equal = heap
        .record(x)
        .fields
        .iter()
        .zip(heap.record(y).fields.iter())
        .all(|(&fx, &fy)| equal_rec(fx, fy, heap, in_progress));
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
mod tests;
