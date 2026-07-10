//! Data model for the conformance runner.

use doodle_core::stage::Stage;

/// The loading mode a test declares (`#! mode:`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    /// Load only (lex/parse/resolve) and check static errors/warnings.
    Static,
    /// Execute the program under the conformance host.
    Run,
}

/// A discovered, parsed conformance test.
///
/// M0 records only what the SKIP policy and coverage summary need. The parsed
/// expectations are syntax-checked and counted but not retained; the stored
/// `Expectation` model that M1 matches against real output lands with the
/// first executable stage.
#[derive(Clone, Debug)]
pub(crate) struct Test {
    /// Canonical test id, `<primary-clause>-<topic>-<seq>`.
    pub(crate) id: String,
    /// Declared clauses (`#! clause:`); the first is primary.
    pub(crate) clauses: Vec<String>,
    /// Declared mode.
    pub(crate) mode: Mode,
    /// The pipeline stage this test requires (mode + `#! stage:` resolved).
    pub(crate) required: Stage,
    /// Number of `#! expect-...` directives declared (matched from M1).
    pub(crate) expectation_count: usize,
}
