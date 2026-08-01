//! The instance heap: per-kind index slabs, allocation accounting, and object
//! identity (machine-design §4).
//!
//! One [`Slab`] per heap kind holds that kind's objects; a heap value (the
//! `Copy` [`Value`] representation) carries only the `u32` slab index, so object
//! identity is the index (MD §4) and the machine state stays snapshot-friendly.
//!
//! **Scope (M2a.1).** The foundation: the slab mechanism, allocation accounting,
//! and slot identity — **no GC yet** (mark/sweep is M2a.10; allocation only
//! grows the heap until then). The slab set is the demo subset's *value-bearing*
//! kinds that are ready to build and test now — strings, byte strings, lists;
//! the remaining kinds (bignums, callables, cells, types) get their slabs in the
//! chunks that first construct them (M2a.3/M2a.5/M2a.8), each a one-line
//! addition on the generic [`Slab`].
//!
//! **Determinism (MD §4/§15).** [`Heap::bytes_allocated`] — which drives GC
//! triggers and the heap limit (E§7.7) — counts *logical* payload bytes (a
//! string's byte length, a list's element count × value width), **never** a
//! `Vec`'s capacity, whose growth policy is an allocator detail and would leak
//! nondeterminism into GC trigger points. Pure caches (the §5 grapheme memo) are
//! excluded when they arrive.
//!
//! **Accounting integrity.** Payload bytes are charged at allocation, so any
//! *in-place growth* of a variable-size payload (e.g. pushing to a list) must
//! also adjust `bytes_allocated` — otherwise a growing object escapes the heap
//! limit. That is why there is deliberately **no raw mutable payload accessor**
//! (`&mut ListObj` etc.): mutation goes through accounting-aware `Heap` methods,
//! which land with the list/string operations that first need them.

mod slab;

pub use slab::Slab;

use crate::machine::{
    BigIntIdx, BuiltinType, BytesIdx, CalIdx, CellIdx, ListIdx, StrIdx, TypeIdx, Value,
};
use crate::span::ModuleId;
use num_bigint::BigInt;

/// The byte width charged to [`Heap::bytes_allocated`] per list element. A fixed
/// per-build constant (not a `Vec` capacity), so accounting is deterministic.
const VALUE_BYTES: u64 = size_of::<Value>() as u64;

/// The byte width charged to [`Heap::bytes_allocated`] per captured cell of a
/// callable. A fixed per-build constant (not a `Vec` capacity), for determinism.
const CELLIDX_BYTES: u64 = size_of::<CellIdx>() as u64;

/// A fixed per-object overhead charged to [`Heap::bytes_allocated`] on **every**
/// allocation, on top of the object's payload (MD §4). It makes **object count**
/// contribute to the heap total: without it, a flood of empty/tiny objects (each
/// ~0 payload but a real slab slot) would grow memory while `bytes_allocated`
/// stayed flat, so the heap limit and (M2a.10) the GC trigger would never fire —
/// an OOM-instead-of-clean-fault hole (the M2a.1 object-count gap, resolved by the
/// user's ruling: MD §4 accounts `payload + fixed per-object overhead`). A flat
/// constant — not `size_of` of a slot — keeps the charge identical across targets.
const OBJECT_OVERHEAD: u64 = 32;

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

/// A callable object (machine-design §4/§8): a `to`/`fn`/lambda value. Its
/// **identity is the slab index** — callable equality is identity (L§4.9), so a
/// plain module-level `to`/`fn` is interned to **one** `CalObj` (its declaration
/// runs once, MD §8) and every call site reads that same index.
#[derive(Clone, Debug)]
pub struct CalObj {
    /// The module the callable was declared in (single module at M2a).
    pub module: ModuleId,
    /// The declaring body's `CallableId` — an index into
    /// [`ResolvedModule::callables`](crate::resolve::ResolvedModule::callables),
    /// which supplies the params, slot count, body, and kind on invocation.
    pub callable: u32,
    /// The cells this closure captured at creation (capture representation B, MD
    /// §7/§10). Empty for a plain `to`/`fn`; populated for closures at M2a.8.
    pub captures: Vec<CellIdx>,
}

