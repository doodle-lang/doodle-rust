//! Index-expression evaluation for dicts and sequences (L§6.3/§4.8): `object[key]` reads
//! ([`index_apply`]) and `object[key] = v` place assignments ([`index_set`]). A dict key whose
//! type has an explicit `implement Hashable` drives that `hash` (a real call, resumed by
//! [`index_read_hashed`]/[`index_assign_hashed`], D-M5-1); every other key uses the native
//! default. Split from the dict core (`mod.rs`) for length; the storage/hashing helpers live
//! there and are reached through `super`.

use super::{HashPlan, get, get_with_hash, hash_plan, insert, insert_with_hash};
use crate::ast::{Node, NodeId};
use crate::heap::Heap;
use crate::machine::cont::Cont;
use crate::machine::error::{ExceptionKind, Raise};
use crate::machine::hash::user_hash_to_bucket;
use crate::machine::step::take_value;
use crate::machine::value::DictIdx;
use crate::machine::{LoadedModule, Machine, Value, compare, protocol};
use crate::resolve::ResolvedModule;
use crate::span::Span;

/// An index expression's object is in the register: stash it (and the key node, a call site
/// for a driven `Hashable.hash`), evaluate the key.
pub(crate) fn index_got_object(
    machine: &mut Machine,
    index: NodeId,
    span: crate::span::Span,
) -> Result<(), Raise> {
    let object = take_value(machine, span)?;
    let frame = machine.frames.last_mut().expect("a frame is active");
    frame.conts.push(Cont::IndexApply {
        object,
        span,
        key_node: index,
    });
    frame.conts.push(Cont::Eval { node: index });
    Ok(())
}

/// An index expression's key is in the register: `object[key]` (L§6.3). A `Dict` indexes
/// by key (absent → `KeyNotFound`); a `List`/`String`/`Bytes` indexes by an `Int` position
/// in `0 <= k < length` (out of range → `IndexOutOfRange`) — a `String` by extended
/// grapheme cluster (yielding a length-one string), `Bytes` by byte (yielding an `Int`).
pub(crate) fn index_apply(
    modules: &[LoadedModule],
    heap: &mut Heap,
    machine: &mut Machine,
    object: Value,
    key_node: NodeId,
    span: crate::span::Span,
) -> Result<(), Raise> {
    let key = take_value(machine, span)?;
    match object {
        // A key whose type has an explicit `implement Hashable` drives its `hash` (resumed by
        // `index_read_hashed`); otherwise the native default hashes and looks up inline.
        Value::Dict(d) => match hash_plan(machine, heap, modules, key) {
            HashPlan::Drive(cal) => {
                let frame = machine.frames.last_mut().expect("a frame is active");
                frame
                    .conts
                    .push(Cont::IndexReadHashed { dict: d, key, span });
                protocol::enter_unary(modules, heap, machine, cal, key, key_node, span)
            }
            HashPlan::Native => index_read_result(machine, get(heap, d, key, span)?, span),
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

/// Places a dict lookup's outcome (L§4.8): the found value into the register, or the
/// `KeyNotFound` raise. Shared by the native and driven-hash read paths.
fn index_read_result(machine: &mut Machine, found: Option<Value>, span: Span) -> Result<(), Raise> {
    match found {
        Some(value) => {
            machine.reg = Some(value);
            Ok(())
        }
        None => Err(Raise::new(
            ExceptionKind::KeyNotFound,
            "that key isn't in the dict",
            span,
        )),
    }
}

/// A driven `Hashable.hash` for an index-read key has returned its bucket `Int` (L§15 hook 2):
/// look the key up under that bucket and place the value (or raise `KeyNotFound`).
pub(crate) fn index_read_hashed(
    heap: &mut Heap,
    machine: &mut Machine,
    dict: DictIdx,
    key: Value,
    span: Span,
) -> Result<(), Raise> {
    let bucket = user_hash_to_bucket(take_value(machine, span)?, heap, span)?;
    index_read_result(machine, get_with_hash(heap, dict, key, bucket), span)
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
pub(crate) fn index_set(
    resolved: &ResolvedModule,
    modules: &[LoadedModule],
    heap: &mut Heap,
    machine: &mut Machine,
    assign: NodeId,
    object: Value,
    key: Value,
) -> Result<(), Raise> {
    let Node::Assign { target, value } = resolved.ast.node(assign) else {
        unreachable!("dict::index_set over a non-Assign node");
    };
    let (target, value) = (*target, *value);
    let span = resolved.ast.span(target);
    let rhs = take_value(machine, resolved.ast.span(value))?;
    match object {
        // A key whose type has an explicit `implement Hashable` drives its `hash` (resumed by
        // `index_assign_hashed`); otherwise the native default hashes and stores inline.
        Value::Dict(d) => match hash_plan(machine, heap, modules, key) {
            HashPlan::Drive(cal) => {
                let frame = machine.frames.last_mut().expect("a frame is active");
                frame.conts.push(Cont::IndexAssignHashed {
                    dict: d,
                    key,
                    value: rhs,
                    span,
                });
                protocol::enter_unary(modules, heap, machine, cal, key, target, span)
            }
            HashPlan::Native => insert(heap, d, key, rhs, span),
        },
        other => Err(Raise::new(
            ExceptionKind::TypeMismatch,
            format!("you can't index {} with `[…]`", compare::kind_name(other)),
            span,
        )),
    }
}

/// A driven `Hashable.hash` for an index-assign key has returned its bucket `Int` (L§15 hook 2):
/// store `key → value` under that bucket. The statement yields Void.
pub(crate) fn index_assign_hashed(
    heap: &mut Heap,
    machine: &mut Machine,
    dict: DictIdx,
    key: Value,
    value: Value,
    span: Span,
) -> Result<(), Raise> {
    let bucket = user_hash_to_bucket(take_value(machine, span)?, heap, span)?;
    insert_with_hash(heap, dict, key, value, bucket);
    Ok(())
}
