//! Pipeline stages, and which of them doodle-core currently implements.
//!
//! The conformance runner (`tools/conformance-runner`) asks doodle-core, per
//! test, whether the stage a test requires is implemented yet; a test whose
//! required stage is unimplemented is skipped rather than failed. At milestone
//! M0 the pipeline implements no stages, so every conformance test skips and
//! the suite is green from day one (plan M0.4 pass policy). The gate lifts
//! stage by stage across M1 (lex/parse/full) and M2 (run).

/// A front-end / execution stage a conformance test may require, ordered least
/// to most: lexing < parsing < full static analysis < running.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum Stage {
    /// Tokenize the source; only lexical errors are observable.
    Lex,
    /// Lex and parse to an AST; syntax errors are observable.
    Parse,
    /// Lex, parse, and resolve — full static analysis (static errors/warnings).
    Full,
    /// Load and execute the program under a host (run mode).
    Run,
}

/// The highest [`Stage`] doodle-core currently implements, or `None` when the
/// pipeline implements no stages yet (milestone M0).
///
/// A conformance test requiring stage `s` is executable iff this returns
/// `Some(impl)` with `impl >= s`; otherwise the runner skips the test.
pub fn implemented_through() -> Option<Stage> {
    // M0: the front end and machine are shells (see `crate::drive`), so no
    // stage is executable yet.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_order_from_lex_to_run() {
        assert!(Stage::Lex < Stage::Parse);
        assert!(Stage::Parse < Stage::Full);
        assert!(Stage::Full < Stage::Run);
    }

    #[test]
    fn m0_implements_no_stages() {
        assert_eq!(implemented_through(), None);
    }
}
