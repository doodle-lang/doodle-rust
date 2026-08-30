//! Structural value inspection (engine spec E§4.4/§8.4): the **pure**, Doodle-code-free
//! reads a debugger renders program state from — a record's type and fields, a dict's
//! entries (insertion order, L§4.7), a callable's reflection (name / `to`-`fn` kind /
//! declaration position / docstring), a type's name, and a module's members. Handle-minting
//! readers follow the `list_get` discipline (host-owned; the host must
//! [`release`](Instance::release) them); the pure readers borrow the instance. Split from
//! `boundary.rs` (the value constructors + scalar/list readers) for length.

use super::boundary::{Kind, ValueError};
use super::observe::Position;
use super::{Handle, Instance, TypeKind, Value, types};
use crate::ast::{Node, NodeId};
use crate::heap::CallableTarget;
use crate::resolve::BodyKind;
use crate::span::ModuleId;

impl Instance {
    /// The declared **type name** of a record value (E§4.4): its nominal record type (L§9).
    pub fn record_type_name(&self, handle: Handle) -> Result<String, ValueError> {
        let Value::Record(r) = self.value_of(handle)? else {
            return Err(self.wrong_kind(handle, Kind::Record));
        };
        match &self.heap.type_value(self.heap.record(r).type_idx).kind {
            TypeKind::Record(rt) => Ok(rt.name.to_string()),
            _ => unreachable!("a record's type is a record type"),
        }
    }

    /// The number of fields a record has (E§4.4).
    pub fn record_length(&self, handle: Handle) -> Result<usize, ValueError> {
        let Value::Record(r) = self.value_of(handle)? else {
            return Err(self.wrong_kind(handle, Kind::Record));
        };
        Ok(self.heap.record(r).fields.len())
    }

    /// The name of a record's `index`-th field, in declaration order (E§4.4). Errors with
    /// [`ValueError::IndexOutOfBounds`] past the last field. Borrows the instance for the
    /// returned slice's lifetime (like [`string_bytes`](Instance::string_bytes)).
    pub fn record_field_name(&self, handle: Handle, index: usize) -> Result<&str, ValueError> {
        let Value::Record(r) = self.value_of(handle)? else {
            return Err(self.wrong_kind(handle, Kind::Record));
        };
        match &self.heap.type_value(self.heap.record(r).type_idx).kind {
            TypeKind::Record(rt) => rt
                .fields
                .get(index)
                .map(std::convert::AsRef::as_ref)
                .ok_or(ValueError::IndexOutOfBounds),
            _ => unreachable!("a record's type is a record type"),
        }
    }

    /// A fresh handle to a record's `index`-th field **value** (E§4.4) — host-owned (a GC
    /// root; the host must [`release`](Instance::release) it). Errors past the last field.
    pub fn record_field(&mut self, handle: Handle, index: usize) -> Result<Handle, ValueError> {
        let Value::Record(r) = self.value_of(handle)? else {
            return Err(self.wrong_kind(handle, Kind::Record));
        };
        let value = *self
            .heap
            .record(r)
            .fields
            .get(index)
            .ok_or(ValueError::IndexOutOfBounds)?;
        Ok(self.intern(value))
    }

    /// The number of entries in a dict (E§4.4).
    pub fn dict_length(&self, handle: Handle) -> Result<usize, ValueError> {
        let Value::Dict(d) = self.value_of(handle)? else {
            return Err(self.wrong_kind(handle, Kind::Dict));
        };
        Ok(self.heap.dict(d).entries.len())
    }

    /// A fresh handle to the `index`-th **key**, in insertion order (E§4.4, L§4.7) —
    /// host-owned. Errors past the last entry.
    pub fn dict_key(&mut self, handle: Handle, index: usize) -> Result<Handle, ValueError> {
        let Value::Dict(d) = self.value_of(handle)? else {
            return Err(self.wrong_kind(handle, Kind::Dict));
        };
        let key = self
            .heap
            .dict(d)
            .entries
            .get(index)
            .ok_or(ValueError::IndexOutOfBounds)?
            .0;
        Ok(self.intern(key))
    }

    /// A fresh handle to the `index`-th **value**, in insertion order (E§4.4, L§4.7) —
    /// host-owned. Errors past the last entry.
    pub fn dict_value(&mut self, handle: Handle, index: usize) -> Result<Handle, ValueError> {
        let Value::Dict(d) = self.value_of(handle)? else {
            return Err(self.wrong_kind(handle, Kind::Dict));
        };
        let value = self
            .heap
            .dict(d)
            .entries
            .get(index)
            .ok_or(ValueError::IndexOutOfBounds)?
            .1;
        Ok(self.intern(value))
    }

