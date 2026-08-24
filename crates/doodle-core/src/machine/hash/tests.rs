//! Tests for value hashing (L§4.8): coherence with structural `==` (equal values
//! hash equal, especially across numeric kinds), kind separation, and hashability.

use super::*;
use crate::machine::arith::int_value;
use crate::machine::compare;
use num_bigint::BigInt;

fn h(v: Value, heap: &Heap) -> u64 {
    hash_value(v, heap)
}

fn is_hashable(v: Value, heap: &Heap) -> bool {
    check_hashable(v, heap).is_ok()
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

fn record_type(
    heap: &mut Heap,
    name: &str,
    fields: &[&str],
    is_ref: bool,
) -> crate::machine::TypeIdx {
    let schema = crate::machine::RecordType {
        name: name.into(),
        fields: fields.iter().map(|f| (*f).into()).collect(),
        is_ref,
    };
    heap.alloc_type(crate::heap::TypeObj {
        kind: crate::machine::TypeKind::Record(schema),
    })
}

#[test]
fn hashability_of_kinds() {
    let mut heap = Heap::new();
    assert!(is_hashable(Value::Nil, &heap));
    assert!(is_hashable(Value::Bool(true), &heap));
    assert!(is_hashable(Value::Int(3), &heap));
    assert!(is_hashable(Value::Float(1.5), &heap));
    let s = Value::Str(heap.alloc_string("x".into()));
    let by = Value::Bytes(heap.alloc_bytes(vec![1].into()));
    assert!(is_hashable(s, &heap));
    assert!(is_hashable(by, &heap));
    // Lists and dicts are never hashable.
    assert!(!is_hashable(Value::List(heap.alloc_list(vec![])), &heap));
    assert!(!is_hashable(Value::Dict(heap.alloc_dict()), &heap));
}

#[test]
fn record_hashability_follows_value_vs_ref_and_field_content() {
    let mut heap = Heap::new();
    // A value record with all-scalar fields is hashable.
    let point = record_type(&mut heap, "Point", &["x", "y"], false);
    let p = Value::Record(heap.alloc_record(point, Box::new([Value::Int(1), Value::Int(2)])));
    assert!(is_hashable(p, &heap));

    // A reference record is not hashable, and the message says so.
    let turtle = record_type(&mut heap, "Turtle", &["heading"], true);
    let t = Value::Record(heap.alloc_record(turtle, Box::new([Value::Int(0)])));
    let err = check_hashable(t, &heap).unwrap_err();
    assert!(
        err.contains("Turtle") && err.contains("reference record"),
        "{err}"
    );

    // A value record with a list field is not hashable; the raise names the field.
    let holder = record_type(&mut heap, "Holder", &["items"], false);
    let list = Value::List(heap.alloc_list(vec![Value::Int(1)]));
    let h = Value::Record(heap.alloc_record(holder, Box::new([list])));
    let err = check_hashable(h, &heap).unwrap_err();
    assert!(
        err.contains("items"),
        "should name the offending field: {err}"
    );

    // Nesting: a value record whose value-record field holds a list — the message
    // names the deep offending field, not the intermediate one.
    let outer = record_type(&mut heap, "Outer", &["inner"], false);
    let o = Value::Record(heap.alloc_record(outer, Box::new([h])));
    let err = check_hashable(o, &heap).unwrap_err();
    assert!(err.contains("items"), "should name the deep field: {err}");
}

#[test]
fn value_records_hash_coherently_with_structural_eq() {
    let mut heap = Heap::new();
    let point = record_type(&mut heap, "Point", &["x", "y"], false);
    // Two distinct instances with equal fields are `==` and must hash equal.
    let a = Value::Record(heap.alloc_record(point, Box::new([Value::Int(1), Value::Float(2.0)])));
    let b = Value::Record(heap.alloc_record(point, Box::new([Value::Int(1), Value::Int(2)])));
    assert!(compare::equal(a, b, &heap));
    assert_coheres(a, b, &heap);
    // A different field value hashes differently (not required, but expected here).
    let c = Value::Record(heap.alloc_record(point, Box::new([Value::Int(1), Value::Int(3)])));
    assert!(!compare::equal(a, c, &heap));
}
