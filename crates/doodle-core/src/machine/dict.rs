//! Dict operations (L§4.7/§4.8): insertion-ordered key→value storage with hashing
//! that coheres with structural `==` (`super::hash`). The heap owns the
//! [`DictObj`](crate::heap::DictObj) storage (entries + index); the logic — hashing,
//! first-key-wins, and `==` collision resolution — lives here because it needs value
//! hashing and equality (which the heap layer must not reach up to).

use super::compare::{self, equal};
use super::cont::Cont;
use super::error::{ExceptionKind, Raise};
use super::hash::{check_hashable, hash_value};
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
    let hash = key_hash(key, heap, span)?;
    let value = super::record::copy_on_bind(value, heap);
    match find(heap, idx, key, hash) {
        Some(pos) => heap.dict_set_value(idx, pos, value),
        None => {
            let key = super::record::copy_on_bind(key, heap);
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

/// Hashes a key, raising if it is not hashable (L§4.8). The raise's message names
/// the offending field for a value record with a non-hashable field.
fn key_hash(key: Value, heap: &Heap, span: Span) -> Result<u64, Raise> {
    if let Err(reason) = check_hashable(key, heap) {
        return Err(Raise::new(ExceptionKind::UnhashableKey, reason, span));
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

/// An index expression's key is in the register: `object[key]` (L§6.3). A `Dict` indexes
/// by key (absent → `KeyNotFound`); a `List`/`String`/`Bytes` indexes by an `Int` position
/// in `0 <= k < length` (out of range → `IndexOutOfRange`) — a `String` by extended
/// grapheme cluster (yielding a length-one string), `Bytes` by byte (yielding an `Int`).
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
        Value::List(l) => {
            let i = sequence_index(heap, key, heap.list(l).items.len(), span)?;
            machine.reg = Some(heap.list(l).items[i]);
            Ok(())
        }
        Value::Bytes(b) => {
            let i = sequence_index(heap, key, heap.byte_string(b).bytes.len(), span)?;
            machine.reg = Some(Value::Int(heap.byte_string(b).bytes[i] as i64));
            Ok(())
        }
        Value::Str(s) => {
            let i = sequence_index(heap, key, heap.grapheme_offsets(s).len(), span)?;
            // The i-th grapheme: the substring between cluster boundaries. It is a slice of
            // an NFC string at normalization boundaries, so it is itself NFC.
            let (start, end) = {
                let offsets = heap.grapheme_offsets(s);
                let start = offsets[i] as usize;
                let end = offsets
                    .get(i + 1)
                    .map_or(heap.string(s).utf8.len(), |&e| e as usize);
                (start, end)
            };
            let grapheme: Box<str> = heap.string(s).utf8[start..end].into();
            machine.reg = Some(Value::Str(heap.alloc_string(grapheme)));
            Ok(())
        }
        other => Err(Raise::new(
            ExceptionKind::TypeMismatch,
            format!("you can't index {} with `[…]`", compare::kind_name(other)),
            span,
        )),
    }
}

/// Resolves a sequence index `key` against a container of `length` positions (L§6.3): a
/// non-negative `Int` in range yields the `usize` position; a negative or too-large `Int`
/// (or a bignum, which no real container can index) raises `IndexOutOfRange`; a non-`Int`
/// raises `TypeMismatch`. The out-of-range message branches on sign — a negative index
/// gets the deliberate no-negative-positions hint (a Python habit).
fn sequence_index(heap: &Heap, key: Value, length: usize, span: Span) -> Result<usize, Raise> {
    match key {
        Value::Int(n) if n >= 0 && (n as u128) < length as u128 => Ok(n as usize),
        Value::Int(n) => Err(out_of_range(&n.to_string(), n < 0, length, span)),
        Value::BigInt(idx) => {
            let value = &heap.bigint(idx).value;
            let negative = value.sign() == num_bigint::Sign::Minus;
            Err(out_of_range(&value.to_string(), negative, length, span))
        }
        other => Err(Raise::new(
            ExceptionKind::TypeMismatch,
            format!(
                "an index must be a whole number (an Int), not {}",
                compare::kind_name(other)
            ),
            span,
        )),
    }
}

/// The `IndexOutOfRange` raise (L§6.3, S-58), its message branching on sign: a negative
/// index carries the no-negative-positions hint; a too-large one names the length.
fn out_of_range(index: &str, negative: bool, length: usize, span: Span) -> Raise {
    let message = if negative {
        format!(
            "there's no position {index} — Doodle has no negative positions; to reach the \
             last item, use `length - 1`"
        )
    } else {
        format!("there's no position {index} — the length is {length}")
    };
    Raise::new(ExceptionKind::IndexOutOfRange, message, span)
}

/// Completes an index place assignment `object[key] = rhs` (L§5.3): `object` (the
/// place, no copy) and `key` are passed in; the RHS is in the register. For a dict,
/// stores `key → rhs` ([`insert`] applies first-key-wins and copies a value-record RHS
/// for binding). A `String`/`Bytes` is immutable (L§4.4/§4.5) so its index is never an
/// assignment target; a `List` element assignment (`xs[i] = v`) is a separate, still-
/// pending list-mutation item — until it lands, a non-dict object raises `TypeMismatch`.
/// The statement yields Void.
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