/// A type value (L§4.12): a built-in type denoted for use with `is` (L§6.5) and
/// reflection (L§13). Record types and protocol values join at M4/M5.
#[derive(Clone, Debug)]
pub struct TypeObj {
    /// Which built-in type this value denotes. Crate-internal: `BuiltinType` is a
    /// machine detail, not part of the heap's public surface.
    pub(crate) builtin: BuiltinType,
}

/// The per-instance heap (machine-design §4): the object slabs plus the
/// allocation accounting the GC and heap limit read.
pub struct Heap {
    strings: Slab<StrObj>,
    bytes: Slab<BytesObj>,
    lists: Slab<ListObj>,
    bigints: Slab<BigIntObj>,
    cells: Slab<CellObj>,
    callables: Slab<CalObj>,
    types: Slab<TypeObj>,
    /// Accounted heap bytes (MD §4): each object's program-driven payload plus a
    /// fixed [`OBJECT_OVERHEAD`], so object count counts (see the module-level
    /// determinism note). Monotonic under M2a.1 (no reclamation); GC decreases it
    /// at M2a.10.
    bytes_allocated: u64,
    /// Monotonic allocation counter; stamped into each object's slot as its
    /// identity serial (MD §4).
    alloc_serial: u64,
}

impl Heap {
    /// Creates an empty heap.
    pub fn new() -> Self {
        Heap {
            strings: Slab::new(),
            bytes: Slab::new(),
            lists: Slab::new(),
            bigints: Slab::new(),
            cells: Slab::new(),
            callables: Slab::new(),
            types: Slab::new(),
            bytes_allocated: 0,
            alloc_serial: 0,
        }
    }

    /// The next allocation serial (post-increment): a fresh, monotonic stamp.
    fn next_serial(&mut self) -> u64 {
        let serial = self.alloc_serial;
        self.alloc_serial += 1;
        serial
    }

    /// Charges one object's `payload` bytes plus the fixed [`OBJECT_OVERHEAD`] to
    /// the heap total (MD §4). Every `alloc_*` routes through this, so no allocation
    /// can escape the per-object charge that makes object count count.
    fn charge_object(&mut self, payload: u64) {
        self.bytes_allocated += OBJECT_OVERHEAD + payload;
    }

    /// Allocates a string (its `utf8` must already be NFC — see [`StrObj`]).
    pub fn alloc_string(&mut self, utf8: Box<str>) -> StrIdx {
        debug_assert!(
            unicode_normalization::is_nfc(&utf8),
            "alloc_string requires NFC input (machine-design §5)"
        );
        self.charge_object(utf8.len() as u64);
        let serial = self.next_serial();
        StrIdx(self.strings.alloc(StrObj { utf8 }, serial))
    }

    /// Allocates a byte string.
    pub fn alloc_bytes(&mut self, bytes: Box<[u8]>) -> BytesIdx {
        self.charge_object(bytes.len() as u64);
        let serial = self.next_serial();
        BytesIdx(self.bytes.alloc(BytesObj { bytes }, serial))
    }

    /// Allocates a list from its initial elements.
    pub fn alloc_list(&mut self, items: Vec<Value>) -> ListIdx {
        self.charge_object(items.len() as u64 * VALUE_BYTES);
        let serial = self.next_serial();
        ListIdx(self.lists.alloc(ListObj { items }, serial))
    }

    /// Allocates a bignum. The caller must uphold the canonical-int invariant
    /// (MD §3): `value` must not fit `i64` — a fitting value is a [`Value::Int`].
    pub fn alloc_bigint(&mut self, value: BigInt) -> BigIntIdx {
        // Payload is the magnitude size in bytes (bit length rounded up); `bits`
        // is deterministic, so accounting stays replay-stable.
        self.charge_object(value.bits().div_ceil(8));
        let serial = self.next_serial();
        BigIntIdx(self.bigints.alloc(BigIntObj { value }, serial))
    }

    /// Borrows the string at `idx`.
    pub fn string(&self, idx: StrIdx) -> &StrObj {
        self.strings.get(idx.0)
    }

    /// Borrows the byte string at `idx`.
    pub fn byte_string(&self, idx: BytesIdx) -> &BytesObj {
        self.bytes.get(idx.0)
    }

