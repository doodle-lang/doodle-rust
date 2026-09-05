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
    BlockOutcome, ConstValue, ForeignBuilder, HostReply, Intrinsic, IntrinsicCtx, Registry,
    decode_intrinsic, each_intrinsic, encode_intrinsic, length_intrinsic, print_intrinsic,
    random_intrinsic, read_line_intrinsic, time_intrinsic,
};
use doodle_core::resolve::BodyKind;
use std::sync::Arc;

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
    ("test_greet", test_greet, false),
];

/// The S-42 conformance foreign function `test_greet(name, punct = "!", do body)` (E§5.1): a host
/// `fn` with an **immutable default** parameter and a **trailing block** parameter, both binding per
/// L§8.3. It invokes the block once with `name` (a side effect the block owns — e.g. it prints),
/// then returns `name + punct` (the default `"!"`, or a keyword-bound punctuation). A fixture calls
/// it three ways — default omitted, `punct:` keyword, and a block that makes a non-local exit — and
/// the transcript certifies identical L§8.3 binding across native/wasm/C (M7.5d). The C example host
/// (M7.5e) registers the same shape; the D-M7-8 mutable-default rejection is a per-surface host-API
/// unit test, not a fixture (a `List`/`Dict` default is unrepresentable in `ConstValue`).
fn test_greet() -> Intrinsic {
    ForeignBuilder::new("test_greet", BodyKind::Func)
        .param("name")
        .default_param("punct", ConstValue::Str("!".into()))
        .block_param("body")
        .host(Arc::new(|ctx: &mut IntrinsicCtx| {
            let (Some(name), Some(punct)) = (ctx.arg_handle(0), ctx.arg_handle(1)) else {
                ctx.fault_host();
                return HostReply::Value(None);
            };
            // Read both string args to owned bytes before the reentrant drive / construction.
            let (Ok(name_bytes), Ok(punct_bytes)) = (
                ctx.string_bytes(name).map(<[u8]>::to_vec),
                ctx.string_bytes(punct).map(<[u8]>::to_vec),
            ) else {
                ctx.fault_host();
                return HostReply::Value(None);
            };
            // Invoke the trailing block once with `name`. A break/return/raise crossing the boundary
            // means "return promptly, no result" (E§7.6): reply `Value(None)` so the apply site
            // resumes the parked exit.
            let outcome = ctx.invoke_block_handles(&[name]);
            ctx.release(name).ok();
            ctx.release(punct).ok();
            if outcome != BlockOutcome::Completed {
                return HostReply::Value(None);
            }
            let mut result = name_bytes;
            result.extend_from_slice(&punct_bytes);
            match ctx.make_string(&result) {
                Ok(handle) => HostReply::Value(Some(handle)),
                Err(_) => {
                    ctx.fault_host();
                    HostReply::Value(None)
                }
            }
        }))
}

/// Whether `name` is a registered manifest primitive — the `#! requires:` existence check
/// (`matcher`). Loud both ways: a fixture requiring an unregistered name is a fixture error.
pub(crate) fn is_registered(name: &str) -> bool {
    MANIFEST.iter().any(|(n, _, _)| *n == name)
}

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

    #[test]
    fn test_greet_is_a_registered_non_capability_primitive() {
        // The S-42 foreign fn is in the manifest (so a `#! requires: test_greet` fixture runs) but is
        // synchronous, not a suspending capability — it never gets a capability id.
        assert!(is_registered("test_greet"), "test_greet is in the manifest");
        assert_eq!(
            capability_id("test_greet"),
            None,
            "test_greet is synchronous"
        );
        // The manifest builds cleanly with `test_greet` appended (its default is an immutable
        // `ConstValue::Str`, D-M7-8 immutable-by-construction — no mutable-default variant exists).
        let _ = registry();
        assert!(
            !is_registered("no_such_primitive"),
            "unknown name is unregistered"
        );
    }
}
