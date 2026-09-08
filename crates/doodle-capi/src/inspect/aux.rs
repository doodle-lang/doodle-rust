//! Auxiliary evaluation (engine spec E§8.4, S-22): rendering a value to its `to_string` with its
//! **own** fuel budget, without disturbing the instance's pause. Split from the parent `inspect`
//! for length; reached as `abi::inspect::doodle_eval_to_string` via the parent's re-export.

use super::inst_mut;
use crate::abi::{self, DoodleAuxOutcome, DoodleAuxOutcomeKind, DoodleHandle, DoodleStatus};
use crate::guard::catch;
use crate::instance::DoodleInstance;
use crate::value::write_out;
use doodle_core::drive::EngineFault;
use doodle_core::machine::{AuxOutcome, Handle};

/// Renders `handle`'s value to its `to_string` (E§8.4) with its **own** `fuel` budget, writing
/// the result to `out_outcome` — **without disturbing the instance's pause** (S-22). It runs
/// Doodle code, so it may render, raise, or fault (see [`DoodleAuxOutcome`]); breakpoints and the
/// raise-trap are suppressed, and the pause generation is **not** bumped (frame addressing stays
/// valid across it). `Rendered`/`Raised` carry host-owned handles the host releases.
///
/// # Safety
/// `instance` live; `out_outcome` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_eval_to_string(
    instance: *mut DoodleInstance,
    handle: DoodleHandle,
    fuel: u64,
    out_outcome: *mut DoodleAuxOutcome,
) -> DoodleStatus {
    catch(|| {
        // Validate the out-param before the (effectful, handle-minting) eval, so a NULL out never
        // orphans the Rendered/Raised handle.
        if out_outcome.is_null() {
            return DoodleStatus::ErrNullPointer;
        }
        let inst = match inst_mut(instance) {
            Ok(inst) => inst,
            Err(status) => return status,
        };
        let outcome = match inst.eval_to_string(Handle::from_bits(handle), fuel) {
            AuxOutcome::Rendered(h) => DoodleAuxOutcome {
                kind: DoodleAuxOutcomeKind::Rendered,
                value: h.bits(),
                fault: abi::fault(EngineFault::Internal),
                reserved: [0; 2],
            },
            AuxOutcome::Raised(h) => DoodleAuxOutcome {
                kind: DoodleAuxOutcomeKind::Raised,
                value: h.bits(),
                fault: abi::fault(EngineFault::Internal),
                reserved: [0; 2],
            },
            AuxOutcome::Faulted(f) => DoodleAuxOutcome {
                kind: DoodleAuxOutcomeKind::Faulted,
                value: abi::DOODLE_NULL_HANDLE,
                fault: abi::fault(f),
                reserved: [0; 2],
            },
        };
        if write_out(out_outcome, outcome) {
            DoodleStatus::Ok
        } else {
            DoodleStatus::ErrNullPointer
        }
    })
}
