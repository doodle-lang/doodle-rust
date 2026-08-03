//! The precise, non-moving mark-sweep collector (machine-design §15).
//!
//! GC is **precise** (it walks exactly the reachable graph from typed roots) and
//! **non-moving** (a survivor keeps its slab index, so identities and the whole
//! index-based machine state are undisturbed). The mark phase uses an explicit
//! work-stack — no recursion — so cycles (through cells, lists, and later ref
//! records) are legal and a deep graph cannot overflow the Rust stack. The sweep
//! reclaims every unmarked object and rebuilds each free list in index order
//! ([`Slab::sweep`](super::Slab::sweep)), a determinism gate. Reclamation is
//! invisible to Doodle: only unreachable objects — which no live reference can
//! witness — are freed, so results never change (E§11).
//!
//! Root **enumeration** lives on the machine side (`machine/gc.rs`), which knows
//! the frame stack, register, unwind record, ring, and namespaces; it seeds this
//! module's [`Tracer`] with each root. This module owns the **traversal** (a heap
//! value's outgoing references) and the **sweep**.

use super::{
    Heap, OBJECT_OVERHEAD, bigint_payload, bytes_payload, cal_payload, cell_payload,
    foreign_payload, list_payload, str_payload, type_payload,
};
use crate::machine::{CalIdx, CellIdx, Value};

/// Seeds and drives a collection's mark phase (MD §15). The machine-side root
/// enumeration invokes [`value`](Tracer::value) / [`cell`](Tracer::cell) /
/// [`callable`](Tracer::callable) for each root; [`Heap::collect`] then traces the
/// reachable graph transitively before sweeping.
pub(crate) struct Tracer<'h> {
    heap: &'h mut Heap,
    /// The gray set: marked objects whose children are not yet scanned. Only kinds
    /// with outgoing references (lists, callables) are pushed here; leaves are
    /// marked in place.
    stack: Vec<Value>,
}

impl Tracer<'_> {
    /// Marks a value root reachable (a no-op for a non-heap scalar).
    pub(crate) fn value(&mut self, v: Value) {
        self.enqueue(v);
    }

    /// Marks a binding-cell root reachable and schedules its contents.
    pub(crate) fn cell(&mut self, cell: CellIdx) {
        self.enqueue_cell(cell);
    }

    /// Marks a callable root reachable and schedules its captured cells.
    pub(crate) fn callable(&mut self, cal: CalIdx) {
        self.enqueue(Value::Callable(cal));
    }

    /// Marks `v`'s object (if any) reachable, graying it for child-scanning when it
    /// is an aggregate kind and was newly marked.
    fn enqueue(&mut self, v: Value) {
        match v {
            // Aggregate kinds (outgoing references): mark, then gray to scan later.
            Value::List(i) => {
                if self.heap.lists.mark(i.0) {
                    self.stack.push(v);
                }
            }
            Value::Callable(i) => {
                if self.heap.callables.mark(i.0) {
                    self.stack.push(v);
                }
            }
            // Leaf heap objects: mark only (no children).
            Value::Str(i) => {
                self.heap.strings.mark(i.0);
            }
            Value::Bytes(i) => {
                self.heap.bytes.mark(i.0);
            }
            Value::BigInt(i) => {
                self.heap.bigints.mark(i.0);
            }
            Value::Type(i) => {
                self.heap.types.mark(i.0);
            }
            // A foreign value is an opaque leaf (E§4.5): its host `ptr` is not a heap
            // reference, so it has no children to scan — mark only. A dead one's
            // finalizer is taken and queued by the sweep, not here.
            Value::Foreign(i) => {
                self.heap.foreigns.mark(i.0);
            }
            // Non-heap scalars: nothing to mark.
            Value::Nil | Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Module(_) => {}
            // Not yet allocatable (no slab exists): such a value cannot be
            // constructed, so reaching here is a bug. When dicts/records land (M4/M5),
            // add their marking — and child-scanning for the aggregate kinds — here
            // and in `run`.
            Value::Dict(_) | Value::Record(_) => {
                unreachable!("GC reached {v:?} — dict/record are not allocatable yet")
            }
        }
    }

    /// Marks a cell reachable and enqueues its bound value (the cell's one child).
    fn enqueue_cell(&mut self, cell: CellIdx) {
        if !self.heap.cells.mark(cell.0) {
            return; // already marked this cycle — its value is (or will be) scanned
        }
        if let Some(v) = self.heap.cells.get(cell.0).value {
            self.enqueue(v);
        }
    }

    /// Drains the gray set, scanning each aggregate object's outgoing references.
    fn run(&mut self) {
        while let Some(v) = self.stack.pop() {
            match v {
                Value::List(i) => {
                    // Copy each element out before `enqueue` re-borrows the heap
                    // mutably; the index walk avoids holding the list's borrow.
                    let n = self.heap.lists.get(i.0).items.len();
                    for k in 0..n {
                        let item = self.heap.lists.get(i.0).items[k];
                        self.enqueue(item);
                    }
                }
                Value::Callable(i) => {
                    let n = self.heap.callables.get(i.0).captures.len();
                    for k in 0..n {
                        let cell = self.heap.callables.get(i.0).captures[k];
                        self.enqueue_cell(cell);
                    }
                }
                _ => unreachable!("only aggregate kinds are grayed, got {v:?}"),
            }
        }
    }
}

