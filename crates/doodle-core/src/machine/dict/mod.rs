//! Dict operations (L§4.7/§4.8): insertion-ordered key→value storage with hashing
//! that coheres with structural `==` (`super::hash`). The heap owns the
//! [`DictObj`](crate::heap::DictObj) storage (entries + index); the logic — hashing,
//! first-key-wins, and `==` collision resolution — lives here because it needs value
//! hashing and equality (which the heap layer must not reach up to).

mod index;

pub(super) use index::{
    index_apply, index_assign_hashed, index_got_object, index_read_hashed, index_set,
};

use super::compare::equal;
use super::cont::Cont;
use super::error::Raise;
use super::hash::{hash_value, native_key_hash, user_hash_to_bucket};
use super::protocol::{self, Dispatch};
use super::step::take_value;
use super::{LoadedModule, Machine, Value};
use crate::ast::{DictKey, Node, NodeId};
use crate::heap::Heap;
use crate::machine::DictIdx;
use crate::machine::value::CalIdx;
use crate::resolve::ResolvedModule;
use crate::span::Span;

/// How an incoming dict key's hash is obtained (L§15 hook 2, D-M5-1): a key whose runtime
/// type has an explicit `implement Hashable` drives that method (a real call); every other
/// key uses the engine's native structural hash. Chosen by [`hash_plan`] at each dict site.
enum HashPlan {
    /// Use the native `Hashable` default (`hash::native_key_hash`) — raises if unhashable.
    Native,
    /// Drive this `hash` implementation on the key; its returned `Int` fixes the bucket.
    Drive(CalIdx),
}

/// Decides how to hash an incoming dict key (L§4.8, D-M5-1): `Drive` when the key's type has an
/// explicit `implement Hashable`, else `Native`. A non-`Hashable`-filtered resolution keeps an
/// unrelated user protocol that happens to declare a `hash` member out of dict hashing.
fn hash_plan(machine: &Machine, heap: &Heap, modules: &[LoadedModule], key: Value) -> HashPlan {
    if let (Some(member), Some(filter)) = (
        machine.protocols.hash_member(),
        machine.protocols.hashable_id(),
    ) {
        let dt = protocol::dispatch_type_of(key, heap, modules, &machine.intrinsics);
        if let Dispatch::Call(cal) = machine.protocols.resolve(member, dt, Some(filter), heap) {
            return HashPlan::Drive(cal);
        }
    }
    HashPlan::Native
}

/// Inserts `key → value` (L§4.8). If an existing key is structurally `==` `key`, its
/// value is overwritten and the **first key is kept** (first-key-wins); otherwise a
/// new entry is appended in insertion order. Raises [`ExceptionKind::UnhashableKey`]
/// if `key` is not hashable. Both the key **and** the value are copied on store
/// (L§4.14/§9.5) — a dict entry is a place, and a stored key that aliased its source
/// would let a later mutation of the source change the dict's key set (and desync a
/// key from its hash bucket) — so this is the single copy choke for both dict literals
/// and `d[k] = v` place assignment. A value-record copy is structurally `==` and hashes
/// identically, so the pre-computed `hash` still applies.
pub(super) fn insert(
    heap: &mut Heap,
    idx: DictIdx,
    key: Value,
    value: Value,
    span: Span,
) -> Result<(), Raise> {
    let hash = native_key_hash(key, heap, span)?;
    insert_with_hash(heap, idx, key, value, hash);
    Ok(())
}

/// Inserts `key → value` under an already-computed `hash` (L§4.8): the store choke once the
/// bucket hash is known, whether from the native default or a driven `Hashable.hash`. Applies
/// first-key-wins and copies both key and value on store (see [`insert`]).
fn insert_with_hash(heap: &mut Heap, idx: DictIdx, key: Value, value: Value, hash: u64) {
    let value = super::record::copy_on_bind(value, heap);
    match find(heap, idx, key, hash) {
        Some(pos) => heap.dict_set_value(idx, pos, value),
        None => {
            let key = super::record::copy_on_bind(key, heap);
            heap.dict_push_entry(idx, key, value, hash);
        }
    }
}

/// Looks up `key`, returning its value or `None` if absent. Using a non-hashable
/// key raises (L§4.8) rather than silently missing.
pub(super) fn get(
    heap: &Heap,
    idx: DictIdx,
    key: Value,
    span: Span,
) -> Result<Option<Value>, Raise> {
    let hash = native_key_hash(key, heap, span)?;
    Ok(get_with_hash(heap, idx, key, hash))
}

/// Looks up `key` under an already-computed `hash` (the bucket from the native default or a
/// driven `Hashable.hash`); `None` if absent. `==` still resolves collisions.
fn get_with_hash(heap: &Heap, idx: DictIdx, key: Value, hash: u64) -> Option<Value> {
    find(heap, idx, key, hash).map(|pos| heap.dict(idx).entries[pos as usize].1)
}

