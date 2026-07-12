//! Pipeline stages, and which of them doodle-core currently implements.
//!
//! The conformance runner (`tools/conformance-runner`) asks doodle-core, per
//! test, whether the stage a test requires is implemented yet; a test whose
//! required stage is unimplemented is skipped rather than failed. The gate
//! lifts stage by stage across M1 (lex/parse/full) and M2 (run). As of M1.7
//! the lexer and parser are implemented, so `stage: lex` and `stage: parse`
//! tests execute; full analysis and run still skip.

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

/// The highest [`Stage`] doodle-core currently implements, or `None` before any
/// stage exists. As of M1.7 this is `Some(Stage::Parse)`.
///
/// A conformance test requiring stage `s` is executable iff this returns
/// `Some(impl)` with `impl >= s`; otherwise the runner skips the test.
pub fn implemented_through() -> Option<Stage> {
    // M1.7: the lexer (`crate::lex`) and parser (`crate::parse`) are
    // implemented; full static analysis and run are not. Bumps here must land
    // with the corresponding conformance-runner executor
    // (`tools/conformance-runner`) atomically.
    Some(Stage::Parse)
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
    fn implemented_through_parse_at_m1_7() {
        // The parser is the highest implemented stage; full and run are not.
        assert_eq!(implemented_through(), Some(Stage::Parse));
    }
}
