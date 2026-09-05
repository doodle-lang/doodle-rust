//! Host handles into an instance's heap (engine spec E§4.2, machine-design §16).
//!
//! The host never touches a raw [`Value`]; it holds **handles**. A live handle
//! keeps its value reachable — the handle table's values are GC roots (§15) — so
//! the host must [`release`](HandleTable::release) handles it no longer needs. A
//! handle packs `(index, generation)` into a `u64`; when a slot is freed and later
//! reused its generation bumps, so a **stale** handle (used after release) is caught
//! at the host boundary rather than silently naming a different value. That check
//! runs on every host-facing handle operation — the machine's internal slab indices
//! never pay it (they are structurally kept live). In **debug builds** the handle also
//! carries a small per-instance id (MD §16), stolen from the index field, so a handle
//! minted by one instance and used on another is caught as
//! [`HandleError::ForeignInstance`] instead of silently naming that instance's own slot.
//! Release keeps the full 32-bit index+generation and cannot distinguish the collision —
//! a wrong answer, never memory-unsafe (the index+generation still validate against the
//! resolving instance's own table).

use super::Value;

/// A host-held reference to a value in an instance's heap (E§4.2). Opaque: the host
/// round-trips it as [`bits`](Handle::bits) but reads its value only through engine
/// operations. Equality/hash are on the raw bits — a released-and-reused handle
/// compares unequal to the new one (its generation differs).
///
/// Packed `u64` layout: the **generation** is the low 32 bits in every build. In release
/// the **index** is the high 32 bits. In debug the index is 24 bits (32..56) and the top
/// 8 bits (56..64) hold the owning instance's id (MD §16); [`HandleTable::pack`] stamps it
/// and [`HandleTable::live_index`] checks it. The layout is fully encapsulated here and in
/// `HandleTable` — no other code reads or writes these bit positions.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct Handle(u64);

impl Handle {
    fn index(self) -> u32 {
        #[cfg(debug_assertions)]
        {
            ((self.0 >> 32) & 0xFF_FFFF) as u32
        }
        #[cfg(not(debug_assertions))]
        {
            (self.0 >> 32) as u32
        }
    }

    fn generation(self) -> u32 {
        self.0 as u32
    }

    /// The owning instance's id — the top 8 bits (MD §16). Debug builds only; release
    /// handles do not encode it.
    #[cfg(debug_assertions)]
    fn instance_id(self) -> u8 {
        (self.0 >> 56) as u8
    }

    /// The opaque `u64` the host stores and passes back (E§4.2).
    pub fn bits(self) -> u64 {
        self.0
    }

    /// Rebuilds a handle from its [`bits`](Handle::bits). The generation check makes
    /// a forged or stale value safe — it resolves to a boundary error, not a wrong value.
    pub fn from_bits(bits: u64) -> Self {
        Handle(bits)
    }
}

/// Why a handle operation failed at the boundary (E§4.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HandleError {
    /// The handle names a freed slot, or one reused under a newer generation — a
    /// use-after-release (or forged) handle.
    Stale,
    /// The handle names a **live slot of a *different* instance** (MD §16): its embedded
    /// debug instance-id does not match the resolving table's, yet its index+generation
    /// name a live slot here. A cross-instance handle mixup — a host bug, reported
    /// distinctly from [`Stale`](HandleError::Stale) (which a host may routinely retry, e.g.
    /// after releasing). Produced only in debug builds; release does not encode the id, so
    /// it cannot distinguish the collision and resolves against this instance's own slot (a
    /// wrong value, never memory-unsafe).
    ForeignInstance,
}

/// A slot in the [`HandleTable`].
enum Slot {
    /// A live handle: the referenced value, the slot's current generation, and the
    /// outstanding reference count (`retain`/`release`).
    Occupied {
        value: Value,
        generation: u32,
        refs: u32,
    },
    /// A freed slot on the free list. Its `generation` is already bumped past the
    /// handle that last named it, so that handle now reads as stale.
    Free {
        generation: u32,
        next_free: Option<u32>,
    },
}