/// The value stored under a key `==`-equal to `key`, or `None` if absent. Used by
/// dict `==` (`super::compare`) to look up one dict's keys in the other. The key
/// comes from a dict's own entries, so it is known hashable and this never raises.
pub(super) fn value_for_key(heap: &Heap, idx: DictIdx, key: Value) -> Option<Value> {
    let hash = hash_value(key, heap);
    find(heap, idx, key, hash).map(|pos| heap.dict(idx).entries[pos as usize].1)
}

/// The position of the entry whose key is `==` `key` (already hashed to `hash`), or
/// `None`. Candidates share the key's content hash; `==` resolves the collision.
fn find(heap: &Heap, idx: DictIdx, key: Value, hash: u64) -> Option<u32> {
    let d = heap.dict(idx);
    let positions = d.index.get(&hash)?;
    positions
        .iter()
        .copied()
        .find(|&p| equal(d.entries[p as usize].0, key, heap))
}

// --- dict-literal + index-read evaluation (the continuation handlers `step` dispatches) ---

/// Drives a dict literal's entry at `index` (L§4.8): a **bare** key is the string
/// built here (skip straight to the value); a **computed** key is evaluated first.
/// Once past the last entry, allocate the dict and insert every pair in order — the
/// insert applies first-key-wins for duplicate keys.
pub(super) fn dict_advance(
    resolved: &ResolvedModule,
    modules: &[LoadedModule],
    heap: &mut Heap,
    machine: &mut Machine,
    node: NodeId,
    entries: Vec<(Value, Value)>,
    index: u32,
) -> Result<(), Raise> {
    let Node::Dict(literal) = resolved.ast.node(node) else {
        unreachable!("a Dict continuation over a non-Dict node");
    };
    match literal.get(index as usize) {
        // Past the last entry: allocate the dict, then insert each pair — driving an explicit
        // `Hashable.hash` per key (D-M5-1) — via the build loop.
        None => {
            let dict = heap.alloc_dict();
            dict_build(resolved, modules, heap, machine, node, dict, entries, 0)
        }
        Some(entry) => {
            let value_node = entry.value;
            match &entry.key {
                // A bare-word key `name:` is the string `"name"`. Identifiers are NFC
                // already, but normalize to uphold the heap-string invariant (L§4.4).
                DictKey::Bare(name) => {
                    let nfc = crate::unicode::nfc(name).into_owned();
                    let key = Value::Str(heap.alloc_string(nfc.into_boxed_str()));
                    let frame = machine.frames.last_mut().expect("a frame is active");
                    frame.conts.push(Cont::DictGotValue {
                        dict: node,
                        entries,
                        index,
                        key,
                    });
                    frame.conts.push(Cont::Eval { node: value_node });
                    Ok(())
                }
                DictKey::Expr(key_node) => {
                    let key_node = *key_node;
                    let frame = machine.frames.last_mut().expect("a frame is active");
                    frame.conts.push(Cont::DictGotKey {
                        dict: node,
                        entries,
                        index,
                    });
                    frame.conts.push(Cont::Eval { node: key_node });
                    Ok(())
                }
            }
        }
    }
}

/// A dict entry's computed key is in the register: pair it with the value to
/// evaluate next.
pub(super) fn dict_got_key(
    resolved: &ResolvedModule,
    machine: &mut Machine,
    node: NodeId,
    entries: Vec<(Value, Value)>,
    index: u32,
) -> Result<(), Raise> {
    let key = take_value(machine, resolved.ast.span(node))?;
    let Node::Dict(literal) = resolved.ast.node(node) else {
        unreachable!("a Dict continuation over a non-Dict node");
    };
    let value_node = literal[index as usize].value;
    let frame = machine.frames.last_mut().expect("a frame is active");
    frame.conts.push(Cont::DictGotValue {
        dict: node,
        entries,
        index,
        key,
    });
    frame.conts.push(Cont::Eval { node: value_node });
    Ok(())
}

/// A dict entry's value is in the register: record `key → value` and move on.
#[allow(clippy::too_many_arguments)]
pub(super) fn dict_got_value(
    resolved: &ResolvedModule,
    modules: &[LoadedModule],
    heap: &mut Heap,
    machine: &mut Machine,
    node: NodeId,
    mut entries: Vec<(Value, Value)>,
    index: u32,
    key: Value,
) -> Result<(), Raise> {
    let value = take_value(machine, resolved.ast.span(node))?;
    entries.push((key, value));
    dict_advance(resolved, modules, heap, machine, node, entries, index + 1)
}