    /// The declared **name** of a callable (E§8.2, L§13): a named `to`/`fn`'s name, or
    /// `None` for an anonymous `fn` or a callable with no source declaration (an intrinsic
    /// or a protocol dispatcher). Minimal engine-level reflection — the Doodle `help`
    /// stdlib is M9a (D-M6-4).
    pub fn callable_name(&self, handle: Handle) -> Result<Option<String>, ValueError> {
        Ok(self.callable_source(handle)?.and_then(|(m, decl)| {
            match self.modules[m].resolved.ast.node(decl) {
                Node::Callable { name, .. } => name.as_ref().map(|n| n.to_string()),
                _ => None,
            }
        }))
    }

    /// Whether a callable is a **function** (`fn`, yields a value) rather than a
    /// **procedure** (`to`), per S-37 — for the stack panel's kind badge. Total over the
    /// three callable targets (source, intrinsic, dispatcher); `None` only if a dispatcher
    /// member's kind is indeterminate.
    pub fn callable_is_function(&self, handle: Handle) -> Result<Option<bool>, ValueError> {
        let Value::Callable(cal) = self.value_of(handle)? else {
            return Err(self.wrong_kind(handle, Kind::Callable));
        };
        let obj = self.heap.callable(cal);
        let kind = match obj.target {
            CallableTarget::Source(id) => {
                Some(self.modules[obj.module.0 as usize].resolved.callables[id as usize].kind)
            }
            CallableTarget::Intrinsic(iid) => Some(self.machine.intrinsics.kind_of(iid)),
            CallableTarget::Dispatcher { member, .. } => self.machine.protocols.member_kind(member),
        };
        Ok(kind.map(|k| matches!(k, BodyKind::Func)))
    }

    /// The source **declaration position** of a callable (E§8.2): the span of its `to`/`fn`
    /// declaration, or `None` for a callable with no source (an intrinsic or a dispatcher).
    pub fn callable_position(&self, handle: Handle) -> Result<Option<Position>, ValueError> {
        Ok(self.callable_source(handle)?.map(|(m, decl)| Position {
            module: self.modules[m].resolved.canonical_id,
            span: self.modules[m].resolved.ast.span(decl),
        }))
    }

    /// The **docstring** position of a callable (L§8.6, E§8.2), if it has one: the raw
    /// source span of its leading body string. `None` for no docstring or no source.
    pub fn callable_docstring(&self, handle: Handle) -> Result<Option<Position>, ValueError> {
        Ok(self.callable_source(handle)?.and_then(|(m, decl)| {
            match self.modules[m].resolved.ast.node(decl) {
                Node::Callable { doc, .. } => doc.map(|span| Position {
                    module: self.modules[m].resolved.canonical_id,
                    span,
                }),
                _ => None,
            }
        }))
    }

    /// The **name** of a type value (E§4.4): a built-in's spelling (`Int`/`String`/…), a
    /// record type's declared name, or a protocol's name (L§4.12, L§9, L§10).
    pub fn type_name(&self, handle: Handle) -> Result<String, ValueError> {
        let Value::Type(idx) = self.value_of(handle)? else {
            return Err(self.wrong_kind(handle, Kind::Type));
        };
        Ok(match &self.heap.type_value(idx).kind {
            TypeKind::Builtin(b) => types::BUILTINS
                .iter()
                .find(|(_, bt)| bt == b)
                .map_or_else(|| "Type".to_string(), |(n, _)| (*n).to_string()),
            TypeKind::Record(rt) => rt.name.to_string(),
            TypeKind::Protocol(pt) => pt.name.to_string(),
        })
    }

    /// The **member names** of a module value (E§4.4, L§11): its module-level declarations,
    /// in declaration order — what the debugger's value tree lists under a module.
    pub fn module_member_names(&self, handle: Handle) -> Result<Vec<String>, ValueError> {
        let Value::Module(mid) = self.value_of(handle)? else {
            return Err(self.wrong_kind(handle, Kind::Module));
        };
        Ok(self.modules[mid.0 as usize]
            .resolved
            .globals
            .iter()
            .map(|g| g.name.to_string())
            .collect())
    }

    /// The `(module index, decl node)` of a **source** callable, or `None` for an intrinsic
    /// or dispatcher (no source declaration). Errors if the handle is not a callable.
    fn callable_source(&self, handle: Handle) -> Result<Option<(usize, NodeId)>, ValueError> {
        let Value::Callable(cal) = self.value_of(handle)? else {
            return Err(self.wrong_kind(handle, Kind::Callable));
        };
        let obj = self.heap.callable(cal);
        Ok(match obj.target {
            CallableTarget::Source(id) => {
                let ModuleId(m) = obj.module;
                Some((
                    m as usize,
                    self.modules[m as usize].resolved.callables[id as usize].decl,
                ))
            }
            CallableTarget::Intrinsic(_) | CallableTarget::Dispatcher { .. } => None,
        })
    }
}
