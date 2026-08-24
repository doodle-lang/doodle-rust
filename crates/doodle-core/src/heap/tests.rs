//! Unit tests for [`Heap`](super::Heap): allocation identity, byte accounting, GC
//! reclamation, and reference identity. Kept in a `tests.rs` submodule so the heap
//! module's production source stays within the file-length budget.

use super::*;

#[test]
fn allocations_get_distinct_indices_and_round_trip() {
    let mut heap = Heap::new();
    let a = heap.alloc_string("hi".into());
    let b = heap.alloc_string("yo".into());
    assert_ne!(a.0, b.0);
    assert_eq!(&*heap.string(a).utf8, "hi");
    assert_eq!(&*heap.string(b).utf8, "yo");
    assert_eq!(heap.live_objects(), 2);
}

#[test]
fn bytes_allocated_counts_payload_plus_per_object_overhead_across_kinds() {
    let mut heap = Heap::new();
    heap.alloc_string("abcd".into()); // 4 payload bytes
    heap.alloc_bytes(vec![0u8; 3].into()); // 3 payload bytes
    heap.alloc_list(vec![Value::Int(1), Value::Int(2)]); // 2 * VALUE_BYTES
    // Each of the three objects also carries the fixed per-object overhead, so
    // object count (not just payload) contributes to the heap total.
    assert_eq!(
        heap.bytes_allocated(),
        4 + 3 + 2 * VALUE_BYTES + 3 * OBJECT_OVERHEAD
    );
}

#[test]
fn list_push_charges_one_value_width_and_the_sweep_reclaims_it_exactly() {
    let mut heap = Heap::new();
    let l = heap.alloc_list(Vec::new()); // empty: overhead only
    assert_eq!(heap.bytes_allocated(), OBJECT_OVERHEAD);
    // Each in-place append charges exactly one value width, so a list that grows
    // element-by-element cannot escape the heap limit (the accounting-integrity
    // rule the module header names — without this, a growing list stays "free").
    heap.list_push(l, Value::Int(1));
    heap.list_push(l, Value::Int(2));
    assert_eq!(heap.bytes_allocated(), OBJECT_OVERHEAD + 2 * VALUE_BYTES);
    // The sweep subtracts `list_payload` (= items.len() * VALUE_BYTES), so the
    // pushes' charge and the reclamation must agree: with no roots the list is
    // garbage and the heap returns to empty (a mismatch trips the sweep's
    // `freed <= bytes_allocated` assert or leaves a nonzero residue).
    heap.collect(|_| {});
    assert_eq!(heap.bytes_allocated(), 0);
}

#[test]
fn serials_are_monotonic_across_kinds_in_allocation_order() {
    // One shared counter stamps every kind, so identity serials never collide.
    let mut heap = Heap::new();
    let s = heap.alloc_string("x".into());
    let by = heap.alloc_bytes(vec![1].into());
    let l = heap.alloc_list(vec![]);
    // Reach into the slabs by index to read the stamped serial.
    assert_eq!(heap.strings.serial(s.0), 0);
    assert_eq!(heap.bytes.serial(by.0), 1);
    assert_eq!(heap.lists.serial(l.0), 2);
}

#[test]
fn same_ref_matches_slot_identity_per_reference_variant() {
    use crate::machine::{CalIdx, RecIdx};
    use crate::span::ModuleId;

    // Same variant, same index — the same object.
    assert!(same_ref(Value::Str(StrIdx(3)), Value::Str(StrIdx(3))));
    assert!(same_ref(Value::List(ListIdx(0)), Value::List(ListIdx(0))));
    assert!(same_ref(
        Value::Callable(CalIdx(9)),
        Value::Callable(CalIdx(9))
    ));
    assert!(same_ref(
        Value::Module(ModuleId(2)),
        Value::Module(ModuleId(2))
    ));

    // Same variant, different index — different objects.
    assert!(!same_ref(Value::Str(StrIdx(3)), Value::Str(StrIdx(4))));
    assert!(!same_ref(
        Value::Record(RecIdx(0)),
        Value::Record(RecIdx(1))
    ));
}

#[test]
fn same_ref_rejects_cross_variant_and_immediates() {
    use crate::machine::{CalIdx, DictIdx, TypeIdx};

    // Same underlying index, different variant — not the same slot.
    assert!(!same_ref(Value::Str(StrIdx(0)), Value::Bytes(BytesIdx(0))));
    assert!(!same_ref(Value::List(ListIdx(0)), Value::Dict(DictIdx(0))));
    assert!(!same_ref(
        Value::Type(TypeIdx(1)),
        Value::Callable(CalIdx(1))
    ));

    // Immediates carry no slot identity, even value-equal ones.
    assert!(!same_ref(Value::Int(5), Value::Int(5)));
    assert!(!same_ref(Value::Bool(true), Value::Bool(true)));
    assert!(!same_ref(Value::Nil, Value::Nil));
    // A heap value is never the same reference as an immediate.
    assert!(!same_ref(Value::Str(StrIdx(0)), Value::Nil));
}
