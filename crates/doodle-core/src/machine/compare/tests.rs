//! Tests for comparison, equality, and ordering (L§4.13, L§6.6): cross-kind numeric
//! equality, string/bytes content equality, and the structural, cycle-safe walk over
//! lists, dicts (order-independent), and records (nominal).

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

fn dict(entries: &[(Value, Value)], h: &mut Heap) -> Value {
    let d = h.alloc_dict();
    for &(k, v) in entries {
        super::super::dict::insert(h, d, k, v, S).unwrap();
    }
    Value::Dict(d)
}

fn record_type(h: &mut Heap, name: &str, fields: &[&str], is_ref: bool) -> crate::machine::TypeIdx {
    let schema = crate::machine::RecordType {
        name: name.into(),
        fields: fields.iter().map(|f| (*f).into()).collect(),
        is_ref,
    };
    h.alloc_type(crate::heap::TypeObj {
        kind: crate::machine::TypeKind::Record(schema),
    })
}

#[test]
fn dicts_compare_order_independently() {
    let mut h = Heap::new();
    let (one, two) = (int("1", &mut h), int("2", &mut h));
    // Same entries, different insertion order — equal (L§4.13 delta).
    let a = dict(&[(one, Value::Int(10)), (two, Value::Int(20))], &mut h);
    let b = dict(&[(two, Value::Int(20)), (one, Value::Int(10))], &mut h);
    assert!(eq(a, b, &h));
    // A differing value under a shared key.
    let c = dict(&[(one, Value::Int(10)), (two, Value::Int(99))], &mut h);
    assert!(!eq(a, c, &h));
    // A differing key set of the same size.
    let three = int("3", &mut h);
    let d = dict(&[(one, Value::Int(10)), (three, Value::Int(20))], &mut h);
    assert!(!eq(a, d, &h));
    // Different sizes.
    let e = dict(&[(one, Value::Int(10))], &mut h);
    assert!(!eq(a, e, &h));
    // Values compare cross-kind by exact value through the walk (1 ≡ 1.0).
    let f = dict(&[(one, Value::Float(10.0)), (two, Value::Int(20))], &mut h);
    assert!(eq(a, f, &h));
    // Empty dicts are equal.
    assert!(eq(dict(&[], &mut h), dict(&[], &mut h), &h));
}

#[test]
fn records_compare_nominally_and_fieldwise() {
    let mut h = Heap::new();
    let point = record_type(&mut h, "Point", &["x", "y"], false);
    let a = Value::Record(h.alloc_record(point, Box::new([Value::Int(1), Value::Int(2)])));
    let b = Value::Record(h.alloc_record(point, Box::new([Value::Int(1), Value::Float(2.0)])));
    // Same type, fieldwise-equal (cross-kind value equality recurses).
    assert!(eq(a, b, &h));
    // A differing field.
    let c = Value::Record(h.alloc_record(point, Box::new([Value::Int(1), Value::Int(3)])));
    assert!(!eq(a, c, &h));
    // Same shape, *different declared type* — nominal, so not equal.
    let pair = record_type(&mut h, "Pair", &["x", "y"], false);
    let d = Value::Record(h.alloc_record(pair, Box::new([Value::Int(1), Value::Int(2)])));
    assert!(!eq(a, d, &h));
    // A record is never equal to a non-record, and never raises.
    assert!(!eq(a, Value::Int(1), &h));
}

#[test]
fn record_and_dict_equality_is_cycle_safe() {
    let mut h = Heap::new();
    // Two self-referential `ref` records `a.next = a`, `b.next = b`: structurally
    // equal by co-induction, and the walk must terminate.
    let node = record_type(&mut h, "Node", &["next"], true);
    let ai = h.alloc_record(node, Box::new([Value::Nil]));
    h.record_set_field(ai, 0, Value::Record(ai));
    let bi = h.alloc_record(node, Box::new([Value::Nil]));
    h.record_set_field(bi, 0, Value::Record(bi));
    assert!(eq(Value::Record(ai), Value::Record(ai), &h)); // reflexive
    assert!(eq(Value::Record(ai), Value::Record(bi), &h)); // co-inductive, terminates
    // A dict whose value points back to itself: equality must also terminate.
    let key = Value::Str(h.alloc_string("self".into()));
    let da = h.alloc_dict();
    super::super::dict::insert(&mut h, da, key, Value::Dict(da), S).unwrap();
    let db = h.alloc_dict();
    super::super::dict::insert(&mut h, db, key, Value::Dict(db), S).unwrap();
    assert!(eq(Value::Dict(da), Value::Dict(db), &h));
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
