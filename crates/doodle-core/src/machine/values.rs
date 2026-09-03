//! The shared value/handle boundary operations (engine spec E§4.3/§4.4/§4.5): the constructors,
//! typed readers, and `release`, as free functions over `(&HandleTable, &Heap)` (readers) or
//! `(&mut HandleTable, &mut Heap)` (constructors). Both the **host** boundary ([`Instance`] in
//! `boundary.rs`/`foreign.rs`) and a **foreign-callback** activation ([`IntrinsicCtx`]) delegate
//! here, so a value operation reads and writes the same handle table and heap whether the host
//! reaches it through the instance (between drives) or through the ctx (inside a synchronous
//! callback) — one implementation, no second `&mut Instance` (E§5.2, M7.2b).
//!
//! Two determinism obligations live on the construction path (E§11): [`make_string`] normalizes
//! to **NFC** and [`make_float`] canonicalizes any NaN to the single canonical NaN (S-28).

use super::boundary::{Kind, ValueError};
use super::handle::{Handle, HandleError, HandleTable};
use super::value::Value;
use crate::heap::{Finalizer, Heap};

/// The bit pattern of the single canonical quiet NaN the engine maintains (E§4.3, S-28). Every
/// NaN crossing the boundary is normalized to this so NaN payload/sign bits — hidden platform
/// state — never become observable through hashing, formatting, or byte views (E§11).
pub(crate) const CANONICAL_NAN_BITS: u64 = 0x7FF8_0000_0000_0000;

/// Canonicalizes a NaN to the single engine NaN (E§4.3); passes every finite value and ±∞
/// through unchanged (host-injected ±∞ is inert data, S-56). Shared with
/// [`native::materialize_const`](super::native::materialize_const), the other host
/// float-injection point.
pub(crate) fn canonical_float(x: f64) -> f64 {
    if x.is_nan() {
        f64::from_bits(CANONICAL_NAN_BITS)
    } else {
        x
    }
}

/// The [`Kind`] of a machine [`Value`] (E§4.4). `Int`/`BigInt` both map to [`Kind::Int`] — the
/// split is a representation detail, not language-observable.
pub(crate) fn kind_of_value(value: Value) -> Kind {
    match value {
        Value::Nil => Kind::Nil,
        Value::Bool(_) => Kind::Bool,
        Value::Int(_) | Value::BigInt(_) => Kind::Int,
        Value::Float(_) => Kind::Float,
        Value::Str(_) => Kind::String,
        Value::Bytes(_) => Kind::Bytes,
        Value::List(_) => Kind::List,
        Value::Dict(_) => Kind::Dict,
        Value::Record(_) => Kind::Record,
        Value::Callable(_) => Kind::Callable,
        Value::Module(_) => Kind::Module,
        Value::Type(_) => Kind::Type,
        Value::Foreign(_) => Kind::Foreign,
    }
}

/// A [`ValueError::WrongKind`] for `value` where `expected` was required.
pub(crate) fn wrong_kind(value: Value, expected: Kind) -> ValueError {
    ValueError::WrongKind {
        expected,
        got: kind_of_value(value),
    }
}

/// Interns `value` as a fresh host handle (one reference), a GC root until released.
pub(crate) fn intern(handles: &mut HandleTable, value: Value) -> Handle {
    handles.intern(value)
}

/// The value a handle names, generation-checked (E§4.2), mapping a stale handle to
/// [`ValueError::Stale`].
pub(crate) fn value_of(handles: &HandleTable, handle: Handle) -> Result<Value, ValueError> {
    Ok(handles.resolve(handle)?)
}

/// Constructs an integer (E§4.3).
pub(crate) fn make_int(handles: &mut HandleTable, value: i64) -> Handle {
    intern(handles, Value::Int(value))
}

/// Constructs an integer of any magnitude from base-10 `decimal` (E§4.3), or
/// [`ValueError::MalformedInt`] if it is not a base-10 integer literal.
pub(crate) fn make_int_decimal(
    handles: &mut HandleTable,
    heap: &mut Heap,
    decimal: &str,
) -> Result<Handle, ValueError> {
    let n: num_bigint::BigInt = decimal.parse().map_err(|_| ValueError::MalformedInt)?;
    let value = super::arith::int_value(n, heap);
    Ok(intern(handles, value))
}

