//! The Doodle engine's WebAssembly facade, built with wasm-bindgen.
//!
//! M0 hello-world: exports a single [`version`] that returns doodle-core's
//! version, establishing the wasm toolchain and the size budget (implementation
//! plan §6.5). The full JS facade (the embedding API surface) lands at M3.

use wasm_bindgen::prelude::*;

/// Returns the version of the underlying doodle-core engine.
#[wasm_bindgen]
pub fn version() -> String {
    doodle_core::version().to_string()
}
