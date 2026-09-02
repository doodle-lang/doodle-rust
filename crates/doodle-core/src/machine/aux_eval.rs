//! Auxiliary evaluation (engine spec E§8.4, S-22): the host renders a value the way the
//! program would — by driving its `to_string` (L§15) — at a **stopped** instance, without
//! disturbing the pause. This is the one **effectful** part of inspection: it runs Doodle
//! code and may raise or fault, unlike the pure structural inspection of §4.4.
//!
//! A value whose type has an explicit `implement Stringable` drives that `to_string` in a
//! **nested drive** (like a native block-consumer's reentrant drive, MD §14) over a
//! **saved/restored debug context**: the register, in-flight unwind, budget, and stepping
//! bookkeeping are snapshotted, the aux drive runs on its **own small step budget** with
//! **breakpoints and raise-trap suppressed** (it steps through `step::step`, not the debug
//! drive loop), and the context is restored afterward so the outer pause is byte-for-byte
//! intact (heap allocations aside — a partial render's garbage is reclaimed by GC). A value
//! with no explicit `Stringable` renders through the pure native seam, no drive at all.

use super::error::{ExceptionKind, Raise};
use super::limits::FusedCounter;
use super::protocol::{self, Dispatch};
use super::ring::RingBuffer;
use super::unwind::Unwind;
use super::value::CalIdx;
use super::{Halt, Handle, Instance, Value, exception, step, stringify};
use crate::drive::{EngineFault, LimitKind, Limits};
use crate::span::ModuleId;
use std::sync::Arc;

/// The result of a host-driven `to_string` (E§8.4): the rendered value, an exception it
/// raised, or a fault (its own budget exhausted, or a forbidden nested suspend — a `to_string`
/// that calls a suspending capability, which a nested drive cannot freeze, S-15). Every case
/// leaves the outer instance's pause intact. `Rendered`/`Raised` carry fresh **host-owned**
/// handles (release them).
#[derive(Clone, Copy, Debug)]
pub enum AuxOutcome {
    /// `to_string` returned a String — a handle to it (a non-String result `Raised`s instead).
    Rendered(Handle),
    /// `to_string` raised — a handle to the raised value.
    Raised(Handle),
    /// The aux drive faulted without a value (budget exhausted, or a forbidden nested suspend).
    Faulted(EngineFault),
}

/// The raw (un-interned) outcome of the nested drive, before the debug context is restored.
enum AuxResult {
    Completed(Value),
    Raised(Value),
    Faulted(EngineFault),
}

impl Instance {
    /// Renders `handle`'s value the way the program would (E§8.4): drives its `to_string`
    /// (L§15) if its type has an explicit `implement Stringable`, else uses the native seam.
    /// **Effectful** — it runs Doodle code, so it may [`Raised`](AuxOutcome::Raised) or
    /// [`Faulted`](AuxOutcome::Faulted) — but it never disturbs the instance's pause: a nested
    /// drive runs under a saved/restored debug context and with breakpoints + raise-trap
    /// suppressed (S-22). `fuel` is the aux drive's **own** step bound — a required per-call
    /// argument (as with a bounded run, E§7.3), untouched by and separate from the program's
    /// budget; exhausting it faults the aux drive (one-shot, never paused), leaving the paused
    /// program intact. Call it at a stopped instance, like the rest of the observation surface.
    pub fn eval_to_string(&mut self, handle: Handle, fuel: u64) -> AuxOutcome {
        let Ok(value) = self.value_of(handle) else {
            // A stale handle is a host-contract violation; report it as an internal fault
            // rather than panicking (the instance itself is untouched).
            return AuxOutcome::Faulted(EngineFault::Internal);
        };
        match self.stringable_to_string(value) {
            Some(cal) => self.drive_aux_to_string(cal, value, fuel),
            None => {
                // No explicit `Stringable`: the pure native seam (scalars final, compounds a
                // placeholder) — no Doodle code, so no drive and nothing to save/restore.
                let rendered = stringify::render(&self.heap, value);
                let idx = self.heap.alloc_string(rendered.into());
                AuxOutcome::Rendered(self.intern(Value::Str(idx)))
            }
        }
    }

    /// The `Stringable.to_string` implementation for `value`'s type, if it has an explicit
    /// `implement Stringable` (L§15, D-M5-1) — the same hidden-member dispatch interpolation
    /// uses (S-37), so `eval_to_string` and `{value}` render identically. `None` for a type
    /// that renders through the native seam.
    fn stringable_to_string(&self, value: Value) -> Option<CalIdx> {
        let member = self.machine.protocols.to_string_member()?;
        let filter = self.machine.protocols.stringable_id()?;
        let dt =
            protocol::dispatch_type_of(value, &self.heap, &self.modules, &self.machine.intrinsics);
        match self
            .machine
            .protocols
            .resolve(member, dt, Some(filter), &self.heap)
        {
            Dispatch::Call(cal) => Some(cal),
            _ => None,
        }
    }