/// Hands out a distinct id to each [`HandleTable`] in a debug build (MD §16), stamped into
/// the top 8 bits of that table's handles so a handle used on the wrong instance is caught.
/// Wraps after 256 tables — a wrap only makes the cross-instance check miss a collision
/// between two same-id instances (a silent fall-back to release behavior), never a false
/// positive. Host-side and debug-only, so it is not a Doodle-observable path (handle bits
/// never appear in program output or replay transcripts) — a plain relaxed counter is fine.
#[cfg(debug_assertions)]
static NEXT_INSTANCE_ID: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// The per-instance handle table (machine-design §16): a slab of handle slots with a
/// free list, generational reuse, and reference counts.
pub(crate) struct HandleTable {
    slots: Vec<Slot>,
    free_head: Option<u32>,
    /// This table's cross-instance id (MD §16), stamped into every handle it mints and
    /// checked on resolve. Debug builds only.
    #[cfg(debug_assertions)]
    instance_id: u8,
}

impl HandleTable {
    /// An empty table. In debug it claims the next cross-instance id (MD §16).
    pub(crate) fn new() -> Self {
        HandleTable {
            slots: Vec::new(),
            free_head: None,
            #[cfg(debug_assertions)]
            instance_id: NEXT_INSTANCE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        }
    }

    /// Packs a slot `index` and `generation` into a [`Handle`] (E§4.2). In debug the owning
    /// instance's id rides the top 8 bits, leaving 24 for the index (MD §16); release uses
    /// the full 32-bit index. The generation is always the low 32 bits.
    fn pack(&self, index: u32, generation: u32) -> Handle {
        #[cfg(debug_assertions)]
        {
            debug_assert!(
                index < (1 << 24),
                "handle index {index} exceeds the 24-bit debug field (MD §16 id steals 8 bits)"
            );
            Handle(
                (u64::from(self.instance_id) << 56)
                    | (u64::from(index & 0xFF_FFFF) << 32)
                    | u64::from(generation),
            )
        }
        #[cfg(not(debug_assertions))]
        {
            Handle((u64::from(index) << 32) | u64::from(generation))
        }
    }

    /// Interns `value` as a fresh handle with one reference — a value crossing to the
    /// host (E§4.2). Reuses a freed slot (keeping its bumped generation) or grows.
    pub(crate) fn intern(&mut self, value: Value) -> Handle {
        match self.free_head {
            Some(index) => {
                let Slot::Free {
                    generation,
                    next_free,
                } = self.slots[index as usize]
                else {
                    unreachable!("handle free_head pointed at an occupied slot");
                };
                self.free_head = next_free;
                self.slots[index as usize] = Slot::Occupied {
                    value,
                    generation,
                    refs: 1,
                };
                self.pack(index, generation)
            }
            None => {
                let index = u32::try_from(self.slots.len())
                    .expect("handle table exceeds the u32 index space");
                // Generations are **1-based** so a live handle never encodes to `0`: slot 0 at
                // generation 0 would pack to `0`, which the host boundary reserves as the null
                // handle (E§4.2). Starting at 1 (and skipping 0 on the wrap-reuse in `release`)
                // keeps `0` reserved, so the first minted handle differs from "no value".
                self.slots.push(Slot::Occupied {
                    value,
                    generation: 1,
                    refs: 1,
                });
                self.pack(index, 1)
            }
        }
    }