    /// Borrows the list at `idx`.
    pub fn list(&self, idx: ListIdx) -> &ListObj {
        self.lists.get(idx.0)
    }

    /// Borrows the bignum at `idx`.
    pub fn bigint(&self, idx: BigIntIdx) -> &BigIntObj {
        self.bigints.get(idx.0)
    }

    /// Allocates a binding cell with the given initial `value` (`None` =
    /// uninitialized). Its payload is one value width (MD §6/§7).
    pub fn alloc_cell(&mut self, value: Option<Value>) -> CellIdx {
        self.charge_object(VALUE_BYTES);
        let serial = self.next_serial();
        CellIdx(self.cells.alloc(CellObj { value }, serial))
    }

    /// Borrows the cell at `idx`.
    pub fn cell(&self, idx: CellIdx) -> &CellObj {
        self.cells.get(idx.0)
    }

    /// Mutably borrows the cell at `idx` (a binding write; MD §6/§7).
    pub fn cell_mut(&mut self, idx: CellIdx) -> &mut CellObj {
        self.cells.get_mut(idx.0)
    }

    /// Allocates a callable. Payload is a fixed header plus one width per captured
    /// cell (MD §4), over the per-object overhead.
    pub fn alloc_callable(&mut self, obj: CalObj) -> CalIdx {
        self.charge_object(VALUE_BYTES + obj.captures.len() as u64 * CELLIDX_BYTES);
        let serial = self.next_serial();
        CalIdx(self.callables.alloc(obj, serial))
    }

    /// Borrows the callable at `idx`.
    pub fn callable(&self, idx: CalIdx) -> &CalObj {
        self.callables.get(idx.0)
    }

    /// Allocates a type value. Payload is one fixed header width (MD §4), over the
    /// per-object overhead.
    pub fn alloc_type(&mut self, obj: TypeObj) -> TypeIdx {
        self.charge_object(VALUE_BYTES);
        let serial = self.next_serial();
        TypeIdx(self.types.alloc(obj, serial))
    }

    /// Borrows the type value at `idx`.
    pub fn type_value(&self, idx: TypeIdx) -> &TypeObj {
        self.types.get(idx.0)
    }

    /// Total accounted heap bytes (payload + per-object overhead, MD §4). Drives GC
    /// triggering and the heap limit (M2a.9/M2a.10).
    pub fn bytes_allocated(&self) -> u64 {
        self.bytes_allocated
    }

    /// The number of live objects across all slabs (for tests and, later, GC
    /// assertions).
    pub fn live_objects(&self) -> u32 {
        self.strings.live_count()
            + self.bytes.live_count()
            + self.lists.live_count()
            + self.bigints.live_count()
            + self.cells.live_count()
            + self.callables.live_count()
            + self.types.live_count()
    }
}

impl Default for Heap {
    fn default() -> Self {
        Heap::new()
    }
}

