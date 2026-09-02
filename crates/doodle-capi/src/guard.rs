//! The panic firewall for the C ABI boundary (freeze convention 5). Rust's default
//! `panic = "unwind"` makes a panic escaping an `extern "C"` function undefined behavior,
//! and the drive path is dense with `debug_assert!`/`unreachable!`/`.expect()` a
//! mis-driving host can reach. Every entry point runs its body through [`catch`], so a
//! caught panic becomes a defined [`DoodleStatus::ErrPanic`] instead of crossing the FFI.

use crate::abi::DoodleStatus;
use std::panic::{AssertUnwindSafe, catch_unwind};

/// Runs `body` catching any panic, returning [`DoodleStatus::ErrPanic`] if it unwinds.
///
/// `AssertUnwindSafe` is sound here because a caught panic ends the call at the boundary:
/// the instance is left as the panicking transition left it and the host receives an
/// error, so no code observes a logically-torn value across the catch. (A panic still
/// means an engine bug — `ErrPanic` is a last-resort firewall, not a normal path.)
pub(crate) fn catch(body: impl FnOnce() -> DoodleStatus) -> DoodleStatus {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or(DoodleStatus::ErrPanic)
}
