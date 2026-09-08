//! The Doodle engine's C ABI (implementation-plan AD1: the one crate permitted
//! to use `unsafe`). It marshals the `doodle-core` embedding API (engine spec E)
//! across a stable, cbindgen-generated `doodle.h`.
//!
//! # Freeze conventions (D-M7-3)
//!
//! The committed header is a **binary-compatibility promise**; these conventions
//! keep it stable *and* additively extensible, and every new symbol in M7.2/M7.3
//! must follow them:
//!
//! 1. **Dedicated `#[repr(C)]` ABI types, owned here.** The core's enums/structs are
//!    default-repr (no guaranteed layout); the ABI mirrors each as its own
//!    `#[repr(C)]`/`#[repr(u32)]` type ([`abi`]) with **explicit** discriminants,
//!    hand-mapped from the core. cbindgen parses only this crate, so the header can
//!    never leak an internal representation.
//! 2. **Every host-facing struct grows without breaking.** Config and descriptor
//!    surfaces are **opaque + builder functions** (a new setter is additive); the
//!    fixed-layout [`abi::DoodleOutcome`] carries a **reserved tail** for a new field.
//!    A new outcome *kind* fits because the `kind`/tag enums cross as a **fixed-width
//!    integer**: a value an older host does not recognize arrives as an unrecognized number
//!    it handles via its `switch` default — never UB, and never a resize. (There is **no**
//!    reserved sentinel *enumerator*; the fixed underlying width is the whole guarantee, so a
//!    host must always have a default arm.)
//!    **Every** by-value struct carries a `reserved` tail — **no exceptions**, even one
//!    argued structurally complete (e.g. [`abi::DoodlePosition`], which is deliberately
//!    just (module, span) and whose tail is expected to stay zero forever): a tailless
//!    exception is perpetual re-litigation, and a by-value struct embedded in another
//!    (Position in [`abi::DoodleFrame`]) would cascade a wrong freeze. The reserved
//!    convention is uniform: the **producer writes zero, the consumer ignores** the tail,
//!    and a slot is repurposable **only at a minor ABI-version bump** ([`doodle_abi_version`]
//!    gates it) — so the growth mechanism is actually usable, not just present.
//! 3. **Names are prefixed.** Types are `Doodle*`, functions `doodle_*`, and enum
//!    constants are name-prefixed (`DoodleStatus_Ok`), so nothing pollutes the
//!    global C namespace.
//! 4. **Strings cross by copy** ([`value`], D-M7-6): a reader copies UTF-8 bytes
//!    into a caller buffer and reports the needed length; the host owns the buffer,
//!    so there is no interior-pointer validity window to get wrong.
//! 5. **Fallible calls return a [`abi::DoodleStatus`]** and write results through
//!    out-params; nothing panics across the boundary — every entry point is wrapped
//!    in [`guard::catch`] so a Rust panic becomes `DoodleStatus_ErrPanic`, never UB
//!    (the drive path is dense with `debug_assert!`/`unreachable!`).
//! 6. **Platform-neutral** (D-M7-9): no compiler-specific attributes, and no
//!    platform-varying sizes for the values the ABI *defines* — certification is
//!    Linux-only for M7, but the surface is portable. Concretely: **element counts and
//!    indices cross as fixed-width `u32`** (the engine's heap is `u32`-indexed, so this
//!    is the real width, not a cap — `doodle_list_length`/`_get` and their `call_*`
//!    peers all use `u32`); **raw byte-buffer sizes cross as `size_t`** (the copy-out
//!    `cap`/`out_len` of convention 4 — a memory quantity, correctly platform-width).
//!    A `uintptr_t` for an *opaque host pointer* a foreign value round-trips is not an
//!    ABI-defined size and is exempt.
//! 7. **Host-supplied discriminants are validated, never trusted** (R3). A parameter carrying an
//!    enum *from* the host crosses as a plain `uint32_t` and is range-checked to a known variant,
//!    returning [`abi::DoodleStatus::ErrContract`] (or the entry's failure signal) on an unknown
//!    value — never constructed directly as a Rust `#[repr(u32)]` enum, which would be instant UB
//!    for an out-of-range value the panic firewall cannot catch. This mirrors the fixed-width
//!    tag on the engine→host side (convention 2): neither side trusts the other's discriminants,
//!    so a minor-version value reaching an older peer is defined, not UB. Every future
//!    host-supplied enum parameter follows this — do not take a typed `Doodle*` enum by value.
//!
//! The ABI contract version ([`doodle_abi_version`]) is distinct from the engine
//! version ([`doodle_version`]): major = breaking, minor = additive.
//!
//! # Handles & instances (handle-ownership contract)
//!
//! A value crossing to the host is a `uint64_t` **handle** into *one* instance's heap. The host
//! owns it and must [`value::doodle_release`] it exactly as many times as it was obtained, and a
//! handle is only valid on the instance that minted it. A stale or forged handle is caught at the
//! boundary as [`abi::DoodleStatus::ErrStaleHandle`]. Using a handle on the **wrong instance** is
//! a host bug: in debug builds the engine detects it (a per-instance id stamped into the handle
//! bits, MD §16) and reports [`abi::DoodleStatus::ErrContract`], distinct from a stale handle.
//!
//! Release builds do not encode that id and cannot detect the cross-instance case — but it is
//! never memory-unsafe. The handle's index+generation are always validated against the
//! *resolving* instance's own table, so the worst case a foreign handle can produce is a
//! wrong-but-live value from that instance (or a boundary error), never undefined behavior. The
//! Rust-side memory safety of the boundary (AD1) holds in every build; only the *diagnosis* of a
//! cross-instance mixup is debug-only.
//!
//! # Certification hooks (NOT part of the ABI)
//!
//! For determinism certification (D-M7-11), `doodle_load` can honor one environment variable:
//! **`DOODLE_GC_STRESS`**: when set to any non-empty value, every instance it creates collects at
//! **every** safe point, so a test harness can drive a corpus under maximal GC pressure and confirm
//! the trace is byte-identical to an un-stressed run (GC timing is unobservable, E§11). This is a
//! **test/CI hook, deliberately not a `doodle.h` symbol**: it is read once, in this host layer, and
//! latched before the first drive (the engine never reads the environment, so the E§11 determinism
//! boundary stays exact).
//!
//! **The env read is behind the off-by-default `gc-stress` cargo feature.** A shipped library must
//! not silently vary with an ambient variable, so a default build ignores `DOODLE_GC_STRESS`
//! entirely; only a build with `--features gc-stress` reads it (the gate,
//! `scripts/gc-stress-conformance.sh`, builds `--release --features gc-stress`). An embedder who
//! wants to run the determinism gate against their own build opts in the same way. The explicit
//! `Instance::enable_gc_stress` path (doodle-core) is unaffected by the feature.