/// Constructs a float (E§4.3), canonicalizing any NaN to the single engine NaN (S-28).
pub(crate) fn make_float(handles: &mut HandleTable, value: f64) -> Handle {
    intern(handles, Value::Float(canonical_float(value)))
}

/// Constructs a boolean (E§4.3).
pub(crate) fn make_bool(handles: &mut HandleTable, value: bool) -> Handle {
    intern(handles, Value::Bool(value))
}

/// Constructs `nil` (E§4.3).
pub(crate) fn make_nil(handles: &mut HandleTable) -> Handle {
    intern(handles, Value::Nil)
}

/// Constructs a string from UTF-8 `bytes` (E§4.3): validates well-formed UTF-8 (else
/// [`ValueError::InvalidUtf8`]) and normalizes to NFC.
pub(crate) fn make_string(
    handles: &mut HandleTable,
    heap: &mut Heap,
    bytes: &[u8],
) -> Result<Handle, ValueError> {
    let text = std::str::from_utf8(bytes).map_err(|e| ValueError::InvalidUtf8 {
        position: e.valid_up_to(),
    })?;
    let nfc = crate::unicode::nfc(text);
    let idx = heap.alloc_string(nfc.into_owned().into_boxed_str());
    Ok(intern(handles, Value::Str(idx)))
}

/// Constructs a byte string (E§4.3): raw bytes, no encoding or normalization.
pub(crate) fn make_bytes(handles: &mut HandleTable, heap: &mut Heap, bytes: &[u8]) -> Handle {
    let idx = heap.alloc_bytes(bytes.to_vec().into_boxed_slice());
    intern(handles, Value::Bytes(idx))
}

/// Constructs an empty list (E§4.3); grow it with [`list_append`].
pub(crate) fn make_list(handles: &mut HandleTable, heap: &mut Heap) -> Handle {
    let idx = heap.alloc_list(Vec::new());
    intern(handles, Value::List(idx))
}

/// Appends the value named by `value` to the list named by `list` (E§4.3). Errors if either
/// handle is stale, or `list` does not name a list.
pub(crate) fn list_append(
    handles: &HandleTable,
    heap: &mut Heap,
    list: Handle,
    value: Handle,
) -> Result<(), ValueError> {
    let element = value_of(handles, value)?;
    let Value::List(idx) = value_of(handles, list)? else {
        return Err(wrong_kind(value_of(handles, list)?, Kind::List));
    };
    heap.list_push(idx, element);
    Ok(())
}

/// Constructs a foreign (host) value (E§4.5): an opaque `tag`/`ptr` with an exactly-once
/// `finalizer`.
pub(crate) fn make_foreign(
    handles: &mut HandleTable,
    heap: &mut Heap,
    tag: u64,
    ptr: u64,
    finalizer: Option<Finalizer>,
) -> Handle {
    let idx = heap.alloc_foreign(tag, ptr, finalizer);
    intern(handles, Value::Foreign(idx))
}

/// Releases a host handle (E§4.2): decrements its reference count, freeing the slot at zero.
pub(crate) fn release(handles: &mut HandleTable, handle: Handle) -> Result<(), HandleError> {
    handles.release(handle)
}

/// The [`Kind`] of the value a handle names (E§4.4).
pub(crate) fn kind_of(handles: &HandleTable, handle: Handle) -> Result<Kind, ValueError> {
    Ok(kind_of_value(value_of(handles, handle)?))
}

/// Reads an integer (E§4.3): `WrongKind` for a non-int, `IntOutOfRange` for a bignum
/// beyond `i64`.
pub(crate) fn as_int(handles: &HandleTable, handle: Handle) -> Result<i64, ValueError> {
    match value_of(handles, handle)? {
        Value::Int(n) => Ok(n),
        Value::BigInt(_) => Err(ValueError::IntOutOfRange),
        other => Err(wrong_kind(other, Kind::Int)),
    }
}