    /// Drives `cal` (a `to_string`) on `value` in a nested drive under a saved/restored debug
    /// context (E§8.4, S-22): the outer register, unwind, budget, stepping bookkeeping, and
    /// stack heights are snapshotted; the aux drive runs on its own small budget; then the
    /// context is restored so the pause is intact. Any frames/bindings the aux drive left above
    /// the boundary (a fault mid-render) are truncated away.
    fn drive_aux_to_string(&mut self, cal: CalIdx, value: Value, fuel: u64) -> AuxOutcome {
        // The nested drive nests on the host's Rust stack (MD §14): refuse if that would
        // overflow it, exactly as a reentrant block invocation does.
        if self.machine.reentry_would_overflow() {
            return AuxOutcome::Faulted(EngineFault::LimitExceeded(LimitKind::StackDepth));
        }
        // Snapshot the debug context. `mem::take`/`mem::replace` for the owned fields (unwind,
        // fuel, pending); a plain copy for the rest. Clearing `unwind` is load-bearing: at a
        // raise-trap pause the outer instance has an armed raise, which a nested `step::step`
        // would otherwise unwind.
        let saved_reg = self.machine.reg;
        let saved_unwind = std::mem::take(&mut self.machine.unwind);
        let aux_limits = Limits {
            step_budget: fuel,
            ..self.machine.limits
        };
        let saved_fuel = std::mem::replace(&mut self.machine.fuel, FusedCounter::new(&aux_limits));
        let saved_fine = self.machine.fine_span;
        let saved_stmt = self.machine.safe_point_stmt;
        let saved_directive = self.machine.directive;
        let saved_pending = std::mem::take(&mut self.machine.pending);
        let saved_pending_fault = self.machine.pending_fault.take();
        let saved_ring: RingBuffer = self.machine.ring.clone();
        let saved_gc_threshold = self.machine.gc_threshold;
        let frames_len = self.machine.frames.len();
        let dyn_len = self.machine.dyn_stack.len();
        let handling_len = self.machine.handling.len();
        let foreign_len = self.machine.foreign_roots.len();

        // Keep the **saved context's** heap values rooted while the nested drive runs. They were
        // moved out of the machine fields above (reg/unwind/pending), so `gc::collect` — which
        // roots them only through those live fields — no longer reaches them, and a collection
        // inside the aux drive would sweep them, corrupting the restored pause. The sharp case is a
        // raise-trap pause: the trapped raise value is reachable *only* from `unwind`, so clearing
        // `unwind` (load-bearing, above) leaves it unrooted. `foreign_roots` exists for exactly
        // this — rooting Rust-stack-held values across a reentrant drive — and the
        // `truncate(foreign_len)` below drops these afterward (captured before the pushes).
        if let Some(v) = saved_reg {
            self.machine.foreign_roots.push(v);
        }
        if let Some(v) = saved_unwind.as_ref().and_then(|u| u.gc_value()) {
            self.machine.foreign_roots.push(v);
        }
        if let Some(super::modload::Suspension::Capability(pending)) = &saved_pending {
            for value in &pending.args {
                self.machine.foreign_roots.push(*value);
            }
        }

        self.machine.enter_reentry();
        let result = self.run_aux_drive(cal, value, frames_len);
        self.machine.exit_reentry();

        // Restore the debug context. `frames`/`handling`/`foreign_roots` truncate to their saved
        // heights — a completion or raise already drained there, and a fault's partial entries
        // hold no cell state needing writeback. `dyn_stack` must be **restored**, not truncated:
        // a fault mid-`with` leaves `(cell, old)` save entries whose values must be written back
        // into their (program-shared) parameter cells, or the aux drive's `with` binding leaks into
        // the outer program (`restore` is a no-op when already drained to `dyn_len`).
        self.machine.frames.truncate(frames_len);
        super::unwind::restore(&mut self.machine, &mut self.heap, dyn_len as u32);
        self.machine.handling.truncate(handling_len);
        self.machine.foreign_roots.truncate(foreign_len);
        self.machine.gc_threshold = saved_gc_threshold;
        self.machine.reg = saved_reg;
        self.machine.unwind = saved_unwind;
        self.machine.fuel = saved_fuel;
        self.machine.fine_span = saved_fine;
        self.machine.safe_point_stmt = saved_stmt;
        self.machine.directive = saved_directive;
        self.machine.pending = saved_pending;
        self.machine.pending_fault = saved_pending_fault;
        self.machine.ring = saved_ring;

        // Intern the produced value **after** restoring (the heap is untouched by restore, so
        // the value is still live; interning mints a fresh host-owned handle).
        match result {
            AuxResult::Completed(v) => AuxOutcome::Rendered(self.intern(v)),
            AuxResult::Raised(v) => AuxOutcome::Raised(self.intern(v)),
            AuxResult::Faulted(fault) => AuxOutcome::Faulted(fault),
        }
    }

