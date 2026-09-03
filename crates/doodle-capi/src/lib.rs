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
//!    fixed-layout [`abi::DoodleOutcome`] carries a **reserved tail** and an
//!    unknown-tag value so a new outcome kind or field fits without a resize.
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
//! 6. **Platform-neutral** (D-M7-9): no compiler-specific attributes, no
//!    platform-varying sizes in the header — certification is Linux-only for M7, but
//!    the surface is portable.
//!
//! The ABI contract version ([`doodle_abi_version`]) is distinct from the engine
//! version ([`doodle_version`]): major = breaking, minor = additive.

use std::ffi::{CString, c_char};
use std::sync::OnceLock;

pub mod abi;
pub mod call;
pub mod call_read;
pub mod call_value;
pub mod config;
pub mod desc;
pub mod guard;
pub mod instance;
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