/// Reads an integer of any magnitude as base-10 text (E§4.3) — total over `Int` and `BigInt`.
pub(crate) fn as_int_decimal(
    handles: &HandleTable,
    heap: &Heap,
    handle: Handle,
) -> Result<String, ValueError> {
    match value_of(handles, handle)? {
        Value::Int(n) => Ok(n.to_string()),
        Value::BigInt(idx) => Ok(heap.bigint(idx).value.to_string()),
        other => Err(wrong_kind(other, Kind::Int)),
    }
}

/// Reads a boolean (E§4.3).
pub(crate) fn as_bool(handles: &HandleTable, handle: Handle) -> Result<bool, ValueError> {
    match value_of(handles, handle)? {
        Value::Bool(b) => Ok(b),
        other => Err(wrong_kind(other, Kind::Bool)),
    }
}

/// Reads a float (E§4.3): finite, ±∞, or the single canonical NaN.
pub(crate) fn as_float(handles: &HandleTable, handle: Handle) -> Result<f64, ValueError> {
    match value_of(handles, handle)? {
        Value::Float(x) => Ok(x),
        other => Err(wrong_kind(other, Kind::Float)),
    }
}

/// Whether the value a handle names is `nil` (E§4.3); only a stale handle errors.
pub(crate) fn is_nil(handles: &HandleTable, handle: Handle) -> Result<bool, ValueError> {
    Ok(value_of(handles, handle)?.is_nil())
}

/// The NFC UTF-8 bytes of a string (E§4.3). Borrows `heap` for the returned slice's lifetime.
pub(crate) fn string_bytes<'a>(
    handles: &HandleTable,
    heap: &'a Heap,
    handle: Handle,
) -> Result<&'a [u8], ValueError> {
    match value_of(handles, handle)? {
        Value::Str(idx) => Ok(heap.string(idx).utf8.as_bytes()),
        other => Err(wrong_kind(other, Kind::String)),
    }
}

/// The raw bytes of a byte string (E§4.3). Borrows `heap` for the returned slice's lifetime.
pub(crate) fn as_bytes<'a>(
    handles: &HandleTable,
    heap: &'a Heap,
    handle: Handle,
) -> Result<&'a [u8], ValueError> {
    match value_of(handles, handle)? {
        Value::Bytes(idx) => Ok(&heap.byte_string(idx).bytes),
        other => Err(wrong_kind(other, Kind::Bytes)),
    }
}

/// The number of elements in a list (E§4.3).
pub(crate) fn list_length(
    handles: &HandleTable,
    heap: &Heap,
    handle: Handle,
) -> Result<usize, ValueError> {
    match value_of(handles, handle)? {
        Value::List(idx) => Ok(heap.list(idx).items.len()),
        other => Err(wrong_kind(other, Kind::List)),
    }
}

/// A fresh **host-owned** handle to the element at `index` of a list (E§4.3): [`ValueError::
/// WrongKind`] for a non-list, [`ValueError::IndexOutOfBounds`] past the end. The host releases it.
pub(crate) fn list_get(
    handles: &mut HandleTable,
    heap: &Heap,
    handle: Handle,
    index: usize,
) -> Result<Handle, ValueError> {
    let Value::List(idx) = value_of(handles, handle)? else {
        return Err(wrong_kind(value_of(handles, handle)?, Kind::List));
    };
    let element = *heap
        .list(idx)
        .items
        .get(index)
        .ok_or(ValueError::IndexOutOfBounds)?;
    Ok(intern(handles, element))
}

/// The host type `tag` of a foreign value (E§4.5). `WrongKind` if not a foreign value.
pub(crate) fn foreign_tag(
    handles: &HandleTable,
    heap: &Heap,
    handle: Handle,
) -> Result<u64, ValueError> {
    match value_of(handles, handle)? {
        Value::Foreign(idx) => Ok(heap.foreign(idx).tag),
        other => Err(wrong_kind(other, Kind::Foreign)),
    }
}

/// The opaque host `ptr` of a foreign value (E§4.5), returned verbatim.
pub(crate) fn foreign_ptr(
    handles: &HandleTable,
    heap: &Heap,
    handle: Handle,
) -> Result<u64, ValueError> {
    match value_of(handles, handle)? {
        Value::Foreign(idx) => Ok(heap.foreign(idx).ptr),
        other => Err(wrong_kind(other, Kind::Foreign)),
    }
}
