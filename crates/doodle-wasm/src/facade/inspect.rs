//! The structural value-inspection surface (engine spec E§4.4/§8.4) on the [`Session`] —
//! the **pure**, Doodle-code-free reads the debugger's value tree renders from. These mirror
//! the native `doodle_core::machine::Instance` inspection API 1:1 (`inspect.rs` + the list
//! readers in `boundary.rs`); the only shaping is `Position → [start, end)` spans and owned
//! `String`s for the JS boundary. Every handle-minting reader hands the host a fresh
//! **host-owned** handle it must [`release`](Session::release).

use super::Session;
use doodle_core::machine::{Handle, ValueError};

impl Session {
    // --- records (E§4.4) ---

    /// The record's declared type name.
    pub fn record_type_name(&self, handle: Handle) -> Result<String, ValueError> {
        self.instance.record_type_name(handle)
    }
    /// The record's field count.
    pub fn record_length(&self, handle: Handle) -> Result<usize, ValueError> {
        self.instance.record_length(handle)
    }
    /// The `index`-th field's name, in declaration order.
    pub fn record_field_name(&self, handle: Handle, index: usize) -> Result<String, ValueError> {
        self.instance
            .record_field_name(handle, index)
            .map(str::to_string)
    }
    /// A fresh host-owned handle to the `index`-th field value.
    pub fn record_field(&mut self, handle: Handle, index: usize) -> Result<Handle, ValueError> {
        self.instance.record_field(handle, index)
    }

    // --- dicts (E§4.4, insertion order L§4.7) ---

    /// The dict's entry count.
    pub fn dict_length(&self, handle: Handle) -> Result<usize, ValueError> {
        self.instance.dict_length(handle)
    }
    /// A fresh host-owned handle to the `index`-th key (insertion order).
    pub fn dict_key(&mut self, handle: Handle, index: usize) -> Result<Handle, ValueError> {
        self.instance.dict_key(handle, index)
    }
    /// A fresh host-owned handle to the `index`-th value (insertion order).
    pub fn dict_value(&mut self, handle: Handle, index: usize) -> Result<Handle, ValueError> {
        self.instance.dict_value(handle, index)
    }

    // --- lists (E§4.4) ---

    /// The list's length.
    pub fn list_length(&self, handle: Handle) -> Result<usize, ValueError> {
        self.instance.list_length(handle)
    }
    /// A fresh host-owned handle to the `index`-th element.
    pub fn list_get(&mut self, handle: Handle, index: usize) -> Result<Handle, ValueError> {
        self.instance.list_get(handle, index)
    }

    // --- callable reflection (E§8.2, D-M6-4 minimal) ---

    /// The callable's declared name, or `None` for an anonymous/sourceless callable.
    pub fn callable_name(&self, handle: Handle) -> Result<Option<String>, ValueError> {
        self.instance.callable_name(handle)
    }
    /// Whether the callable is a `fn` (`true`) or `to` (`false`); `None` if indeterminate.
    pub fn callable_is_function(&self, handle: Handle) -> Result<Option<bool>, ValueError> {
        self.instance.callable_is_function(handle)
    }
    /// The `[start, end)` span of the callable's declaration, or `None` for a sourceless one.
    pub fn callable_position(&self, handle: Handle) -> Result<Option<[u32; 2]>, ValueError> {
        Ok(self
            .instance
            .callable_position(handle)?
            .map(|p| [p.span.start, p.span.end]))
    }
    /// The `[start, end)` span of the callable's docstring, or `None` if it has none.
    pub fn callable_docstring(&self, handle: Handle) -> Result<Option<[u32; 2]>, ValueError> {
        Ok(self
            .instance
            .callable_docstring(handle)?
            .map(|p| [p.span.start, p.span.end]))
    }

    // --- type / module reflection (E§4.4) ---

    /// The type value's name (a built-in spelling, a record type's name, or a protocol's name).
    pub fn type_name(&self, handle: Handle) -> Result<String, ValueError> {
        self.instance.type_name(handle)
    }
    /// A module value's member names, in declaration order.
    pub fn module_member_names(&self, handle: Handle) -> Result<Vec<String>, ValueError> {
        self.instance.module_member_names(handle)
    }
}