/// Whether `a` and `b` reference the **same heap slot** (machine-design §4
/// identity): the same index-carrying [`Value`] variant with equal indices.
///
/// This is the representation-level identity primitive — "these two values name
/// one heap object." Immediate values (`nil`/bools/ints/floats) carry no slot,
/// so they are never the-same-reference here; whether Doodle *exposes* slot
/// identity for a given type (the `same?`/`is` surface, L§4.9) is layered on per
/// type by later milestones and does not change this mechanical answer.
pub fn same_ref(a: Value, b: Value) -> bool {
    // Match `a` exhaustively (no wildcard) so a heap-backed variant added later
    // forces a new arm here at compile time, rather than silently defaulting to
    // "never the same object" — the reason `Value` omits `derive(PartialEq)`.
    // Each arm compares `b` for the *same* variant, so a cross-variant pair (or
    // an immediate) is never the-same-reference.
    match a {
        Value::Nil | Value::Bool(_) | Value::Int(_) | Value::Float(_) => false,
        Value::Str(x) => matches!(b, Value::Str(y) if x == y),
        Value::Bytes(x) => matches!(b, Value::Bytes(y) if x == y),
        Value::List(x) => matches!(b, Value::List(y) if x == y),
        Value::BigInt(x) => matches!(b, Value::BigInt(y) if x == y),
        Value::Dict(x) => matches!(b, Value::Dict(y) if x == y),
        Value::Record(x) => matches!(b, Value::Record(y) if x == y),
        Value::Callable(x) => matches!(b, Value::Callable(y) if x == y),
        Value::Type(x) => matches!(b, Value::Type(y) if x == y),
        Value::Foreign(x) => matches!(b, Value::Foreign(y) if x == y),
        Value::Module(x) => matches!(b, Value::Module(y) if x == y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocations_get_distinct_indices_and_round_trip() {
        let mut heap = Heap::new();
        let a = heap.alloc_string("hi".into());
        let b = heap.alloc_string("yo".into());
        assert_ne!(a.0, b.0);
        assert_eq!(&*heap.string(a).utf8, "hi");
        assert_eq!(&*heap.string(b).utf8, "yo");
        assert_eq!(heap.live_objects(), 2);
    }

    #[test]
    fn bytes_allocated_counts_payload_plus_per_object_overhead_across_kinds() {
        let mut heap = Heap::new();
        heap.alloc_string("abcd".into()); // 4 payload bytes
        heap.alloc_bytes(vec![0u8; 3].into()); // 3 payload bytes
        heap.alloc_list(vec![Value::Int(1), Value::Int(2)]); // 2 * VALUE_BYTES
        // Each of the three objects also carries the fixed per-object overhead, so
        // object count (not just payload) contributes to the heap total.
        assert_eq!(
            heap.bytes_allocated(),
            4 + 3 + 2 * VALUE_BYTES + 3 * OBJECT_OVERHEAD
        );
    }

    #[test]
    fn serials_are_monotonic_across_kinds_in_allocation_order() {
        // One shared counter stamps every kind, so identity serials never collide.
        let mut heap = Heap::new();
        let s = heap.alloc_string("x".into());
        let by = heap.alloc_bytes(vec![1].into());
        let l = heap.alloc_list(vec![]);
        // Reach into the slabs by index to read the stamped serial.
        assert_eq!(heap.strings.serial(s.0), 0);
        assert_eq!(heap.bytes.serial(by.0), 1);
        assert_eq!(heap.lists.serial(l.0), 2);
    }

    #[test]
    fn same_ref_matches_slot_identity_per_reference_variant() {
        use crate::machine::{CalIdx, RecIdx};
        use crate::span::ModuleId;

        // Same variant, same index — the same object.
        assert!(same_ref(Value::Str(StrIdx(3)), Value::Str(StrIdx(3))));
        assert!(same_ref(Value::List(ListIdx(0)), Value::List(ListIdx(0))));
        assert!(same_ref(
            Value::Callable(CalIdx(9)),
            Value::Callable(CalIdx(9))
        ));
        assert!(same_ref(
            Value::Module(ModuleId(2)),
            Value::Module(ModuleId(2))
        ));

        // Same variant, different index — different objects.
        assert!(!same_ref(Value::Str(StrIdx(3)), Value::Str(StrIdx(4))));
        assert!(!same_ref(
            Value::Record(RecIdx(0)),
            Value::Record(RecIdx(1))
        ));
    }

    #[test]
    fn same_ref_rejects_cross_variant_and_immediates() {
        use crate::machine::{CalIdx, DictIdx, TypeIdx};

        // Same underlying index, different variant — not the same slot.
        assert!(!same_ref(Value::Str(StrIdx(0)), Value::Bytes(BytesIdx(0))));
        assert!(!same_ref(Value::List(ListIdx(0)), Value::Dict(DictIdx(0))));
        assert!(!same_ref(
            Value::Type(TypeIdx(1)),
            Value::Callable(CalIdx(1))
        ));

        // Immediates carry no slot identity, even value-equal ones.
        assert!(!same_ref(Value::Int(5), Value::Int(5)));
        assert!(!same_ref(Value::Bool(true), Value::Bool(true)));
        assert!(!same_ref(Value::Nil, Value::Nil));
        // A heap value is never the same reference as an immediate.
        assert!(!same_ref(Value::Str(StrIdx(0)), Value::Nil));
    }
}
