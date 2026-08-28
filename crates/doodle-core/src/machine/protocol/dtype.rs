//! The **runtime type** a value dispatches on (L§10.3): the [`DispatchType`] leaf, the value →
//! leaf mapping ([`dispatch_type_of`]), the type-value → key(s) mapping ([`type_keys`], for
//! `implement … for T`), and the display name for diagnostics. Split from the registry
//! (`mod.rs`) for length; these are the type-classification half of dispatch.

use crate::heap::{CallableTarget, Heap};
use crate::machine::value::TypeIdx;
use crate::machine::{BuiltinType, TypeKind, Value};
use crate::resolve::BodyKind;

/// A value's **runtime type** for dispatch (L§10.3): the concrete leaf type an
/// `implement … for T` registers against and a call dispatches on. Built-in umbrella
/// types (`Number`, `Callable`) are not runtime types — a value is always one leaf — so
/// an `implement … for Number` expands to `Int` and `Float` at registration ([`type_keys`])
/// and a lookup by the value's leaf finds it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DispatchType {
    Nil,
    Bool,
    Int,
    Float,
    Str,
    Bytes,
    List,
    Dict,
    /// A `to` value.
    Procedure,
    /// An `fn` value.
    Function,
    /// A record type, by its nominal type-value identity (L§6.5).
    Record(TypeIdx),
}

/// The runtime-type keys a type *value* covers for `implement … for T`: a concrete
/// built-in is one leaf; an umbrella (`Number`, `Callable`) expands to its leaves; a
/// record type is its nominal identity (L§6.5). A protocol value has no keys (you
/// cannot implement *for* a protocol).
pub(super) fn type_keys(idx: TypeIdx, heap: &Heap) -> Vec<DispatchType> {
    match &heap.type_value(idx).kind {
        TypeKind::Record(_) => vec![DispatchType::Record(idx)],
        TypeKind::Protocol(_) => Vec::new(),
        TypeKind::Builtin(b) => match b {
            BuiltinType::Int => vec![DispatchType::Int],
            BuiltinType::Float => vec![DispatchType::Float],
            BuiltinType::Number => vec![DispatchType::Int, DispatchType::Float],
            BuiltinType::Bool => vec![DispatchType::Bool],
            BuiltinType::String => vec![DispatchType::Str],
            BuiltinType::Bytes => vec![DispatchType::Bytes],
            BuiltinType::Nil => vec![DispatchType::Nil],
            BuiltinType::List => vec![DispatchType::List],
            BuiltinType::Dict => vec![DispatchType::Dict],
            BuiltinType::Procedure => vec![DispatchType::Procedure],
            BuiltinType::Function => vec![DispatchType::Function],
            BuiltinType::Callable => vec![DispatchType::Procedure, DispatchType::Function],
        },
    }
}

/// A value's runtime type for dispatch (L§10.3). Every value has exactly one; a callable's
/// `to`/`fn` split reads the callable's own module (a cross-module dispatch is still keyed
/// by the concrete kind). A bare dispatcher value classifies as a `Function` (provisional,
/// M5.5b — matching [`super::super::types`]).
pub(crate) fn dispatch_type_of(
    value: Value,
    heap: &Heap,
    modules: &[super::super::LoadedModule],
    intrinsics: &super::super::intrinsic::Registry,
) -> DispatchType {
    match value {
        Value::Nil => DispatchType::Nil,
        Value::Bool(_) => DispatchType::Bool,
        Value::Int(_) | Value::BigInt(_) => DispatchType::Int,
        Value::Float(_) => DispatchType::Float,
        Value::Str(_) => DispatchType::Str,
        Value::Bytes(_) => DispatchType::Bytes,
        Value::List(_) => DispatchType::List,
        Value::Dict(_) => DispatchType::Dict,
        Value::Record(r) => DispatchType::Record(heap.record(r).type_idx),
        Value::Callable(cal) => {
            let kind = match heap.callable(cal).target {
                CallableTarget::Source(id) => {
                    let m = heap.callable(cal).module.0 as usize;
                    modules[m].resolved.callables[id as usize].kind
                }
                CallableTarget::Intrinsic(iid) => intrinsics.kind_of(iid),
                CallableTarget::Dispatcher { .. } => BodyKind::Func,
            };
            match kind {
                BodyKind::Proc => DispatchType::Procedure,
                _ => DispatchType::Function,
            }
        }
        // A module, a type value, or a foreign value is not a record/scalar; it has no
        // useful dispatch leaf. No `implement … for` form targets these, so such a value
        // never resolves a member — dispatch reports not-implemented via an unmatched key.
        Value::Module(_) | Value::Type(_) | Value::Foreign(_) => DispatchType::Nil,
    }
}

/// A display name for a runtime type, for diagnostics (L§10.3 messages).
pub(super) fn dispatch_type_name(ty: DispatchType, heap: &Heap) -> String {
    match ty {
        DispatchType::Nil => "Nil".into(),
        DispatchType::Bool => "Bool".into(),
        DispatchType::Int => "Int".into(),
        DispatchType::Float => "Float".into(),
        DispatchType::Str => "String".into(),
        DispatchType::Bytes => "Bytes".into(),
        DispatchType::List => "List".into(),
        DispatchType::Dict => "Dict".into(),
        DispatchType::Procedure => "Procedure".into(),
        DispatchType::Function => "Function".into(),
        DispatchType::Record(idx) => match &heap.type_value(idx).kind {
            TypeKind::Record(rt) => rt.name.to_string(),
            _ => "record".into(),
        },
    }
}
