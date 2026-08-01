//! A generic index slab: the storage primitive under every heap kind
//! (machine-design §4).
//!
//! A [`Slab`] is a `Vec` of slots addressed by a `u32` index — the identity of
//! a heap object *is* its slot index (MD §4), never a Rust reference, so the
//! whole machine state stays index-based and snapshot-friendly (MD ground
//! rule 2). Freed slots are recycled through a free list.
//!
//! **Scope (M2a.1).** This is the allocate-and-recycle mechanism only. Garbage
//! collection (the mark bit, the mark/sweep walk) lands at M2a.10; the [`free`]
//! path and the free list exist now (they are how the sweep will return slots)
//! and are unit-tested, but nothing calls [`free`] in real execution until then.
//! **Determinism (MD §15):** the future sweep rebuilds each slab's free list in
//! *index order*, so allocation reuse is identical across runs; nothing here may
//! depend on an address or on hashing.
//!
//! [`free`]: Slab::free

/// A slot in a [`Slab`]: either a live object or a hole in the free list.
enum Slot<T> {
    /// A live object, stamped with the monotonic allocation `serial` (MD §4) —
    /// a deterministic identity-derived scalar valid while any witness to this
    /// object's identity outlives it.
    Occupied {
        /// The stored object.
        obj: T,
        /// The allocation serial (see the variant doc).
        serial: u64,
        /// The GC reachability mark (MD §15): set during a collection's mark
        /// phase, cleared by its sweep. `false` between collections.
        mark: bool,
    },
    /// A recycled slot; `next_free` links to the next hole, or `None` at the end.
    Free {
        /// The next free slot index, or `None` if this is the list tail.
        next_free: Option<u32>,
    },
}

/// A per-kind index slab (machine-design §4): objects of one heap kind stored in
/// a `Vec`, addressed by a `u32` index, with a free list for recycling.
pub struct Slab<T> {
    slots: Vec<Slot<T>>,
    free_head: Option<u32>,
    live_count: u32,
}

impl<T> Slab<T> {
    /// Creates an empty slab.
    pub fn new() -> Self {
        Slab {
            slots: Vec::new(),
            free_head: None,
            live_count: 0,
        }
    }

    /// Allocates `obj` (stamped with `serial`), returning its index. Reuses a
    /// free slot when one exists, otherwise grows the slab.
    ///
    /// Panics if the slab would exceed the `u32` index space — overflow must
    /// fail loudly, never wrap into an index that aliases a live object (MD
    /// ground rule 2).
    pub fn alloc(&mut self, obj: T, serial: u64) -> u32 {
        self.live_count += 1;
        match self.free_head {
            Some(index) => {
                let Slot::Free { next_free } = self.slots[index as usize] else {
                    unreachable!("free_head pointed at an occupied slot");
                };
                self.free_head = next_free;
                self.slots[index as usize] = Slot::Occupied {
                    obj,
                    serial,
                    mark: false,
                };
                index
            }
            None => {
                let index =
                    u32::try_from(self.slots.len()).expect("heap slab exceeds the u32 index space");
                self.slots.push(Slot::Occupied {
                    obj,
                    serial,
                    mark: false,
                });
                index
            }
        }
    }

    /// Borrows the object at `index`.
    ///
    /// Panics if the slot is free: an index handed around inside the machine
    /// must always name a live object, so a dangling index is a machine bug to
    /// surface, not to paper over.
    pub fn get(&self, index: u32) -> &T {
        match &self.slots[index as usize] {
            Slot::Occupied { obj, .. } => obj,
            Slot::Free { .. } => unreachable!("get on a freed slab slot {index}"),
        }
    }

    /// Mutably borrows the object at `index`. Panics on a free slot (see [`get`]).
    ///
    /// [`get`]: Slab::get
    pub fn get_mut(&mut self, index: u32) -> &mut T {
        match &mut self.slots[index as usize] {
            Slot::Occupied { obj, .. } => obj,
            Slot::Free { .. } => unreachable!("get_mut on a freed slab slot {index}"),
        }
    }

    /// The allocation serial of the object at `index` (MD §4 identity scalar).
    /// Panics on a free slot (see [`get`]).
    ///
    /// [`get`]: Slab::get
    pub fn serial(&self, index: u32) -> u64 {
        match &self.slots[index as usize] {
            Slot::Occupied { serial, .. } => *serial,
            Slot::Free { .. } => unreachable!("serial on a freed slab slot {index}"),
        }
    }

    /// Frees the slot at `index`, linking it into the free list.
    ///
    /// Used by the GC sweep (M2a.10). The sweep rebuilds the whole list in index
    /// order for determinism (MD §15), so the transient link order established
    /// here is never observable.
    pub fn free(&mut self, index: u32) {
        debug_assert!(
            matches!(self.slots[index as usize], Slot::Occupied { .. }),
            "double free of slab slot {index}"
        );
        self.slots[index as usize] = Slot::Free {
            next_free: self.free_head,
        };
        self.free_head = Some(index);
        self.live_count -= 1;
    }

    /// Marks the object at `index` reachable for the in-progress collection (MD
    /// §15). Returns `true` if it was newly marked (previously unmarked), `false`
    /// if already marked — so the tracer scans each object's children exactly once
    /// and cycles terminate. Panics on a free slot (a dangling index is a bug).
    pub fn mark(&mut self, index: u32) -> bool {
        match &mut self.slots[index as usize] {
            Slot::Occupied { mark, .. } => {
                if *mark {
                    false
                } else {
                    *mark = true;
                    true
                }
            }
            Slot::Free { .. } => unreachable!("mark on a freed slab slot {index}"),
        }
    }

