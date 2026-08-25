//! The suspend/resume half of the [`Instance`] drive lifecycle (engine spec E§7.5):
//! parking a capability request, building its host-facing request, and injecting a
//! host resolution (a value, or a raise). Split from `machine.rs` (the `Instance`
//! definition and load path) so that file stays within the hygiene length limit; the
//! drive loop that calls these lives in [`crate::drive`].

use super::error::Trace;
use super::handle::HandleError;
use super::{Handle, Instance, Value};
use crate::drive::{CapabilityId, CapabilityRequest, Directive};
use crate::resolve::BodyKind;

impl Instance {
    /// Whether the instance has parked a capability request — it is mid-suspension
    /// (E§7.5). The drive loop checks this after each step to return `Suspended`.
    pub(crate) fn is_suspended(&self) -> bool {
        self.machine.pending.is_some()
    }

    /// Records the directive the current drive runs under, for a later `resolve` to
    /// resume under it (E§7.3).
    pub(crate) fn set_directive(&mut self, directive: Directive) {
        self.machine.directive = directive;
    }

    /// The directive to resume a suspended drive under (E§7.3).
    pub(crate) fn resume_directive(&self) -> Directive {
        self.machine.directive
    }

    /// Arms the slice fuel for the drive call about to start (S-40): `Some(n)` runs at
    /// most `n` statement safe points before `Paused(SliceEnd)`; `None` runs unbounded.
    pub(crate) fn arm_slice(&mut self, fuel: Option<u64>) {
        self.machine.fuel.arm_slice(fuel);
    }

    /// Whether the current drive slice's fuel is spent (S-40) — the drive loop's signal
    /// to return `Paused(SliceEnd)`.
    pub(crate) fn sliced_out(&self) -> bool {
        self.machine.fuel.sliced_out()
    }

    /// Builds the capability request for the parked suspension (E§7.5): the capability
    /// identity and its bound arguments as fresh **host-owned** handles (S-17 — the
    /// host releases them). Leaves the pending request in place (consumed by `resolve`).
    pub(crate) fn capability_request(&mut self) -> CapabilityRequest {
        let (capability, values) = {
            let pending = self.machine.pending.as_ref().expect("a parked request");
            (pending.capability, pending.args.clone())
        };
        let args = values
            .into_iter()
            .map(|value| self.machine.handles.intern(value))
            .collect();
        CapabilityRequest {
            capability: CapabilityId(capability),
            args,
        }
    }

    /// Resolves a suspension with the host's value (E§7.5): sets the register to it (or
    /// Void for a `to` capability), clears the pending request. `resolve` then resumes
    /// the drive so the caller's continuation consumes the result.
    pub(crate) fn resume_with_value(&mut self, handle: Handle) -> Result<(), HandleError> {
        // Take the pending request first, so a stale resolution handle (a host-contract
        // violation) still clears the suspension — the drive then faults it terminally
        // rather than leaving a resumable half-state (`resolve`, E§3.3).
        let pending = self.machine.pending.take().expect("a parked request");
        let value = self.machine.handles.resolve(handle)?;
        // A `to` capability yields Void regardless of the resolution value (E§7.5).
        self.machine.reg = if self.machine.intrinsics.kind_of(pending.capability) == BodyKind::Proc
        {
            None
        } else {
            Some(value)
        };
        Ok(())
    }

    /// Resolves a suspension with a host raise (E§7.5/§9): clears the pending request and
    /// **arms a Raise unwind** carrying the host-supplied value at the capability call
    /// site. The value *is* the raised exception (a program `rescue e` binds it as-is);
    /// re-driving runs cleanup and lets a `try` around the call catch it, or drains to the
    /// terminal `Raised`. Errors on a stale handle.
    pub(crate) fn resume_with_raise(&mut self, handle: Handle) -> Result<(), HandleError> {
        // Take the pending request first (see `resume_with_value`): a stale handle must
        // still clear the suspension so the drive can fault it terminally.
        let pending = self.machine.pending.take().expect("a parked request");
        let value = self.machine.handles.resolve(handle)?;
        self.machine.arm_raise_value(
            value,
            Trace {
                raised_at: Some(pending.span),
            },
        );
        Ok(())
    }

    /// Describes a raised value for host display (E§9): its `(kind_slug, message)`. An
    /// `Error` record reports its own `kind`/`message` fields; any other raised value the
    /// generic `"raised"` with a best-effort message. For a host mapping an
    /// [`Outcome::Raised`](crate::drive::Outcome::Raised) to a diagnostic.
    pub fn describe_raised(&self, value: Value) -> (String, String) {
        super::exception::describe(&self.heap, self.machine.error_type, value)
    }
}
