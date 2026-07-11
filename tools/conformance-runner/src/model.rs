//! Data model for the conformance runner.

use doodle_core::source::Position;
use doodle_core::stage::Stage;

/// The loading mode a test declares (`#! mode:`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    /// Load only (lex/parse/resolve) and check static errors/warnings.
    Static,
    /// Execute the program under the conformance host.
    Run,
}

/// A single `#! expect-…` directive, retained for matching against real output.
///
/// The static kinds (error/warning) are matched from the first stage (M1.3,
/// lex); the run kinds (out/raise) are parsed and retained now but matched once
/// execution lands (M2a/M2b).
#[derive(Clone, Debug)]
pub(crate) enum Expectation {
    /// `expect-static-error: <substring> @ <pos>` — a load-time error.
    StaticError { substring: String, pos: Position },
    /// `expect-warning: <substring> @ <pos>` — a load-time warning.
    Warning { substring: String, pos: Position },
    /// `expect-out: <text>` — one printed line (run mode).
    Out { text: String },
    /// `expect-raise: <substring> @ <pos>` — an uncaught error (run mode).
    Raise { substring: String, pos: Position },
}

/// A discovered, parsed conformance test.
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
    /// The declared `#! expect-…` directives, in file order.
    pub(crate) expectations: Vec<Expectation>,
}