    /// Sweeps the slab (MD §15): frees every occupied slot whose mark is clear
    /// (calling `on_free` with the reclaimed object, so the heap can subtract its
    /// byte charge), clears the mark on every survivor, and rebuilds the free list
    /// in **index order** so allocation reuse is identical across runs (a
    /// determinism gate). The mark phase must have run first; afterward every
    /// survivor is unmarked, ready for the next cycle.
    pub fn sweep(&mut self, mut on_free: impl FnMut(&T)) {
        let mut free_head = None;
        let mut live = 0u32;
        // Walk indices high→low and prepend each hole, so the rebuilt list runs
        // low→high (smallest free index at the head, reused first): index-ordered,
        // deterministic reuse regardless of the order objects died.
        for i in (0..self.slots.len()).rev() {
            let keep = match &mut self.slots[i] {
                Slot::Occupied { mark, .. } => {
                    let reachable = *mark;
                    *mark = false;
                    reachable
                }
                Slot::Free { .. } => false,
            };
            if keep {
                live += 1;
            } else {
                if let Slot::Occupied { obj, .. } = &self.slots[i] {
                    on_free(obj);
                }
                self.slots[i] = Slot::Free {
                    next_free: free_head,
                };
                free_head = Some(i as u32);
            }
        }
        self.free_head = free_head;
        self.live_count = live;
    }

    /// Whether the slot at `index` currently holds a live object.
    pub fn is_occupied(&self, index: u32) -> bool {
        matches!(self.slots.get(index as usize), Some(Slot::Occupied { .. }))
    }

    /// The number of live objects in this slab.
    pub fn live_count(&self) -> u32 {
        self.live_count
    }
}

impl<T> Default for Slab<T> {
    fn default() -> Self {
        Slab::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_hands_out_ascending_indices() {
        let mut slab: Slab<u32> = Slab::new();
        assert_eq!(slab.alloc(10, 0), 0);
        assert_eq!(slab.alloc(20, 1), 1);
        assert_eq!(slab.alloc(30, 2), 2);
        assert_eq!(slab.live_count(), 3);
    }

    #[test]
    fn get_and_get_mut_round_trip() {
        let mut slab: Slab<u32> = Slab::new();
        let i = slab.alloc(7, 0);
        assert_eq!(*slab.get(i), 7);
        *slab.get_mut(i) = 99;
        assert_eq!(*slab.get(i), 99);
    }

    #[test]
    fn serial_is_the_stamp_passed_at_alloc() {
        let mut slab: Slab<u32> = Slab::new();
        let a = slab.alloc(1, 41);
        let b = slab.alloc(2, 42);
        assert_eq!(slab.serial(a), 41);
        assert_eq!(slab.serial(b), 42);
    }

    #[test]
    fn free_recycles_the_slot_and_updates_live_count() {
        let mut slab: Slab<u32> = Slab::new();
        let a = slab.alloc(1, 0);
        let b = slab.alloc(2, 1);
        assert_eq!(slab.live_count(), 2);
        assert!(slab.is_occupied(a));

        slab.free(a);
        assert_eq!(slab.live_count(), 1);
        assert!(!slab.is_occupied(a));
        assert!(slab.is_occupied(b));

        // The next allocation reuses the freed slot rather than growing.
        let c = slab.alloc(3, 2);
        assert_eq!(c, a);
        assert_eq!(*slab.get(c), 3);
        assert_eq!(slab.serial(c), 2);
        assert_eq!(slab.live_count(), 2);
    }

    #[test]
    fn is_occupied_is_false_past_the_end() {
        let mut slab: Slab<u32> = Slab::new();
        slab.alloc(1, 0);
        assert!(!slab.is_occupied(5));
    }

    #[test]
    fn mark_is_idempotent_within_a_cycle() {
        let mut slab: Slab<u32> = Slab::new();
        let a = slab.alloc(1, 0);
        assert!(slab.mark(a), "first mark is newly-marked");
        assert!(!slab.mark(a), "second mark reports already-marked");
    }

    #[test]
    fn sweep_frees_unmarked_keeps_marked_and_clears_marks() {
        let mut slab: Slab<u32> = Slab::new();
        let a = slab.alloc(10, 0);
        let b = slab.alloc(20, 1);
        let c = slab.alloc(30, 2);
        slab.mark(b); // only `b` is reachable
        let mut freed = Vec::new();
        slab.sweep(|v| freed.push(*v));
        freed.sort_unstable(); // on_free order is unspecified; only the set matters
        // `a` and `c` were reclaimed; `b` survives; live count reflects it.
        assert_eq!(freed, vec![10, 30]);
        assert!(!slab.is_occupied(a));
        assert!(slab.is_occupied(b));
        assert!(!slab.is_occupied(c));
        assert_eq!(slab.live_count(), 1);
        // The mark on the survivor is cleared, so a second sweep (nothing marked)
        // reclaims it too — marks do not persist across cycles.
        let mut freed2 = Vec::new();
        slab.sweep(|v| freed2.push(*v));
        assert_eq!(freed2, vec![20]);
        assert_eq!(slab.live_count(), 0);
    }

    #[test]
    fn sweep_rebuilds_the_free_list_in_index_order() {
        let mut slab: Slab<u32> = Slab::new();
        let a = slab.alloc(10, 0); // index 0
        let b = slab.alloc(20, 1); // index 1
        let _c = slab.alloc(30, 2); // index 2
        // Keep only index 2; free 0 and 1. The rebuilt free list must hand out the
        // SMALLEST free index first (index order) regardless of sweep direction.
        slab.mark(_c);
        slab.sweep(|_| {});
        assert_eq!(slab.alloc(40, 3), a, "smallest free index reused first");
        assert_eq!(slab.alloc(50, 4), b, "then the next");
    }
}