/// Inserts the collected `(key, value)` pairs of a dict literal into `dict` from `index` on
/// (L§4.8, D-M5-1). A key with a native hash inserts inline (the common case, no per-entry
/// continuation); the first key whose type has an explicit `implement Hashable` drives that
/// `hash` and parks [`Cont::DictBuildHashed`] to resume with the returned bucket. `dict` is a
/// GC root through the parked continuation, so it survives an allocation inside the driven hash.
#[allow(clippy::too_many_arguments)]
fn dict_build(
    resolved: &ResolvedModule,
    modules: &[LoadedModule],
    heap: &mut Heap,
    machine: &mut Machine,
    node: NodeId,
    dict: DictIdx,
    entries: Vec<(Value, Value)>,
    mut index: usize,
) -> Result<(), Raise> {
    let span = resolved.ast.span(node);
    while index < entries.len() {
        let (key, value) = entries[index];
        match hash_plan(machine, heap, modules, key) {
            HashPlan::Native => {
                insert(heap, dict, key, value, span)?;
                index += 1;
            }
            HashPlan::Drive(cal) => {
                let frame = machine.frames.last_mut().expect("a frame is active");
                frame.conts.push(Cont::DictBuildHashed {
                    node,
                    dict,
                    entries,
                    index: index as u32,
                });
                return protocol::enter_unary(modules, heap, machine, cal, key, node, span);
            }
        }
    }
    machine.reg = Some(Value::Dict(dict));
    Ok(())
}

/// A driven `Hashable.hash` for the key of dict-literal entry `index` has returned its bucket
/// `Int` (L§15 hook 2): insert that pair, then continue building the remaining entries.
#[allow(clippy::too_many_arguments)]
pub(super) fn dict_build_hashed(
    resolved: &ResolvedModule,
    modules: &[LoadedModule],
    heap: &mut Heap,
    machine: &mut Machine,
    node: NodeId,
    dict: DictIdx,
    entries: Vec<(Value, Value)>,
    index: u32,
) -> Result<(), Raise> {
    let span = resolved.ast.span(node);
    let bucket = user_hash_to_bucket(take_value(machine, span)?, heap, span)?;
    let (key, value) = entries[index as usize];
    insert_with_hash(heap, dict, key, value, bucket);
    dict_build(
        resolved,
        modules,
        heap,
        machine,
        node,
        dict,
        entries,
        index as usize + 1,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::Span;

    const S: Span = Span::DUMMY;

    // `Value` has no derived `==` (equality is structural); assert on the payload.
    fn as_int(v: Option<Value>) -> Option<i64> {
        match v {
            Some(Value::Int(n)) => Some(n),
            _ => None,
        }
    }

    #[test]
    fn insert_get_first_key_wins_and_cross_kind() {
        let mut heap = Heap::new();
        let d = heap.alloc_dict();
        let one = Value::Int(1);
        insert(&mut heap, d, one, Value::Int(10), S).unwrap();
        assert_eq!(as_int(get(&heap, d, one, S).unwrap()), Some(10));
        // A cross-kind-equal key finds the same entry (1 ≡ 1.0, S-28).
        assert_eq!(
            as_int(get(&heap, d, Value::Float(1.0), S).unwrap()),
            Some(10)
        );
        // First-key-wins: re-inserting via `1.0` updates the value but keeps the `Int(1)` key.
        insert(&mut heap, d, Value::Float(1.0), Value::Int(99), S).unwrap();
        assert_eq!(heap.dict(d).entries.len(), 1, "still one entry");
        assert!(
            matches!(heap.dict(d).entries[0].0, Value::Int(1)),
            "the first key is kept"
        );
        assert_eq!(as_int(get(&heap, d, one, S).unwrap()), Some(99));
        // A missing key is a clean `None`, not a raise.
        assert!(get(&heap, d, Value::Int(2), S).unwrap().is_none());
    }

    #[test]
    fn a_non_hashable_key_raises_on_both_insert_and_get() {
        let mut heap = Heap::new();
        let d = heap.alloc_dict();
        let list = Value::List(heap.alloc_list(vec![]));
        assert!(insert(&mut heap, d, list, Value::Nil, S).is_err());
        assert!(get(&heap, d, list, S).is_err());
    }

    #[test]
    fn gc_traces_a_reachable_dicts_keys_and_values() {
        let mut heap = Heap::new();
        let d = heap.alloc_dict();
        let key = Value::Str(heap.alloc_string("k".into()));
        let val = Value::List(heap.alloc_list(vec![Value::Int(7)]));
        insert(&mut heap, d, key, val, S).unwrap();
        let before = heap.live_objects();
        // With the dict as the only root, its key (a string) and value (a list) survive
        // because the dict scan reaches them.
        heap.collect(|tracer| tracer.value(Value::Dict(d)));
        assert_eq!(
            heap.live_objects(),
            before,
            "dict, key, and value all survive"
        );
        assert!(
            get(&heap, d, key, S).unwrap().is_some(),
            "the entry is still found"
        );
        // With no root, everything is reclaimed.
        heap.collect(|_| {});
        assert_eq!(heap.live_objects(), 0);
    }
}
