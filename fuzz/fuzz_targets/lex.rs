#![no_main]

//! Fuzz the lexer (M1.14): over arbitrary bytes, no valid-UTF-8 input — after
//! load normalization, as the host feeds it — may panic, hang, or OOM.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let src = doodle_core::source::normalize(s);
        let _ = doodle_core::lex_to_diagnostics(src.as_ref());
    }
});