    /// The slot index a live handle names, or a [`HandleError`] — the boundary generation
    /// check (MD §16), plus the debug cross-instance id check. A handle whose index+generation
    /// name a live slot here but whose embedded debug id belongs to a *different* instance is
    /// [`HandleError::ForeignInstance`] (a host bug), distinct from the [`Stale`](HandleError::
    /// Stale) a released/forged handle yields. The id check runs only once the generation matches,
    /// so a stale or forged handle stays `Stale` and only a genuine live-slot collision surfaces
    /// as `ForeignInstance`.
    fn live_index(&self, handle: Handle) -> Result<usize, HandleError> {
        match self.slots.get(handle.index() as usize) {
            Some(Slot::Occupied { generation, .. }) if *generation == handle.generation() => {
                #[cfg(debug_assertions)]
                if handle.instance_id() != self.instance_id {
                    return Err(HandleError::ForeignInstance);
                }
                Ok(handle.index() as usize)
            }
            _ => Err(HandleError::Stale),
        }
    }

    /// Adds a reference (E§4.2 `retain`). Errors on a stale handle.
    pub(crate) fn retain(&mut self, handle: Handle) -> Result<Handle, HandleError> {
        let index = self.live_index(handle)?;
        let Slot::Occupied { refs, .. } = &mut self.slots[index] else {
            unreachable!("live_index returned a free slot");
        };
        *refs += 1;
        Ok(handle)
    }

    /// Drops a reference (E§4.2 `release`); at zero references frees the slot and
    /// **bumps its generation**, so any handle still naming it becomes stale. Errors
    /// on a stale handle (a double-release is caught).
    pub(crate) fn release(&mut self, handle: Handle) -> Result<(), HandleError> {
        let index = self.live_index(handle)?;
        let Slot::Occupied {
            refs, generation, ..
        } = &mut self.slots[index]
        else {
            unreachable!("live_index returned a free slot");
        };
        *refs -= 1;
        if *refs == 0 {
            // Bump the generation so any handle still naming this slot goes stale; skip `0` on
            // the (astronomically rare) 2^32 wrap so generations stay 1-based and no reused slot
            // 0 ever encodes a live handle to the reserved null value (see `intern`).
            let next_generation = match generation.wrapping_add(1) {
                0 => 1,
                n => n,
            };
            self.slots[index] = Slot::Free {
                generation: next_generation,
                next_free: self.free_head,
            };
            self.free_head = Some(index as u32);
        }
        Ok(())
    }

    /// The value a handle names, with the boundary generation check (E§4.2). The
    /// typed host readers (`as_int`, `kind_of`, …, `machine/boundary.rs`) build on it.
    pub(crate) fn resolve(&self, handle: Handle) -> Result<Value, HandleError> {
        let index = self.live_index(handle)?;
        let Slot::Occupied { value, .. } = &self.slots[index] else {
            unreachable!("live_index returned a free slot");
        };
        Ok(*value)
    }

