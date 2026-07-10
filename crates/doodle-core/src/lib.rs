//! The Doodle engine: front end, resumable machine, heap/GC, and embedding API.
//!
//! This crate implements the engine specified in the Doodle engine spec
//! (`discussions/spec/engine.md`), realizing the language specified in the
//! Doodle language spec (`discussions/spec/language.md`). See the
//! implementation plan (`discussions/plan/implementation.md`) for the
//! architecture (AD1–AD8) and milestone schedule.
//!
//! Currently a placeholder: milestone M0 (scaffolding) is in progress.

/// Returns the version of the doodle-core crate.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_nonempty() {
        assert!(!version().is_empty());
    }
}
