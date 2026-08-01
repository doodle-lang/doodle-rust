//! GC root enumeration (machine-design §15): the machine-side half of a collection.
//! [`collect`] walks the live machine state — the frame stack, the result register,
//! any in-flight unwind, the tail-elided ring, and the module namespace — and seeds
//! each heap reference it finds into the heap's precise collector
//! ([`Heap::collect`](crate::heap::Heap)), which marks transitively and sweeps.
//!
//! **The root set (M2a.10).** From every frame: its running callable (a `Callable`
//! frame's `cal`, which must outlive no-other-reference cases like an
//! immediately-invoked closure), its direct/cell-boxed `locals`, and the `Value`s
//! stashed in its continuations. Plus the register, the unwind record's carried
//! value, the ring's callables, and every namespace binding cell. A frame's
//! `block_param`, `Block` static link, and `Consumer` hold frame indices/serials —
//! not heap references — so they root nothing. Roots that join later as their state
//! lands: the handle table (M2a.11), the dynamic-parameter stack (M4), and
//! suspended-capability args + the drive stack (M2b).

use super::Machine;
use super::control::Namespace;
use super::frame::{FrameKind, Local};
use crate::heap::Heap;

/// Runs one precise, non-moving collection (machine-design §15): enumerate every
/// root reachable from the machine state into the heap collector, which marks the
/// reachable graph and sweeps the rest.
pub(crate) fn collect(heap: &mut Heap, machine: &Machine, namespace: &Namespace) {
    heap.collect(|tracer| {
        for frame in &machine.frames {
            // The value the frame is executing: a running callable must survive even
            // with no other live reference (its captures live through it).
            if let FrameKind::Callable { cal } = frame.kind {
                tracer.callable(cal);
            }
            for local in &frame.locals {
                match local {
                    Local::Direct(Some(v)) => tracer.value(*v),
                    Local::Boxed(cell) => tracer.cell(*cell),
                    Local::Direct(None) => {}
                }
            }
            for cont in &frame.conts {
                cont.each_value(|v| tracer.value(v));
            }
        }
        if let Some(v) = machine.reg {
            tracer.value(v);
        }
        if let Some(v) = machine.unwind.and_then(|u| u.gc_value()) {
            tracer.value(v);
        }
        for cal in machine.ring.callables() {
            tracer.callable(cal);
        }
        for (_, cell) in namespace {
            tracer.cell(*cell);
        }
    });
}
