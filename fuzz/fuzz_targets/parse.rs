#![no_main]

//! Fuzz the parser (M1.14): lex + parse over arbitrary text; no input may panic,
//! hang, or OOM, and it must always terminate with a `Vec<Diagnostic>`.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let src = doodle_core::source::normalize(s);
        let _ = doodle_core::parse_to_diagnostics(src.as_ref());
    }
});
