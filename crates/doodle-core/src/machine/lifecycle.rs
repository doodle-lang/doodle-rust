//! The suspend/resume half of the [`Instance`] drive lifecycle (engine spec E§7.5):
//! parking a capability request, building its host-facing request, and injecting a
//! host resolution (a value, or a raise). Split from `machine.rs` (the `Instance`
//! definition and load path) so that file stays within the hygiene length limit; the
//! drive loop that calls these lives in [`crate::drive`].

use super::error::{Exception, ExceptionKind, Trace};
use super::handle::HandleError;
use super::{Handle, Instance, intrinsic};
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

    /// Resolves a suspension with a host raise (E§7.5): clears the pending request and
    /// builds the exception + trace to surface as `Raised` at the capability call site.
    /// **Provisional (M2b.4):** the host-raised value is rendered into the message; the
    /// value-carrying exception that `rescue` binds arrives with exceptions-as-values
    /// (M4, E§9). Errors on a stale handle.
    pub(crate) fn resume_with_raise(
        &mut self,
        handle: Handle,
    ) -> Result<(Exception, Trace), HandleError> {
        // Take the pending request first (see `resume_with_value`): a stale handle must
        // still clear the suspension so the drive can fault it terminally.
        let pending = self.machine.pending.take().expect("a parked request");
        let value = self.machine.handles.resolve(handle)?;
        let rendered = intrinsic::render(&self.heap, value);
        let exception = Exception {
            kind: ExceptionKind::HostRaised,
            message: format!("a capability call was rejected by the host: {rendered}"),
        };
        Ok((
            exception,
            Trace {
                raised_at: Some(pending.span),
            },
        ))
    }
}
