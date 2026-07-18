//! The Doodle engine: front end, resumable machine, heap/GC, and embedding API.
//!
//! This crate implements the engine specified in the Doodle engine spec
//! (`discussions/spec/engine.md`), realizing the language specified in the
//! Doodle language spec (`discussions/spec/language.md`). See the
//! implementation plan (`discussions/plan/implementation.md`) for the
//! architecture (AD1–AD8) and milestone schedule.
//!
//! The crate currently holds the milestone-M0 **pipeline skeleton**: the
//! module shells the front end and machine will fill in ([`span`], [`diag`],
//! [`ast`], [`machine`], [`drive`]), a [`stage`] entry point reporting which
//! pipeline stages are implemented (none yet), plus a [`drive::run`] that
//! executes a hand-built one-statement program to completion. The parser and
//! the machine core (heap, frames, GC — machine-design §4+) arrive at M1 and
//! M2a.

pub mod ast;
pub mod diag;
pub mod drive;
pub mod lex;
pub mod machine;
pub mod parse;
pub mod resolve;
pub mod source;
pub mod span;
pub mod stage;
pub mod unicode;

pub use lex::lex_to_diagnostics;
pub use parse::parse_to_diagnostics;
pub use resolve::full_to_diagnostics;

/// Returns the version of the doodle-core crate.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Fuzz seam (placeholder): consumes arbitrary bytes without panicking,
/// establishing the `fuzz/` plumbing. The real lexer/parser entry points
/// replace it at M1. Not part of the stable API.
#[doc(hidden)]
pub fn fuzz_smoke(input: &[u8]) {
    let _ = std::str::from_utf8(input);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_nonempty() {
        assert!(!version().is_empty());
    }
}