    /// The value of every live handle — GC roots (machine-design §15/§16). A live
    /// handle keeps its value reachable across a collection.
    pub(crate) fn root_values(&self) -> impl Iterator<Item = Value> + '_ {
        self.slots.iter().filter_map(|slot| match slot {
            Slot::Occupied { value, .. } => Some(*value),
            Slot::Free { .. } => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Value::Int` needs no heap; the table's mechanics are value-agnostic.
    fn int(n: i64) -> Value {
        Value::Int(n)
    }

    #[test]
    fn intern_and_resolve_round_trip() {
        let mut t = HandleTable::new();
        let a = t.intern(int(10));
        let b = t.intern(int(20));
        assert_ne!(a, b);
        assert_eq!(t.resolve(a).unwrap().as_int(), Some(10));
        assert_eq!(t.resolve(b).unwrap().as_int(), Some(20));
    }

    #[test]
    fn retain_needs_a_matching_release_to_free() {
        let mut t = HandleTable::new();
        let h = t.intern(int(7)); // refs = 1
        t.retain(h).unwrap(); // refs = 2
        t.release(h).unwrap(); // refs = 1 — still live
        assert!(t.resolve(h).is_ok());
        t.release(h).unwrap(); // refs = 0 — freed
        assert_eq!(t.resolve(h).err(), Some(HandleError::Stale));
    }

    #[test]
    fn a_reused_slot_makes_the_old_handle_stale() {
        let mut t = HandleTable::new();
        let old = t.intern(int(1));
        t.release(old).unwrap(); // frees slot 0, bumps its generation
        let new = t.intern(int(2)); // reuses slot 0 under the new generation
        // Same slot index, different generation: the handles are distinct and the
        // stale one is caught on every boundary op.
        assert_ne!(old, new);
        assert_eq!(t.resolve(old).err(), Some(HandleError::Stale));
        assert_eq!(t.retain(old), Err(HandleError::Stale));
        assert_eq!(t.release(old), Err(HandleError::Stale));
        assert_eq!(t.resolve(new).unwrap().as_int(), Some(2));
    }

    #[test]
    fn no_live_handle_encodes_to_zero() {
        // `0` is the reserved null handle at the host boundary (E§4.2, `DOODLE_NULL_HANDLE`), so
        // no *live* handle may pack to `0` — else the first minted handle (slot 0) would read as
        // "no value". The first intern must not be `0`, and a re-used slot 0 must not either.
        let mut t = HandleTable::new();
        let first = t.intern(int(1));
        assert_ne!(
            first.bits(),
            0,
            "the first handle must not collide with the null handle"
        );
        t.release(first).unwrap();
        let reused = t.intern(int(2)); // reuses slot 0 under a bumped generation
        assert_ne!(
            reused.bits(),
            0,
            "a re-used slot 0 must not encode to the null handle"
        );
        // A handle rebuilt from the reserved null bits is stale, never a live value.
        assert_eq!(
            t.resolve(Handle::from_bits(0)).err(),
            Some(HandleError::Stale)
        );
    }

    /// A table with an explicit cross-instance id, so the cross-instance test controls the
    /// ids deterministically rather than depending on the process-global counter's order
    /// (parallel tests share it). Debug-only: the id field does not exist in release.
    #[cfg(debug_assertions)]
    fn table_with_id(instance_id: u8) -> HandleTable {
        HandleTable {
            slots: Vec::new(),
            free_head: None,
            instance_id,
        }
    }

    /// MD §16: a handle minted by one instance and used on another whose slot layout collides
    /// bit-for-bit — both interned a first value, so both name index 0 at generation 1. Only the
    /// debug instance-id in the top bits tells them apart; without it (release / the old design)
    /// `from_a` would silently resolve against `b`'s own slot 0. This is the exact case a
    /// value-keyed minted-set could not express: the two handles' low 48 bits are equal.
    #[cfg(debug_assertions)]
    #[test]
    fn a_handle_from_another_instance_is_caught_as_foreign() {
        let mut a = table_with_id(1);
        let mut b = table_with_id(2);
        let from_a = a.intern(int(10));
        let from_b = b.intern(int(20));
        // Index + generation collide; only the embedded id differs.
        assert_eq!(from_a.index(), from_b.index());
        assert_eq!(from_a.generation(), from_b.generation());
        assert_ne!(from_a.instance_id(), from_b.instance_id());
        // `b` resolves its own handle, but rejects `a`'s look-alike as foreign — not stale, so a
        // host learns it mixed up instances rather than being told to retry.
        assert_eq!(b.resolve(from_b).unwrap().as_int(), Some(20));
        assert_eq!(b.resolve(from_a).err(), Some(HandleError::ForeignInstance));
        assert_eq!(b.retain(from_a), Err(HandleError::ForeignInstance));
        assert_eq!(b.release(from_a), Err(HandleError::ForeignInstance));
    }

    #[test]
    fn root_values_lists_live_handles_only() {
        let mut t = HandleTable::new();
        let a = t.intern(int(1));
        let _b = t.intern(int(2));
        t.release(a).unwrap();
        let live: Vec<_> = t.root_values().filter_map(Value::as_int).collect();
        assert_eq!(live, vec![2]); // `a` was released; only `b` remains a root
    }
}
