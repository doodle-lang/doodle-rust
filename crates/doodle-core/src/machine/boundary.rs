//! The host↔value boundary API on [`Instance`] (engine spec E§4.3/§4.4): value constructors
//! and typed readers, all mediated by handles (E§4.2).
//!
//! The host never holds a raw [`Value`]; it constructs values with `make_*` (each returns a
//! fresh [`Handle`] the host owns and must [`release`](Instance::release)) and reads them with
//! the typed readers (`kind_of`, `as_int`, `string_bytes`, …). The operations themselves live in
//! [`values`](super::values) as free functions over the handle table + heap, so the same code
//! serves both this instance surface (between drives) and a foreign callback's
//! [`IntrinsicCtx`](super::IntrinsicCtx) (inside a synchronous call, M7.2b). A constructed value
//! is interned into the handle table, so it is a GC root that survives collection until released.
//!
//! Most readers return a scalar or a borrow and mint nothing; [`list_get`](Instance::list_get)
//! is the exception — it returns a **fresh host-owned handle** the host must release, exactly
//! like a `make_*`.

use super::values;
use super::{Handle, HandleError, Instance, Value};

/// The language-level kind of a value (engine spec E§4.4), as `kind_of` reports it.
///
/// This is the **language** taxonomy, not the machine's value representation: an integer is
/// [`Kind::Int`] whether it is a machine-word [`Value::Int`] or a heap [`Value::BigInt`] (Doodle
/// has one integer type, L§4.2).
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
    /// The handle names a freed/reused slot — a use-after-release (or forged/cross-instance)
    /// handle (E§4.2). Mirrors [`HandleError::Stale`].
    Stale,
    /// A typed reader was applied to a value of a different kind (e.g. `as_int` on a string).
    /// Carries the reader's expected kind and the value's actual kind.
    WrongKind {
        /// The kind the reader requires.
        expected: Kind,
        /// The value's actual kind.
        got: Kind,
    },
    /// `as_int` on an integer whose magnitude exceeds host `i64` range (a bignum). The value is
    /// an `Int` (so `kind_of` is [`Kind::Int`]); it just does not fit the fixed-width host reader.
    IntOutOfRange,
    /// `list_get` with an index past the end of the list (E§4.3).
    IndexOutOfBounds,
    /// `make_string` was given bytes that are not well-formed UTF-8 (E§4.3, S-30). Carries the
    /// byte offset of the first invalid sequence, so the host boundary and Doodle `decode` (which
    /// raises `invalid-utf8`, S-58) name the same position.
    InvalidUtf8 {
        /// Byte offset of the first invalid sequence (`Utf8Error::valid_up_to`).
        position: usize,
    },
    /// `make_int_decimal` was given text that is not a base-10 integer literal.
    MalformedInt,
}

impl From<HandleError> for ValueError {
    fn from(_: HandleError) -> Self {
        // The only handle error is staleness; keep it a single boundary error kind.
        ValueError::Stale
    }
}

/// Boundary value constructors and readers (engine spec E§4.3/§4.4). Each `make_*` interns its
/// result as a host-owned handle (a GC root until released); the readers generation-check the
/// handle and type-check the value. Every method delegates to [`values`], the shared core.
impl Instance {
    /// Interns `value` as a fresh host handle (one reference), keeping it reachable across
    /// collections and drives. Shared by every `make_*` (including the foreign-value slice in
    /// [`foreign`](super::foreign)) and the aux-eval renderer.
    pub(super) fn intern(&mut self, value: Value) -> Handle {
        values::intern(&mut self.machine.handles, value)
    }

    /// The value a handle names, generation-checked (E§4.2), mapping a stale handle to
    /// [`ValueError::Stale`]. Shared with the foreign-value readers ([`foreign`](super::foreign)).
    pub(super) fn value_of(&self, handle: Handle) -> Result<Value, ValueError> {
        values::value_of(&self.machine.handles, handle)
    }

    /// Constructs an integer (E§4.3). Larger magnitudes use
    /// [`make_int_decimal`](Self::make_int_decimal).
    pub fn make_int(&mut self, value: i64) -> Handle {
        values::make_int(&mut self.machine.handles, value)
    }

    /// Constructs an integer of any magnitude from its base-10 text (E§4.3) — the
    /// arbitrary-precision counterpart of [`make_int`](Self::make_int). Errors with
    /// [`ValueError::MalformedInt`] if `decimal` is not a base-10 integer literal.
    pub fn make_int_decimal(&mut self, decimal: &str) -> Result<Handle, ValueError> {
        values::make_int_decimal(&mut self.machine.handles, &mut self.heap, decimal)
    }

    /// Constructs a boolean (E§4.3).
    pub fn make_bool(&mut self, value: bool) -> Handle {
        values::make_bool(&mut self.machine.handles, value)
    }

    /// Constructs `nil` (E§4.3).
    pub fn make_nil(&mut self) -> Handle {
        values::make_nil(&mut self.machine.handles)
    }

