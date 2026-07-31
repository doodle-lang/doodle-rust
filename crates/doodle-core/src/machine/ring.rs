//! The tail-elided-frame ring buffer (E§8.3, machine-design §8/§11): a bounded
//! history of the frames that tail-call reuse overwrote, so tooling can later
//! reconstruct the elided calls even though only one frame slot survives.
//!
//! **Scope (M2a.7).** The buffer is **populated** on each tail reuse. Its entries
//! are consumed later: as **GC roots** (each holds a callable reference, §15,
//! M2a.10) and by the **E§8.3 observation** surface (the engine API, M2b+). Until
//! then nothing reads them — hence `#[allow(dead_code)]` — but the history must be
//! recorded now, at the point the frames are elided, or it is lost.

use crate::machine::CalIdx;

/// The bounded depth of the tail-elided history (E§8.3). Beyond this the oldest
/// entries are overwritten — a debugger sees the most recent elided frames.
const RING_CAPACITY: usize = 64;

/// One elided frame (machine-design §11, S-34): the callable whose activation was
/// overwritten by a tail call, tagged with the consuming (reused) frame's serial.
#[allow(dead_code)] // read at M2a.10 (GC roots) and by E§8.3 observation (M2b+)
#[derive(Clone, Copy, Debug)]
pub(crate) struct ElidedFrame {
    /// The overwritten frame's callable value.
    pub(crate) callable: CalIdx,
    /// The reusing frame's `serial` (which activation absorbed this one).
    pub(crate) consuming_serial: u64,
}

/// A fixed-capacity ring of [`ElidedFrame`]s (E§8.3). Recording is O(1) and never
/// grows past [`RING_CAPACITY`]; the oldest entry is overwritten when full.
pub(crate) struct RingBuffer {
    /// Entries, growing to `RING_CAPACITY` then reused cyclically at `next`.
    entries: Vec<ElidedFrame>,
    /// The next write position (cyclic once `entries` is at capacity).
    next: usize,
}

impl RingBuffer {
    /// An empty ring.
    pub(crate) fn new() -> Self {
        RingBuffer {
            entries: Vec::new(),
            next: 0,
        }
    }

    /// Records an elided frame, overwriting the oldest once at capacity.
    pub(crate) fn record(&mut self, entry: ElidedFrame) {
        if self.entries.len() < RING_CAPACITY {
            self.entries.push(entry);
        } else {
            self.entries[self.next] = entry;
        }
        self.next = (self.next + 1) % RING_CAPACITY;
    }
}
