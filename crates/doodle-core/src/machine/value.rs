//! The machine's `Copy` value representation (machine-design §3): [`Value`] and the
//! per-kind slab-index newtypes it carries. A heap-backed value holds only a `u32`
//! slab index (§4), never a Rust reference, so the machine state stays snapshot- and
//! GC-friendly. The [`Instance`](super::Instance) that owns and drives these lives in
//! the parent [`machine`](super) module.

use crate::span::ModuleId;

macro_rules! heap_index {
    ($($name:ident: $doc:literal,)+) => {
        $(
            #[doc = $doc]
            #[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
            pub struct $name(pub u32);
        )+
    };
}

heap_index! {
    BigIntIdx: "Index of a heap bignum in the bigint slab (machine-design §4).",
    StrIdx: "Index of a string in the string slab (machine-design §4).",
    BytesIdx: "Index of a byte string in the bytes slab (machine-design §4).",
    ListIdx: "Index of a list in the list slab (machine-design §4).",
    DictIdx: "Index of a dict in the dict slab (machine-design §4).",
    RecIdx: "Index of a record in the record slab (machine-design §4).",
    CalIdx: "Index of a callable in the callable slab (machine-design §4).",
    TypeIdx: "Index of a type value in the type slab (machine-design §4).",
    FrnIdx: "Index of a foreign value in the foreign slab (machine-design §4).",
}

/// Index of a binding **cell** in the shared cells slab (machine-design §6/§7).
/// A cell is a machine-internal box — a module binding or (later) a closure
/// upvalue — **not** a `Value` variant, so it has no place in the `Value`-oriented
/// index macro above.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct CellIdx(pub u32);

/// A Doodle value (language spec L§4) in the machine's `Copy` representation
/// (machine-design §3).
///
/// Heap-backed variants hold a `u32` slab index (machine-design §4), never a
/// Rust reference. **No `PartialEq`**: value equality is the semantic function
/// of L§4.13 (structural, cycle-safe, cross-numeric-kind), implemented
/// explicitly when the machine core lands; a derived bitwise `==` would be a
/// footgun. `Void` (the L§6.11 procedure-result sentinel) is deliberately not a
/// variant — the result register is `Option<Value>` with `None` = Void, so a
/// Void can never be stored into a data structure by construction.
#[derive(Clone, Copy, Debug)]
pub enum Value {
    /// `nil` (L§4.9).
    Nil,
    /// A boolean (L§4.1).
    Bool(bool),
    /// A machine-word integer — the small-int fast path (L§4.2).
    Int(i64),
    /// A heap bignum, for integers outside `i64` range (L§4.2).
    BigInt(BigIntIdx),
    /// A double-precision float (L§4.3).
    Float(f64),
    /// A string (L§4.4).
    Str(StrIdx),
    /// A byte string (L§4.5).
    Bytes(BytesIdx),
    /// A list (L§4.6).
    List(ListIdx),
    /// A dict (L§4.7).
    Dict(DictIdx),
    /// A record — value or reference; the heap header says which (L§4.14).
    Record(RecIdx),
    /// A callable: `to`, `fn`, or lambda (L§6).
    Callable(CalIdx),
    /// A module value (L§9).
    Module(ModuleId),
    /// A type value: built-in types, record types, and protocols (L§10, L§11).
    Type(TypeIdx),
    /// A foreign (host) value (engine spec E§4.5).
    Foreign(FrnIdx),
}

impl Value {
    /// Returns the integer if this is an `Int`, else `None`.
    pub fn as_int(self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(n),
            _ => None,
        }
    }

    /// Returns the boolean if this is a `Bool`, else `None`.
    pub fn as_bool(self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(b),
            _ => None,
        }
    }

    /// Returns the float if this is a `Float`, else `None`.
    pub fn as_float(self) -> Option<f64> {
        match self {
            Value::Float(x) => Some(x),
            _ => None,
        }
    }

    /// Whether this value is `Nil`.
    pub fn is_nil(self) -> bool {
        matches!(self, Value::Nil)
    }
}