    /// The nested-drive loop (guarded by [`drive_aux_to_string`]): pushes the `to_string` frame
    /// and steps through `step::step` to its completion, returning the raw value / raise / fault.
    /// Mirrors the reentrant block-consumer drive, but its `boundary` can be `0` (aux eval at a
    /// completed instance), so a raise draining the whole aux stack surfaces as `Halt::Raise`.
    fn run_aux_drive(&mut self, cal: CalIdx, value: Value, boundary: usize) -> AuxResult {
        // The `to_string` frame's call site: its own declaration (there is no source call). It
        // is only read if the render raises (its trace), and is valid in the callee's module.
        let obj = self.heap.callable(cal);
        let module = obj.module.0 as usize;
        let decl = self.modules[module].resolved.callables[obj.source_id() as usize].decl;
        let span = self.modules[module].resolved.ast.span(decl);
        if let Err(raise) = protocol::enter_unary(
            &self.modules,
            &mut self.heap,
            &mut self.machine,
            cal,
            value,
            decl,
            span,
        ) {
            // A malformed `to_string` (wrong arity/block, §dispatch): materialize its raise.
            return AuxResult::Raised(self.materialize(raise));
        }
        loop {
            if self.machine.frames.len() <= boundary {
                return match std::mem::take(&mut self.machine.unwind) {
                    // A well-formed `to_string` returns a String; a non-String raises the same
                    // `type-mismatch` interpolation would (E§8.4 rider) — the render previews
                    // the program's truth rather than a value the program itself cannot render.
                    None => match self.machine.reg.unwrap_or(Value::Nil) {
                        value @ Value::Str(_) => AuxResult::Completed(value),
                        value => AuxResult::Raised(self.non_string_result(value)),
                    },
                    // The render raised and unwound to the boundary (a caller `try` below it is
                    // never reached — the aux render is a separate evaluation).
                    Some(Unwind::Raise { value, .. }) => AuxResult::Raised(value),
                    // The host pressed stop during the aux render: `poll_cancel` armed a cancel and
                    // drained here. The cancel flag stays set, so the outer resume observes it too;
                    // report the aux drive's own outcome as cancelled, not a spurious `Internal`.
                    Some(Unwind::Cancel) => AuxResult::Faulted(EngineFault::Cancelled),
                    // A `break`/`return` cannot escape a `to_string` callable to the boundary (a
                    // valued `break` with no consumer raises, S-10); any other unwind here is a
                    // broken invariant.
                    Some(_) => AuxResult::Faulted(EngineFault::Internal),
                };
            }
            let cur = self.machine.frames.last().map_or(ModuleId(0), |f| f.module);
            let resolved = Arc::clone(&self.modules[cur.0 as usize].resolved);
            match step::step(
                &resolved,
                &mut self.modules,
                &mut self.heap,
                &mut self.machine,
            ) {
                Ok(_) => {
                    // A suspending capability inside the nested drive is forbidden (S-15): it
                    // cannot be frozen and resumed on the Rust stack. Clear it and fault.
                    if self.machine.pending.is_some() {
                        self.machine.pending = None;
                        return AuxResult::Faulted(EngineFault::NestedSuspend);
                    }
                }
                // With `boundary == 0` a raise drains the whole aux stack and surfaces here.
                Err(Halt::Raise(value, _)) => return AuxResult::Raised(value),
                Err(Halt::Fault(fault)) => return AuxResult::Faulted(fault),
            }
        }
    }

    /// The `type-mismatch` `Error` value for a `to_string` that returned a non-String `value`
    /// (E§8.4 rider, L§15) — the same exception the program's interpolation raises, with the
    /// `{operator, expected, got}` details (S-58 schema).
    fn non_string_result(&mut self, value: Value) -> Value {
        let details =
            super::exception::type_mismatch_details("to_string", &["String"], value, &self.heap);
        exception::make_error(
            &mut self.heap,
            self.machine.error_type,
            ExceptionKind::TypeMismatch.slug(),
            "a `to_string` implementation must return a String",
            &details,
        )
    }

    /// Materializes an engine [`Raise`] into its `Error` record value (L§12.1) — for a raise
    /// that surfaced before the aux drive pushed a frame (a malformed `to_string`).
    fn materialize(&mut self, raise: Raise) -> Value {
        exception::make_error(
            &mut self.heap,
            self.machine.error_type,
            raise.exception.kind.slug(),
            &raise.exception.message,
            &raise.details,
        )
    }
}