    /// Constructs a float (E§4.3), canonicalizing any NaN to the single engine NaN (S-28); ±∞
    /// passes through as inert data (S-56).
    pub fn make_float(&mut self, value: f64) -> Handle {
        values::make_float(&mut self.machine.handles, value)
    }

    /// Constructs a string from UTF-8 `bytes` (E§4.3, normative): validates well-formed UTF-8
    /// (else [`ValueError::InvalidUtf8`]) and normalizes to NFC.
    pub fn make_string(&mut self, bytes: &[u8]) -> Result<Handle, ValueError> {
        values::make_string(&mut self.machine.handles, &mut self.heap, bytes)
    }

    /// Constructs a byte string (E§4.3): raw bytes, no encoding or normalization.
    pub fn make_bytes(&mut self, bytes: &[u8]) -> Handle {
        values::make_bytes(&mut self.machine.handles, &mut self.heap, bytes)
    }

    /// Constructs an empty list (E§4.3); grow it with [`list_append`](Self::list_append).
    pub fn make_list(&mut self) -> Handle {
        values::make_list(&mut self.machine.handles, &mut self.heap)
    }

    /// Appends the value named by `value` to the list named by `list` (E§4.3). Errors if either
    /// handle is stale, or `list` does not name a list.
    pub fn list_append(&mut self, list: Handle, value: Handle) -> Result<(), ValueError> {
        values::list_append(&self.machine.handles, &mut self.heap, list, value)
    }

    /// The [`Kind`] of the value a handle names (E§4.4).
    pub fn kind_of(&self, handle: Handle) -> Result<Kind, ValueError> {
        values::kind_of(&self.machine.handles, handle)
    }

    /// Reads an integer (E§4.3). Errors if the value is not an integer ([`ValueError::WrongKind`])
    /// or is a bignum beyond `i64` ([`ValueError::IntOutOfRange`]).
    pub fn as_int(&self, handle: Handle) -> Result<i64, ValueError> {
        values::as_int(&self.machine.handles, handle)
    }

    /// Reads an integer of any magnitude as its base-10 text (E§4.3) — total over machine-word
    /// `Int` and heap `BigInt`. Unlike `as_int`, a bignum renders in full rather than erroring.
    pub fn as_int_decimal(&self, handle: Handle) -> Result<String, ValueError> {
        values::as_int_decimal(&self.machine.handles, &self.heap, handle)
    }

    /// Reads a boolean (E§4.3).
    pub fn as_bool(&self, handle: Handle) -> Result<bool, ValueError> {
        values::as_bool(&self.machine.handles, handle)
    }

    /// Reads a float (E§4.3). The result is finite, ±∞, or the single canonical NaN (S-28).
    pub fn as_float(&self, handle: Handle) -> Result<f64, ValueError> {
        values::as_float(&self.machine.handles, handle)
    }

    /// Whether the value a handle names is `nil` (E§4.3). Any other live handle returns
    /// `Ok(false)`; only a stale handle errors.
    pub fn is_nil(&self, handle: Handle) -> Result<bool, ValueError> {
        values::is_nil(&self.machine.handles, handle)
    }

    /// The NFC UTF-8 bytes of a string (E§4.3, normative). Zero-copy: borrows the instance for
    /// the returned slice's lifetime. Errors if the value is not a string.
    pub fn string_bytes(&self, handle: Handle) -> Result<&[u8], ValueError> {
        values::string_bytes(&self.machine.handles, &self.heap, handle)
    }

    /// The raw bytes of a byte string (E§4.3). Errors if the value is not a byte string.
    pub fn as_bytes(&self, handle: Handle) -> Result<&[u8], ValueError> {
        values::as_bytes(&self.machine.handles, &self.heap, handle)
    }

    /// The number of elements in a list (E§4.3). Errors if the value is not a list.
    pub fn list_length(&self, handle: Handle) -> Result<usize, ValueError> {
        values::list_length(&self.machine.handles, &self.heap, handle)
    }

    /// A fresh handle to the element at `index` of a list (E§4.3). Errors if the value is not a
    /// list (`WrongKind`) or `index` is past the end (`IndexOutOfBounds`). Mints a **host-owned**
    /// handle (a GC root): the host must [`release`](Self::release) it.
    pub fn list_get(&mut self, handle: Handle, index: usize) -> Result<Handle, ValueError> {
        values::list_get(&mut self.machine.handles, &self.heap, handle, index)
    }

    /// Builds a [`ValueError::WrongKind`] naming a handle's actual kind. Used where a let-else has
    /// already consumed the resolved value (also by `inspect`).
    pub(super) fn wrong_kind(&self, handle: Handle, expected: Kind) -> ValueError {
        match self.value_of(handle) {
            Ok(value) => values::wrong_kind(value, expected),
            Err(e) => e,
        }
    }
}

#[cfg(test)]
mod tests;