use std::ffi::{CString, c_char};
use std::sync::OnceLock;

pub mod abi;
pub mod call;
pub mod call_read;
pub mod call_value;
pub mod config;
pub mod control;
pub mod desc;
pub mod guard;
pub mod inspect;
pub mod instance;
pub mod observe;
pub mod registry;
pub mod value;

/// The C ABI contract version's major component (a breaking change bumps it). Mirrors
/// the `DOODLE_ABI_VERSION_MAJOR` header macro; [`doodle_abi_version`] packs both.
const ABI_VERSION_MAJOR: u32 = 0;
/// The C ABI contract version's minor component (an additive change bumps it).
const ABI_VERSION_MINOR: u32 = 1;

/// Returns the Doodle engine version as a NUL-terminated C string.
///
/// The returned pointer is valid for the lifetime of the program and must not
/// be freed by the caller.
#[unsafe(no_mangle)]
pub extern "C" fn doodle_version() -> *const c_char {
    static VERSION: OnceLock<CString> = OnceLock::new();
    VERSION
        .get_or_init(|| {
            CString::new(doodle_core::version()).expect("engine version contains no NUL byte")
        })
        .as_ptr()
}

/// Returns the C ABI contract version as `(major << 16) | minor` — distinct from the
/// engine version ([`doodle_version`]). A host compiled against `doodle.h` compares this
/// against the header's `DOODLE_ABI_VERSION_MAJOR`/`_MINOR` at load: a differing **major**
/// is incompatible; a newer **minor** is backward-compatible (additive only).
#[unsafe(no_mangle)]
pub extern "C" fn doodle_abi_version() -> u32 {
    (ABI_VERSION_MAJOR << 16) | ABI_VERSION_MINOR
}
