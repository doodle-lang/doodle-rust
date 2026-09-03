//! The value/handle boundary API on a foreign-callback activation ([`IntrinsicCtx`], engine spec
//! E§4.3/§4.4/§4.5, M7.2b): the same `make_*`/`as_*`/`release` a host reaches through the
//! [`Instance`](crate::machine::Instance) between drives, but **inside** a synchronous callback —
//! operating on the ctx's live machine + heap through the shared [`values`](crate::machine::values)
//! core, so a value operation runs the identical code either way and never forms a second
//! `&mut Instance`. Each `make_*`/`list_get` returns a **host-owned** handle the callback
//! [`release`](IntrinsicCtx::release)s (like the instance surface). Split from `ctx.rs` for length.

use super::IntrinsicCtx;
use crate::heap::Finalizer;
use crate::machine::boundary::{Kind, ValueError};
use crate::machine::values;
use crate::machine::{Handle, HandleError};

/// The value-boundary methods a foreign callback uses (E§4.3/§4.4/§4.5). Constructors and
/// `release`/`list_get` take `&mut self` (they mutate the handle table/heap); the pure readers
/// take `&self`. Every method delegates to [`values`], the shared core.
impl IntrinsicCtx<'_> {
    /// Constructs an integer (E§4.3).
    pub fn make_int(&mut self, value: i64) -> Handle {
        values::make_int(&mut self.machine.handles, value)
    }

    /// Constructs an integer of any magnitude from base-10 `decimal` (E§4.3), or
    /// [`ValueError::MalformedInt`] if it is not a base-10 integer literal.
    pub fn make_int_decimal(&mut self, decimal: &str) -> Result<Handle, ValueError> {
        values::make_int_decimal(&mut self.machine.handles, &mut *self.heap, decimal)
    }

    /// Constructs a float (E§4.3), canonicalizing any NaN to the single engine NaN (S-28).
    pub fn make_float(&mut self, value: f64) -> Handle {
        values::make_float(&mut self.machine.handles, value)
    }

    /// Constructs a boolean (E§4.3).
    pub fn make_bool(&mut self, value: bool) -> Handle {
        values::make_bool(&mut self.machine.handles, value)
    }

    /// Constructs `nil` (E§4.3).
    pub fn make_nil(&mut self) -> Handle {
        values::make_nil(&mut self.machine.handles)
    }

    /// Constructs a string from UTF-8 `bytes` (E§4.3): validates UTF-8 (else
    /// [`ValueError::InvalidUtf8`]) and normalizes to NFC.
    pub fn make_string(&mut self, bytes: &[u8]) -> Result<Handle, ValueError> {
        values::make_string(&mut self.machine.handles, &mut *self.heap, bytes)
    }

    /// Constructs a byte string (E§4.3): raw bytes, no encoding or normalization.
    pub fn make_bytes(&mut self, bytes: &[u8]) -> Handle {
        values::make_bytes(&mut self.machine.handles, &mut *self.heap, bytes)
    }

    /// Constructs an empty list (E§4.3); grow it with [`list_append`](Self::list_append).
    pub fn make_list(&mut self) -> Handle {
        values::make_list(&mut self.machine.handles, &mut *self.heap)
    }

    /// Appends the value named by `value` to the list named by `list` (E§4.3).
    pub fn list_append(&mut self, list: Handle, value: Handle) -> Result<(), ValueError> {
        values::list_append(&self.machine.handles, &mut *self.heap, list, value)
    }

    /// Constructs a foreign (host) value (E§4.5): an opaque `tag`/`ptr` with an exactly-once
    /// `finalizer`.
    pub fn make_foreign(&mut self, tag: u64, ptr: u64, finalizer: Option<Finalizer>) -> Handle {
        values::make_foreign(
            &mut self.machine.handles,
            &mut *self.heap,
            tag,
            ptr,
            finalizer,
        )
    }

    /// A fresh **host-owned** handle to the element at `index` of a list (E§4.3): errors if the
    /// value is not a list ([`ValueError::WrongKind`]) or `index` is past the end.
    pub fn list_get(&mut self, handle: Handle, index: usize) -> Result<Handle, ValueError> {
        values::list_get(&mut self.machine.handles, &*self.heap, handle, index)
    }

    /// Releases a host-owned handle (E§4.2): decrements its reference count, freeing the slot at
    /// zero. The stale-handle source error if already freed.
    pub fn release(&mut self, handle: Handle) -> Result<(), HandleError> {
        values::release(&mut self.machine.handles, handle)
    }

    /// The [`Kind`] of the value a handle names (E§4.4).
    pub fn kind_of(&self, handle: Handle) -> Result<Kind, ValueError> {
        values::kind_of(&self.machine.handles, handle)
    }

    /// Reads an integer (E§4.3): [`ValueError::WrongKind`] for a non-int,
    /// [`ValueError::IntOutOfRange`] for a bignum beyond `i64`.
    pub fn as_int(&self, handle: Handle) -> Result<i64, ValueError> {
        values::as_int(&self.machine.handles, handle)
    }

    /// Reads an integer of any magnitude as base-10 text (E§4.3), over `Int` and `BigInt`.
    pub fn as_int_decimal(&self, handle: Handle) -> Result<String, ValueError> {
        values::as_int_decimal(&self.machine.handles, &*self.heap, handle)
    }

    /// Reads a boolean (E§4.3).
    pub fn as_bool(&self, handle: Handle) -> Result<bool, ValueError> {
        values::as_bool(&self.machine.handles, handle)
    }

    /// Reads a float (E§4.3): finite, ±∞, or the single canonical NaN.
    pub fn as_float(&self, handle: Handle) -> Result<f64, ValueError> {
        values::as_float(&self.machine.handles, handle)
    }

    /// Whether the value a handle names is `nil` (E§4.3); only a stale handle errors.
    pub fn is_nil(&self, handle: Handle) -> Result<bool, ValueError> {
        values::is_nil(&self.machine.handles, handle)
    }

    /// The NFC UTF-8 bytes of a string (E§4.3). Borrows the ctx for the returned slice.
    pub fn string_bytes(&self, handle: Handle) -> Result<&[u8], ValueError> {
        values::string_bytes(&self.machine.handles, &*self.heap, handle)
    }

    /// The raw bytes of a byte string (E§4.3). Borrows the ctx for the returned slice.
    pub fn as_bytes(&self, handle: Handle) -> Result<&[u8], ValueError> {
        values::as_bytes(&self.machine.handles, &*self.heap, handle)
    }

    /// The number of elements in a list (E§4.3).
    pub fn list_length(&self, handle: Handle) -> Result<usize, ValueError> {
        values::list_length(&self.machine.handles, &*self.heap, handle)
    }

    /// The host type `tag` of a foreign value (E§4.5).
    pub fn foreign_tag(&self, handle: Handle) -> Result<u64, ValueError> {
        values::foreign_tag(&self.machine.handles, &*self.heap, handle)
    }

    /// The opaque host `ptr` of a foreign value (E§4.5), returned verbatim.
    pub fn foreign_ptr(&self, handle: Handle) -> Result<u64, ValueError> {
        values::foreign_ptr(&self.machine.handles, &*self.heap, handle)
    }
}
