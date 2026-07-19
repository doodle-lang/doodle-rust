#![no_main]

//! Fuzz the whole static front end (M1.14): lex + parse + resolve over arbitrary
//! text. The resolver landed at M1.10/M1.11, so this catches resolver panics too.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let src = doodle_core::source::normalize(s);
        let _ = doodle_core::full_to_diagnostics(src.as_ref());
    }
});
