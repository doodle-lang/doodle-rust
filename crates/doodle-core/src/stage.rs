//! Pipeline stages, and which of them doodle-core currently implements.
//!
//! The conformance runner (`tools/conformance-runner`) asks doodle-core, per
//! test, whether the stage a test requires is implemented yet; a test whose
//! required stage is unimplemented is skipped rather than failed. The gate
//! lifts stage by stage across M1 (lex/parse/full) and M2 (run). As of M2a.12 the
//! machine runs the demo subset, so `mode: run` tests execute too — the runner
//! drives the program and matches `expect-raise`. A run test whose transcript
//! needs a not-yet-registered capability (any `expect-out`, which needs `print`)
//! still skips until M2b, keyed on the test's expectations, not this stage scalar.

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
/// stage exists. As of M2a.12 this is `Some(Stage::Run)`.
///
/// A conformance test requiring stage `s` is executable iff this returns
/// `Some(impl)` with `impl >= s`; otherwise the runner skips the test. (A run test
/// may still skip for a *capability* it needs — e.g. `print` — which the runner
/// checks from the test's expectations, not from this scalar.)
pub fn implemented_through() -> Option<Stage> {
    // M2a.12: the machine (`crate::machine`, `crate::drive`) runs the demo subset,
    // so run mode joins the front end. Bumps here must land with the corresponding
    // conformance-runner executor (`tools/conformance-runner`) atomically.
    Some(Stage::Run)
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
    fn implemented_through_run_at_m2a_12() {
        // The machine runs the demo subset, so run is the highest implemented
        // stage (bumped atomically with the conformance-runner run executor).
        assert_eq!(implemented_through(), Some(Stage::Run));
    }
}