impl Heap {
    /// Runs one precise, non-moving collection (MD §15). `seed_roots` marks the
    /// root set (the caller enumerates it from the machine state); this then traces
    /// transitively and sweeps every slab, subtracting reclaimed objects from
    /// `bytes_allocated`. Any **finalizers** of collected foreign values (E§4.5) run
    /// **after** the sweep completes, host-side and never Doodle-observable.
    pub(crate) fn collect(&mut self, seed_roots: impl FnOnce(&mut Tracer)) {
        {
            let mut tracer = Tracer {
                heap: self,
                stack: Vec::new(),
            };
            seed_roots(&mut tracer);
            tracer.run();
        }
        let finalizers = self.sweep_all();
        // Run after the whole collection completes (MD §15): the heap is fully swept and
        // consistent, and a finalizer cannot re-enter the instance (E§4.5), so this
        // cannot perturb the deterministic Doodle heap. Each is isolated (a panic in one
        // must not skip the rest — that would leak their resources, MD §15).
        for (ptr, finalizer) in finalizers {
            super::run_finalizer(ptr, finalizer);
        }
    }

    /// Sweeps every slab in index order (MD §15), reclaiming unmarked objects and
    /// subtracting their exact byte charge (overhead + payload, the same formula
    /// `charge_object` added at allocation) from the heap total. Returns the
    /// `(host_ptr, finalizer)` of every reclaimed **foreign** value, taken out of the
    /// dying object so it runs exactly once (E§4.5) — the caller runs them after the
    /// collection completes.
    fn sweep_all(&mut self) -> Vec<(u64, super::Finalizer)> {
        let mut freed = 0u64;
        self.strings
            .sweep(|o| freed += OBJECT_OVERHEAD + str_payload(o));
        self.bytes
            .sweep(|o| freed += OBJECT_OVERHEAD + bytes_payload(o));
        self.lists
            .sweep(|o| freed += OBJECT_OVERHEAD + list_payload(o));
        self.bigints
            .sweep(|o| freed += OBJECT_OVERHEAD + bigint_payload(o));
        self.cells
            .sweep(|o| freed += OBJECT_OVERHEAD + cell_payload(o));
        self.callables
            .sweep(|o| freed += OBJECT_OVERHEAD + cal_payload(o));
        self.types
            .sweep(|o| freed += OBJECT_OVERHEAD + type_payload(o));
        // A reclaimed foreign value: account it like any object, and take its finalizer
        // (if any) to run once the collection is done (E§4.5). The finalizer is taken
        // here, so it can never run again — a later sweep or `destroy` finds the slot
        // already freed / the finalizer gone.
        let mut finalizers = Vec::new();
        self.foreigns.sweep(|o| {
            freed += OBJECT_OVERHEAD + foreign_payload(o);
            if let Some(finalizer) = o.finalizer.take() {
                finalizers.push((o.ptr, finalizer));
            }
        });
        debug_assert!(
            freed <= self.bytes_allocated,
            "GC freed more bytes ({freed}) than were charged ({})",
            self.bytes_allocated
        );
        self.bytes_allocated -= freed;
        finalizers
    }
}
