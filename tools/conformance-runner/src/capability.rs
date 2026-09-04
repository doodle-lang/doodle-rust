//! The conformance registry **manifest**: the intrinsics + capabilities the runner installs, in a
//! fixed registration order, plus the capability name↔id map that order induces.
//!
//! Registration order is replay identity (E§5.5, S-43): a capability's `CapabilityId` is its
//! registration index, so a `#! input:`/`resolve:`/`suspended` directive that names a capability
//! must resolve to the same index the engine reports in an `Outcome::Suspended`. This one ordered
//! list is the single source of truth for **both** the registry and the map, and it is the
//! **portable manifest** (M7.5b, normative in `conformance/README.md`): the wasm surface
//! (`DoodleInstance.demo`) installs the identical set in the identical order, and the M7 C host will
//! too. Any divergence is a determinism leak (E§11); the M7.5d trace schema catches it.

use doodle_core::machine::{
    Intrinsic, Registry, decode_intrinsic, each_intrinsic, encode_intrinsic, length_intrinsic,
    print_intrinsic, random_intrinsic, read_line_intrinsic, time_intrinsic,
};

/// The registry manifest, in registration order: `(name, constructor, is_suspending_capability)`.
/// The index is the `CapabilityId` for the suspending entries. `print`/`length`/`each`/`encode`/
/// `decode` are synchronous (never suspend); `read_line`/`time`/`random` are the suspending
/// capabilities a fixture scripts with `input:`/`resolve:`. The trig + drawing capabilities append
/// here (preserving these indices) when the cross-surface turtle fixture lands (M7.5d).
#[allow(clippy::type_complexity)]
const MANIFEST: &[(&str, fn() -> Intrinsic, bool)] = &[
    ("print", print_intrinsic, false),
    ("length", length_intrinsic, false),
    ("each", each_intrinsic, false),
    ("encode", encode_intrinsic, false),
    ("decode", decode_intrinsic, false),
    ("read_line", read_line_intrinsic, true),
    ("time", time_intrinsic, true),
    ("random", random_intrinsic, true),
];

/// Builds the conformance registry: every manifest entry registered in order (E§5.5).
pub(crate) fn registry() -> Registry {
    let mut registry = Registry::new();
    for &(_, ctor, _) in MANIFEST {
        registry
            .register(ctor())
            .expect("a conformance intrinsic registers cleanly into a fresh registry");
    }
    registry
}

/// The capability named by registration index `id` (an `Outcome::Suspended`'s `CapabilityId`), or
/// `None` if `id` is out of range or names a non-suspending intrinsic.
pub(crate) fn capability_name(id: u32) -> Option<&'static str> {
    MANIFEST
        .get(id as usize)
        .filter(|(_, _, capability)| *capability)
        .map(|(name, _, _)| *name)
}

/// The `CapabilityId` (registration index) of the capability named `name`, or `None` if no
/// suspending capability has that name. The inverse of [`capability_name`]; the transcript emitter
/// (M7.5d) and the C surface (M7.5e) map fixture-declared names to ids through it. (Allowed dead in
/// the library build until then; already exercised by the manifest round-trip test.)
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn capability_id(name: &str) -> Option<u32> {
    MANIFEST
        .iter()
        .position(|(n, _, capability)| *capability && *n == name)
        .map(|i| i as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_and_id_round_trip_for_every_capability() {
        // The manifest is the single source of truth: every suspending entry's id maps back to its
        // name and vice versa, at the registration index the engine will report.
        for (index, &(name, _, capability)) in MANIFEST.iter().enumerate() {
            if capability {
                assert_eq!(capability_id(name), Some(index as u32));
                assert_eq!(capability_name(index as u32), Some(name));
            } else {
                // A synchronous intrinsic has no capability identity.
                assert_eq!(capability_name(index as u32), None);
            }
        }
    }

    #[test]
    fn the_three_ambient_capabilities_are_registered() {
        for name in ["read_line", "time", "random"] {
            assert!(capability_id(name).is_some(), "{name} is a capability");
        }
        assert_eq!(capability_id("print"), None, "print is synchronous");
        assert_eq!(capability_id("nope"), None, "unknown name");
    }
}
