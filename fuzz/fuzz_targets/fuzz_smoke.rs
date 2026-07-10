#![no_main]

use libfuzzer_sys::fuzz_target;

// Placeholder target: exercises the fuzz plumbing over arbitrary bytes. Real
// lexer/parser targets that drive doodle-core's front end arrive at M1.
fuzz_target!(|data: &[u8]| {
    doodle_core::fuzz_smoke(data);
});
