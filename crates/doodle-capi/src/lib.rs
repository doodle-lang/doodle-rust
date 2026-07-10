//! The Doodle engine's C ABI (implementation-plan AD1: the one crate permitted
//! to use `unsafe`).
//!
//! M0 hello-world: a single `doodle_version()` returning the engine version as
//! a NUL-terminated C string, establishing the C ABI, the cbindgen-generated
//! header, and the C smoke test. The full embedding API (engine spec E) is
//! built out from M2b.

use std::ffi::{CString, c_char};
use std::sync::OnceLock;

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
