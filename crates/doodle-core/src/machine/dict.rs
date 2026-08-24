//! Dict operations (L§4.7/§4.8): insertion-ordered key→value storage with hashing
//! that coheres with structural `==` (`super::hash`). The heap owns the
//! [`DictObj`](crate::heap::DictObj) storage (entries + index); the logic — hashing,
//! first-key-wins, and `==` collision resolution — lives here because it needs value
//! hashing and equality (which the heap layer must not reach up to).

use super::compare::{self, equal, kind_name};
use super::cont::Cont;
use super::error::{ExceptionKind, Raise};
use super::hash::{hash_value, is_hashable};
use super::step::take_value;
use super::{Machine, Value};
use crate::ast::{DictKey, Node, NodeId};
use crate::heap::Heap;
use crate::machine::DictIdx;
use crate::resolve::ResolvedModule;
use crate::span::Span;

/// Inserts `key → value` (L§4.8). If an existing key is structurally `==` `key`, its
/// value is overwritten and the **first key is kept** (first-key-wins); otherwise a
/// new entry is appended in insertion order. Raises [`ExceptionKind::UnhashableKey`]
/// if `key` is not hashable. A value record is copied on store (L§4.14) — a dict
/// entry is a place — so this is the single copy choke for both dict literals and
/// `d[k] = v` place assignment.
pub(super) fn insert(
    heap: &mut Heap,
    idx: DictIdx,
    key: Value,
    value: Value,
    span: Span,
) -> Result<(), Raise> {
    let hash = key_hash(key, heap, span)?;
    let value = super::record::copy_on_bind(value, heap);
    match find(heap, idx, key, hash) {
        Some(pos) => heap.dict_set_value(idx, pos, value),
        None => {
            heap.dict_push_entry(idx, key, value, hash);
        }
    }
    Ok(())
}

/// Looks up `key`, returning its value or `None` if absent. Using a non-hashable
/// key raises (L§4.8) rather than silently missing.
pub(super) fn get(
    heap: &Heap,
    idx: DictIdx,
    key: Value,
    span: Span,
) -> Result<Option<Value>, Raise> {
    let hash = key_hash(key, heap, span)?;
    Ok(find(heap, idx, key, hash).map(|pos| heap.dict(idx).entries[pos as usize].1))
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

/// Hashes a key, raising if it is not hashable (L§4.8).
fn key_hash(key: Value, heap: &Heap, span: Span) -> Result<u64, Raise> {
    if !is_hashable(key) {
        return Err(Raise::new(
            ExceptionKind::UnhashableKey,
            format!("{} can't be used as a dict key", kind_name(key)),
            span,
        ));
    }
    Ok(hash_value(key, heap))
}

// --- dict-literal + index-read evaluation (the continuation handlers `step` dispatches) ---

/// Drives a dict literal's entry at `index` (L§4.8): a **bare** key is the string
/// built here (skip straight to the value); a **computed** key is evaluated first.
/// Once past the last entry, allocate the dict and insert every pair in order — the
/// insert applies first-key-wins for duplicate keys.
pub(super) fn dict_advance(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    node: NodeId,
    entries: Vec<(Value, Value)>,
    index: u32,
) -> Result<(), Raise> {
    let Node::Dict(literal) = resolved.ast.node(node) else {
        unreachable!("a Dict continuation over a non-Dict node");
    };
    let span = resolved.ast.span(node);
    match literal.get(index as usize) {
        None => {
            let dict = heap.alloc_dict();
            for (key, value) in entries {
                insert(heap, dict, key, value, span)?;
            }
            machine.reg = Some(Value::Dict(dict));
            Ok(())
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
pub(super) fn dict_got_value(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    node: NodeId,
    mut entries: Vec<(Value, Value)>,
    index: u32,
    key: Value,
) -> Result<(), Raise> {
    let value = take_value(machine, resolved.ast.span(node))?;
    entries.push((key, value));
    dict_advance(resolved, heap, machine, node, entries, index + 1)
}

/// An index expression's object is in the register: stash it, evaluate the key.
pub(super) fn index_got_object(
    machine: &mut Machine,
    index: NodeId,
    span: crate::span::Span,
) -> Result<(), Raise> {
    let object = take_value(machine, span)?;
    let frame = machine.frames.last_mut().expect("a frame is active");
    frame.conts.push(Cont::IndexApply { object, span });
    frame.conts.push(Cont::Eval { node: index });
    Ok(())
}

/// An index expression's key is in the register: look it up in the object (L§4.8).
/// Dicts index by key now; list/string indexing joins the same arm at M4.8.
pub(super) fn index_apply(
    heap: &mut Heap,
    machine: &mut Machine,
    object: Value,
    span: crate::span::Span,
) -> Result<(), Raise> {
    let key = take_value(machine, span)?;
    match object {
        Value::Dict(d) => match get(heap, d, key, span)? {
            Some(value) => {
                machine.reg = Some(value);
                Ok(())
            }
            None => Err(Raise::new(
                ExceptionKind::KeyNotFound,
                "that key isn't in the dict",
                span,
            )),
        },
        other => Err(Raise::new(
            ExceptionKind::TypeMismatch,
            format!("you can't index {} with `[…]`", compare::kind_name(other)),
            span,
        )),
    }
}

/// Completes an index place assignment `object[key] = rhs` (L§5.3): `object` (the
/// place, no copy) and `key` are passed in; the RHS is in the register. For a dict,
/// stores `key → rhs` ([`insert`] applies first-key-wins and copies a value-record
/// RHS for binding). List/string index assignment joins this arm at M4.8, when list
/// indexing lands; until then a non-dict object raises `TypeMismatch`, matching the
/// index *read* path ([`index_apply`]). The statement yields Void.
pub(super) fn index_set(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    assign: NodeId,
    object: Value,
    key: Value,
) -> Result<(), Raise> {
    let Node::Assign { target, value } = resolved.ast.node(assign) else {
        unreachable!("dict::index_set over a non-Assign node");
    };
    let span = resolved.ast.span(*target);
    let rhs = take_value(machine, resolved.ast.span(*value))?;
    match object {
        Value::Dict(d) => insert(heap, d, key, rhs, span),
        other => Err(Raise::new(
            ExceptionKind::TypeMismatch,
            format!("you can't index {} with `[…]`", compare::kind_name(other)),
            span,
        )),
    }
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
