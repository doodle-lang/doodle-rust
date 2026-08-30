//! The host↔value boundary API (engine spec E§4.3/§4.4): value constructors and
//! typed readers, all mediated by handles (E§4.2).
//!
//! The host never holds a raw [`Value`]; it constructs values with `make_*` (each
//! returns a fresh [`Handle`] the host owns and must [`release`](Instance::release))
//! and reads them with the typed readers (`kind_of`, `as_int`, `string_bytes`, …).
//! A constructed value is interned into the handle table, so it is a GC root that
//! survives collection until released — the same contract as
//! [`retain_result`](Instance::retain_result). Most readers return a scalar or a
//! borrow and mint nothing; [`list_get`](Instance::list_get) is the exception — it
//! returns a **fresh host-owned handle** the host must release, exactly like a `make_*`.
//!
//! Two determinism obligations live on the construction path (E§11): `make_string`
//! normalizes to **NFC** (so every heap string is NFC by construction, MD §5), and
//! `make_float` canonicalizes any NaN to the single canonical NaN (E§4.3, S-28).
//! Host-injected non-finite floats (±∞) are inert data — accepted and stored, with
//! arithmetic on them raising per S-56 — so `make_float` passes them through; only
//! NaN is canonicalized.

use super::{Handle, HandleError, Instance, Value};

/// The language-level kind of a value (engine spec E§4.4), as `kind_of` reports it.
///
/// This is the **language** taxonomy, not the machine's value representation: an
/// integer is [`Kind::Int`] whether it is a machine-word [`Value::Int`] or a heap
/// [`Value::BigInt`] (Doodle has one integer type, L§4.2). Kinds not yet
/// constructible in the demo subset (`Dict`, `Record`, `Module`, `Foreign`) are
/// present so the mapping is total.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Kind {
    /// `nil` (L§4.9).
    Nil,
    /// A boolean (L§4.1).
    Bool,
    /// An integer, of any magnitude (L§4.2).
    Int,
    /// A float (L§4.3).
    Float,
    /// A string (L§4.4).
    String,
    /// A byte string (L§4.5).
    Bytes,
    /// A list (L§4.6).
    List,
    /// A dict (L§4.7).
    Dict,
    /// A record (L§4.14).
    Record,
    /// A callable (L§6).
    Callable,
    /// A module value (L§9).
    Module,
    /// A type value (L§4.12).
    Type,
    /// A foreign (host) value (E§4.5).
    Foreign,
}

/// Why a boundary value operation failed (engine spec E§4.2/§4.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ValueError {
    /// The handle names a freed/reused slot — a use-after-release (or forged/
    /// cross-instance) handle (E§4.2). Mirrors [`HandleError::Stale`].
    Stale,
    /// A typed reader was applied to a value of a different kind (e.g. `as_int` on
    /// a string). Carries the reader's expected kind and the value's actual kind.
    WrongKind {
        /// The kind the reader requires.
        expected: Kind,
        /// The value's actual kind.
        got: Kind,
    },
    /// `as_int` on an integer whose magnitude exceeds host `i64` range (a bignum).
    /// The value is an `Int` (so `kind_of` is [`Kind::Int`]); it just does not fit
    /// the fixed-width host reader.
    IntOutOfRange,
    /// `list_get` with an index past the end of the list (E§4.3).
    IndexOutOfBounds,
    /// `make_string` was given bytes that are not well-formed UTF-8 (E§4.3, S-30). Carries
    /// the byte offset of the first invalid sequence, so the host boundary and Doodle
    /// `decode` (which raises `invalid-utf8`, S-58) name the same position — one story
    /// across the boundary.
    InvalidUtf8 {
        /// Byte offset of the first invalid sequence (`Utf8Error::valid_up_to`).
        position: usize,
    },
    /// `make_int_decimal` was given text that is not a base-10 integer literal — the
    /// arbitrary-precision counterpart of the fixed-width constructors rejecting a
    /// malformed magnitude.
    MalformedInt,
}

impl From<HandleError> for ValueError {
    fn from(_: HandleError) -> Self {
        // The only handle error is staleness; keep it a single boundary error kind.
        ValueError::Stale
    }
}

/// The bit pattern of the single canonical quiet NaN the engine maintains (E§4.3,
/// S-28). Every NaN crossing the boundary is normalized to this so NaN payload/sign
/// bits — hidden platform state — never become observable through hashing,
/// formatting, or byte views (a determinism requirement, E§11).
const CANONICAL_NAN_BITS: u64 = 0x7FF8_0000_0000_0000;

/// Canonicalizes a NaN to the single engine NaN (E§4.3); passes every finite value
/// and ±∞ through unchanged (host-injected ±∞ is inert data, S-56).
fn canonical_float(x: f64) -> f64 {
    if x.is_nan() {
        f64::from_bits(CANONICAL_NAN_BITS)
    } else {
        x
    }
}

