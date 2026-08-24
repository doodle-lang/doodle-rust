//! Tests for value hashing (L§4.8): coherence with structural `==` (equal values
//! hash equal, especially across numeric kinds), kind separation, and hashability.

use super::*;
use crate::machine::arith::int_value;
use crate::machine::compare;
use num_bigint::BigInt;

fn h(v: Value, heap: &Heap) -> u64 {
    hash_value(v, heap)
}

/// The load-bearing invariant (L§4.8): `a == b` ⇒ `hash(a) == hash(b)`.
fn assert_coheres(a: Value, b: Value, heap: &Heap) {
    if compare::equal(a, b, heap) {
        assert_eq!(h(a, heap), h(b, heap), "equal values must hash equal");
    }
}

#[test]
fn numeric_kinds_that_are_equal_hash_equal() {
    let mut heap = Heap::new();
    // 1 and 1.0.
    let one = int_value(BigInt::from(1), &mut heap);
    assert!(compare::equal(one, Value::Float(1.0), &heap));
    assert_coheres(one, Value::Float(1.0), &heap);
    // 0, 0.0, and -0.0 all hash as zero.
    let zero = int_value(BigInt::from(0), &mut heap);
    assert_coheres(zero, Value::Float(0.0), &heap);
    assert_coheres(zero, Value::Float(-0.0), &heap);
    assert_eq!(h(Value::Float(0.0), &heap), h(Value::Float(-0.0), &heap));
    // A large integer-valued float within i64 (`2^60`) hashes like the Int.
    let p60 = int_value(BigInt::from(1i64 << 60), &mut heap);
    assert_coheres(p60, Value::Float((1u64 << 60) as f64), &heap);
    // A bignum beyond i64 (`2^70`) vs the exactly-representable float `2^70`.
    let big = Value::BigInt(heap.alloc_bigint(BigInt::from(2).pow(70)));
    let f70 = Value::Float(2f64.powi(70));
    assert!(compare::equal(big, f70, &heap));
    assert_coheres(big, f70, &heap);
    // A non-integer float is not equal to a nearby int, and (as a sanity check)
    // hashes differently.
    let five = int_value(BigInt::from(5), &mut heap);
    assert!(!compare::equal(five, Value::Float(5.5), &heap));
    assert_ne!(h(five, &heap), h(Value::Float(5.5), &heap));
}

#[test]
fn nan_and_infinities() {
    let heap = Heap::new();
    // The canonical NaN equals itself and has one hash.
    let nan = Value::Float(f64::from_bits(0x7ff8_0000_0000_0000));
    assert!(compare::equal(nan, nan, &heap));
    assert_eq!(h(nan, &heap), h(nan, &heap));
    // ±∞ hash consistently and distinctly from each other.
    assert_eq!(
        h(Value::Float(f64::INFINITY), &heap),
        h(Value::Float(f64::INFINITY), &heap)
    );
    assert_ne!(
        h(Value::Float(f64::INFINITY), &heap),
        h(Value::Float(f64::NEG_INFINITY), &heap)
    );
}

#[test]
fn strings_bytes_and_kind_separation() {
    let mut heap = Heap::new();
    let s1 = Value::Str(heap.alloc_string("café".into()));
    let s2 = Value::Str(heap.alloc_string("café".into()));
    assert_eq!(h(s1, &heap), h(s2, &heap));
    // Same bytes under a different kind must not be forced to collide.
    let b = Value::Bytes(heap.alloc_bytes(vec![0x61, 0x62].into()));
    let s = Value::Str(heap.alloc_string("ab".into()));
    assert_ne!(h(b, &heap), h(s, &heap));
    // Nil / Bool / a numeric zero occupy distinct tags.
    assert_ne!(h(Value::Nil, &heap), h(Value::Bool(false), &heap));
    let zero = int_value(BigInt::from(0), &mut heap);
    assert_ne!(h(Value::Bool(false), &heap), h(zero, &heap));
}

#[test]
fn hashability_of_kinds() {
    let mut heap = Heap::new();
    assert!(is_hashable(Value::Nil));
    assert!(is_hashable(Value::Bool(true)));
    assert!(is_hashable(Value::Int(3)));
    assert!(is_hashable(Value::Float(1.5)));
    assert!(is_hashable(Value::Str(heap.alloc_string("x".into()))));
    assert!(is_hashable(Value::Bytes(heap.alloc_bytes(vec![1].into()))));
    // A list is not hashable (M4.1); records join at M4.4.
    assert!(!is_hashable(Value::List(heap.alloc_list(vec![]))));
}
