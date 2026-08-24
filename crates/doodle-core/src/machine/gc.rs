//! GC root enumeration (machine-design §15): the machine-side half of a collection.
//! [`collect`] walks the live machine state — the frame stack, the result register,
//! any in-flight unwind, the tail-elided ring, and the module namespace — and seeds
//! each heap reference it finds into the heap's precise collector
//! ([`Heap::collect`](crate::heap::Heap)), which marks transitively and sweeps.
//!
//! **The root set.** From every frame: its running callable (a `Callable` frame's
//! `cal`, which must outlive no-other-reference cases like an immediately-invoked
//! closure), its direct/cell-boxed `locals`, and the `Value`s stashed in its
//! continuations. Plus the register, the unwind record's carried value, the ring's
//! callables, the **host handle table** (§16, M2a.11), and every namespace binding
//! cell, plus a **parked capability request**'s bound arguments while `Suspended`
//! (§14, M2b.4). A frame's `block_param`, `Block` static link, and `Consumer` hold
//! frame indices/serials — not heap references — so they root nothing. Also the
//! in-flight synchronous foreign-call arguments (§15) and the **dynamic-binding save
//! stack** (§13 — each `with`'s saved prior value). Roots that join later as their
//! state lands: the drive stack (M2b.5).

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
        if let Some(v) = machine.unwind.as_ref().and_then(|u| u.gc_value()) {
            tracer.value(v);
        }
        for cal in machine.ring.callables() {
            tracer.callable(cal);
        }
        // Host handles (§16): a value the host retains stays reachable across a
        // collection until it releases the handle.
        for value in machine.handles.root_values() {
            tracer.value(value);
        }
        for (_, cell) in namespace {
            tracer.cell(*cell);
        }
        // A parked capability request's bound arguments (§14): reachable while the
        // instance is `Suspended`, so a collection at that safe point keeps them alive.
        if let Some(pending) = &machine.pending {
            for value in &pending.args {
                tracer.value(*value);
            }
        }
        // The arguments of in-flight synchronous foreign calls (§15): held only on the
        // host's Rust stack (e.g. a native `each`'s list while its reentrant drive runs),
        // so a collection during that drive must keep them alive.
        for value in &machine.foreign_roots {
            tracer.value(*value);
        }
        // The dynamic-binding save stack (§13): each `with`'s saved prior value stays
        // reachable while the binding is active, so a restore can write it back.
        for (_, value) in &machine.dyn_stack {
            tracer.value(*value);
        }
    });
}