/// The [`Kind`] of a machine [`Value`] (E§4.4). `Int`/`BigInt` both map to
/// [`Kind::Int`] — the split is a representation detail, not language-observable.
fn kind_of_value(value: Value) -> Kind {
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

/// Boundary value constructors and readers (engine spec E§4.3/§4.4). Each `make_*`
/// interns its result as a host-owned handle (a GC root until released); the readers
/// generation-check the handle and type-check the value.
impl Instance {
    /// Interns `value` as a fresh host handle (one reference), keeping it reachable
    /// across collections and drives. Shared by every `make_*` (including the
    /// foreign-value slice in [`foreign`](super::foreign)).
    pub(super) fn intern(&mut self, value: Value) -> Handle {
        self.machine.handles.intern(value)
    }

    /// The value a handle names, generation-checked (E§4.2), mapping a stale handle
    /// to [`ValueError::Stale`]. Shared with the foreign-value readers
    /// ([`foreign`](super::foreign)).
    pub(super) fn value_of(&self, handle: Handle) -> Result<Value, ValueError> {
        Ok(self.machine.handles.resolve(handle)?)
    }

    /// Constructs an integer (E§4.3).
    pub fn make_int(&mut self, value: i64) -> Handle {
        self.intern(Value::Int(value))
    }

    /// Constructs an integer of any magnitude from its base-10 text (E§4.3) — the
    /// arbitrary-precision counterpart of [`make_int`]'s host `i64`. Parses `decimal`
    /// and canonicalizes it (a magnitude fitting `i64` is an `Int`, else a heap bignum,
    /// MD §3), so a Doodle integer that overflows `i64` (L§4.2) still crosses the
    /// boundary. Errors with [`ValueError::MalformedInt`] if `decimal` is not a base-10
    /// integer literal.
    pub fn make_int_decimal(&mut self, decimal: &str) -> Result<Handle, ValueError> {
        let n: num_bigint::BigInt = decimal.parse().map_err(|_| ValueError::MalformedInt)?;
        let value = super::arith::int_value(n, &mut self.heap);
        Ok(self.intern(value))
    }

    /// Constructs a boolean (E§4.3).
    pub fn make_bool(&mut self, value: bool) -> Handle {
        self.intern(Value::Bool(value))
    }

    /// Constructs `nil` (E§4.3).
    pub fn make_nil(&mut self) -> Handle {
        self.intern(Value::Nil)
    }

    /// Constructs a float (E§4.3), canonicalizing any NaN to the single engine NaN
    /// (S-28); ±∞ passes through as inert data (S-56).
    pub fn make_float(&mut self, value: f64) -> Handle {
        self.intern(Value::Float(canonical_float(value)))
    }

    /// Constructs a string from UTF-8 `bytes` (E§4.3, normative): validates
    /// well-formed UTF-8 (else [`ValueError::InvalidUtf8`]) and normalizes to NFC.
    /// `string_bytes` round-trips it: for already-NFC input the bytes are returned
    /// unchanged; otherwise the normalized form is stored.
    pub fn make_string(&mut self, bytes: &[u8]) -> Result<Handle, ValueError> {
        let text = std::str::from_utf8(bytes).map_err(|e| ValueError::InvalidUtf8 {
            position: e.valid_up_to(),
        })?;
        let nfc = crate::unicode::nfc(text);
        let idx = self.heap.alloc_string(nfc.into_owned().into_boxed_str());
        Ok(self.intern(Value::Str(idx)))
    }

    /// Constructs a byte string (E§4.3): raw bytes, no encoding or normalization.
    pub fn make_bytes(&mut self, bytes: &[u8]) -> Handle {
        let idx = self.heap.alloc_bytes(bytes.to_vec().into_boxed_slice());
        self.intern(Value::Bytes(idx))
    }

    /// Constructs an empty list (E§4.3); grow it with [`list_append`](Self::list_append).
    pub fn make_list(&mut self) -> Handle {
        let idx = self.heap.alloc_list(Vec::new());
        self.intern(Value::List(idx))
    }

    /// Appends the value named by `value` to the list named by `list` (E§4.3).
    /// Errors if either handle is stale, or `list` does not name a list.
    pub fn list_append(&mut self, list: Handle, value: Handle) -> Result<(), ValueError> {
        let element = self.value_of(value)?;
        let Value::List(idx) = self.value_of(list)? else {
            return Err(self.wrong_kind(list, Kind::List));
        };
        self.heap.list_push(idx, element);
        Ok(())
    }

    /// The [`Kind`] of the value a handle names (E§4.4).
    pub fn kind_of(&self, handle: Handle) -> Result<Kind, ValueError> {
        Ok(kind_of_value(self.value_of(handle)?))
    }

    /// Reads an integer (E§4.3). Errors if the value is not an integer
    /// ([`ValueError::WrongKind`]) or is a bignum beyond `i64`
    /// ([`ValueError::IntOutOfRange`]).
    pub fn as_int(&self, handle: Handle) -> Result<i64, ValueError> {
        match self.value_of(handle)? {
            Value::Int(n) => Ok(n),
            Value::BigInt(_) => Err(ValueError::IntOutOfRange),
            other => Err(wrong_kind(other, Kind::Int)),
        }
    }

    /// Reads an integer of any magnitude as its base-10 text (E§4.3) — the
    /// arbitrary-precision counterpart of [`as_int`], total over machine-word `Int` and
    /// heap `BigInt` (both [`Kind::Int`]). Unlike `as_int`, a bignum does not error with
    /// [`ValueError::IntOutOfRange`]; it renders in full, so a Doodle integer beyond
    /// `i64` (L§4.2) crosses the boundary intact.
    pub fn as_int_decimal(&self, handle: Handle) -> Result<String, ValueError> {
        match self.value_of(handle)? {
            Value::Int(n) => Ok(n.to_string()),
            Value::BigInt(idx) => Ok(self.heap.bigint(idx).value.to_string()),
            other => Err(wrong_kind(other, Kind::Int)),
        }
    }

    /// Reads a boolean (E§4.3).
    pub fn as_bool(&self, handle: Handle) -> Result<bool, ValueError> {
        match self.value_of(handle)? {
            Value::Bool(b) => Ok(b),
            other => Err(wrong_kind(other, Kind::Bool)),
        }
    }

    /// Reads a float (E§4.3). The result is finite or ±∞ or the single canonical
    /// NaN — never a non-canonical NaN (S-28).
    pub fn as_float(&self, handle: Handle) -> Result<f64, ValueError> {
        match self.value_of(handle)? {
            Value::Float(x) => Ok(x),
            other => Err(wrong_kind(other, Kind::Float)),
        }
    }

    /// Whether the value a handle names is `nil` (E§4.3). A live handle of any other
    /// kind returns `Ok(false)`; only a stale handle errors.
    pub fn is_nil(&self, handle: Handle) -> Result<bool, ValueError> {
        Ok(self.value_of(handle)?.is_nil())
    }

    /// The NFC UTF-8 bytes of a string (E§4.3, normative). Zero-copy: borrows the
    /// instance for the returned slice's lifetime. Errors if the value is not a string.
    pub fn string_bytes(&self, handle: Handle) -> Result<&[u8], ValueError> {
        match self.value_of(handle)? {
            Value::Str(idx) => Ok(self.heap.string(idx).utf8.as_bytes()),
            other => Err(wrong_kind(other, Kind::String)),
        }
    }

    /// The raw bytes of a byte string (E§4.3). Errors if the value is not a byte string.
    pub fn as_bytes(&self, handle: Handle) -> Result<&[u8], ValueError> {
        match self.value_of(handle)? {
            Value::Bytes(idx) => Ok(&self.heap.byte_string(idx).bytes),
            other => Err(wrong_kind(other, Kind::Bytes)),
        }
    }

    /// The number of elements in a list (E§4.3). Errors if the value is not a list.
    pub fn list_length(&self, handle: Handle) -> Result<usize, ValueError> {
        match self.value_of(handle)? {
            Value::List(idx) => Ok(self.heap.list(idx).items.len()),
            other => Err(wrong_kind(other, Kind::List)),
        }
    }

    /// A fresh handle to the element at `index` of a list (E§4.3). Errors if the
    /// value is not a list ([`ValueError::WrongKind`]) or `index` is past the end
    /// ([`ValueError::IndexOutOfBounds`]). Unlike the other readers, this mints a
    /// **host-owned** handle (a GC root): the host must [`release`](Self::release) it,
    /// exactly as for a `make_*` result.
    pub fn list_get(&mut self, handle: Handle, index: usize) -> Result<Handle, ValueError> {
        let Value::List(idx) = self.value_of(handle)? else {
            return Err(self.wrong_kind(handle, Kind::List));
        };
        let element = *self
            .heap
            .list(idx)
            .items
            .get(index)
            .ok_or(ValueError::IndexOutOfBounds)?;
        Ok(self.intern(element))
    }

    /// Builds a [`ValueError::WrongKind`] naming a handle's actual kind. Used where a
    /// let-else has already consumed the resolved value (also by `inspect`).
    pub(super) fn wrong_kind(&self, handle: Handle, expected: Kind) -> ValueError {
        match self.value_of(handle) {
            Ok(value) => wrong_kind(value, expected),
            Err(e) => e,
        }
    }
}

/// A [`ValueError::WrongKind`] for `value` where `expected` was required. Shared with
/// the foreign-value readers ([`foreign`](super::foreign)).
pub(super) fn wrong_kind(value: Value, expected: Kind) -> ValueError {
    ValueError::WrongKind {
        expected,
        got: kind_of_value(value),
    }
}

#[cfg(test)]
mod tests;
