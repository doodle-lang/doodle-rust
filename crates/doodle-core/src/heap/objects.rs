//! The heap object types (machine-design §4): one struct per heap kind, holding
//! that kind's payload. A heap [`Value`](crate::machine::Value) carries only a `u32`
//! slab index into the owning [`Slab`](super::Slab); these are the objects those
//! indices name. Payload accounting (per-kind size, GC sweep) lives in the parent
//! [`heap`](super) module, which owns the [`Heap`](super::Heap) itself.

use crate::machine::{BuiltinType, CellIdx, Value};
use crate::span::ModuleId;
use num_bigint::BigInt;

/// A string object: its UTF-8 payload, **NFC by construction** (MD §5) — every
/// construction path (literal decode, concat seam pass, `make_string`) produces
/// NFC, so consumers never re-normalize. The lazy grapheme memo (MD §5) joins
/// with the M4 grapheme operations.
#[derive(Clone, Debug)]
pub struct StrObj {
    /// The NFC UTF-8 bytes.
    pub utf8: Box<str>,
}

/// A byte-string object (L§4.5): an immutable sequence of bytes, distinct from a
/// text string (no encoding, O(1) indexing).
#[derive(Clone, Debug)]
pub struct BytesObj {
    /// The raw bytes.
    pub bytes: Box<[u8]>,
}

/// A list object (L§4.6): an ordered, growable sequence of values.
#[derive(Clone, Debug)]
pub struct ListObj {
    /// The elements, in order.
    pub items: Vec<Value>,
}

/// A heap bignum (L§4.2): an integer outside `i64` range. The **canonical-int
/// invariant** (MD §3) means any value fitting `i64` is a [`Value::Int`], never a
/// `BigInt` — so a `BigIntObj`'s magnitude always exceeds `i64` range, and
/// `Int`↔`BigInt` comparison never appears on the equality/hash paths.
#[derive(Clone, Debug)]
pub struct BigIntObj {
    /// The arbitrary-precision value.
    pub value: BigInt,
}

/// A binding **cell** (machine-design §6/§7): a mutable box holding one value —
/// a module-level binding, and later a closure upvalue. `value` is `None` while
/// the binding is **uninitialized** (declared but its `let`/`const` has not yet
/// executed): reading it then is a use-before-defined error. The cell `kind`
/// (mutable/const/parameter/dispatcher, MD §6) joins when it is first needed
/// (dynamic parameters at M4, dispatch at M5); at M2a assignability is already a
/// static check (S-6 rule 2a), so the machine does not re-check it here.
#[derive(Clone, Debug)]
pub struct CellObj {
    /// The bound value, or `None` if not yet initialized.
    pub value: Option<Value>,
}

/// What a callable value dispatches to (machine-design §8): a Doodle **source**
/// callable, or a host **intrinsic** foreign function (the provisional S-43
/// mechanism — a `print`-style native seeded before the module system).
#[derive(Clone, Copy, Debug)]
pub enum CallableTarget {
    /// A Doodle source callable: a `CallableId` into
    /// [`ResolvedModule::callables`](crate::resolve::ResolvedModule::callables),
    /// which supplies the params, slot count, body, and kind on invocation.
    Source(u32),
    /// A host intrinsic foreign function (E§5.1): an index into the instance's
    /// intrinsic registry (S-43). It has no source body, frame, or captures — a
    /// synchronous call runs the host callback inline (it never becomes a frame).
    Intrinsic(u32),
}

/// A callable object (machine-design §4/§8): a `to`/`fn`/lambda value, or a host
/// intrinsic foreign function. Its **identity is the slab index** — callable
/// equality is identity (L§4.9), so a plain module-level `to`/`fn` is interned to
/// **one** `CalObj` (its declaration runs once, MD §8) and every call site reads
/// that same index; each registered intrinsic likewise interns to one `CalObj`.
#[derive(Clone, Debug)]
pub struct CalObj {
    /// The module the callable was declared in (single module at M2a); the module
    /// an intrinsic is seeded into.
    pub module: ModuleId,
    /// What the callable dispatches to — a source body or a host intrinsic.
    pub target: CallableTarget,
    /// The cells this closure captured at creation (capture representation B, MD
    /// §7/§10). Empty for a plain `to`/`fn` and for every intrinsic; populated for
    /// closures at M2a.8.
    pub captures: Vec<CellIdx>,
}

impl CalObj {
    /// The source `CallableId`, asserting this is a source callable. Frame and
    /// return code only ever holds source callables — an intrinsic runs inline and
    /// never becomes a callable frame — so those sites read the id through here.
    pub fn source_id(&self) -> u32 {
        match self.target {
            CallableTarget::Source(id) => id,
            CallableTarget::Intrinsic(_) => {
                unreachable!("an intrinsic foreign function never becomes a callable frame")
            }
        }
    }
}

/// A type value (L§4.12): a built-in type denoted for use with `is` (L§6.5) and
/// reflection (L§13). Record types and protocol values join at M4/M5.
#[derive(Clone, Debug)]
pub struct TypeObj {
    /// Which built-in type this value denotes. Crate-internal: `BuiltinType` is a
    /// machine detail, not part of the heap's public surface.
    pub(crate) builtin: BuiltinType,
}
