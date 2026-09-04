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
    /// Drive the program under a **script** of directives (E§8, S-22; drive-script format,
    /// implementation-plan §4.3), asserting the resulting outcome/position/stack **transcript**.
    Drive,
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

/// A scripted primitive value used to resolve a suspending capability (D-M7-21): the literal a
/// fixture writes in a `resolve:` step or an `input:` queue entry. Covers what the conformance
/// capabilities produce — `read_line` a string, `time`/`random` a number — plus `bool`/`nil` for
/// completeness. Materialized into a host handle at resolution (`matcher`/`drive`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ScriptValue {
    /// A `"…"` string literal.
    Str(String),
    /// An integer literal.
    Int(i64),
    /// A float literal (has a `.`).
    Float(f64),
    /// `true` / `false`.
    Bool(bool),
    /// `nil`.
    Nil,
}

/// How a scripted `input:`/`resolve:` fulfils a capability request (E§7.5): a value the capability
/// produces, or an exception raised at its call site.
#[derive(Clone, Debug)]
pub(crate) enum ScriptResponse {
    /// Resolve with this value (`Resolution::Value`).
    Value(ScriptValue),
    /// Raise this message at the call site (`Resolution::Raise`).
    Raise(String),
}

/// One `#! input: <capability> -> <response>` entry (run mode): the host's scripted answer to the
/// next request for that capability. Entries for one capability form an in-order FIFO queue.
#[derive(Clone, Debug)]
pub(crate) struct ScriptInput {
    /// The capability name the response is for (e.g. `read_line`).
    pub(crate) capability: String,
    /// The value/raise to answer its next request with.
    pub(crate) response: ScriptResponse,
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
    /// The declared `#! expect-…` directives, in file order (`run`/`static` modes).
    pub(crate) expectations: Vec<Expectation>,
    /// The scripted capability responses (`#! input:`, `run` mode): per capability, an in-order
    /// FIFO the runner draws from when that capability suspends (D-M7-21).
    pub(crate) inputs: Vec<ScriptInput>,
    /// The drive script (`mode: drive` only): setup + the ordered directive/expectation steps.
    pub(crate) drive: Option<DriveScript>,
}

/// A drive-script test (implementation-plan §4.3): debug **setup** applied once, then an
/// **ordered** sequence of drive [`DriveStep`]s whose actual outcome/position/stack transcript is
/// compared whole against the declared one — the cross-surface determinism evidence of E§8.
#[derive(Clone, Debug)]
pub(crate) struct DriveScript {
    /// Breakpoints to set before driving, as `(canonical id, 1-based line)` (E§8.6).
    pub(crate) breakpoints: Vec<(String, u32)>,
    /// Whether to enable raise-trapping (E§8.7).
    pub(crate) raise_trap: bool,
    /// Whether to run in per-subexpression observation mode (E§8.8, S-62); else per-statement.
    pub(crate) subexpr: bool,
    /// The ordered steps: each drives one action and asserts the resulting stop.
    pub(crate) steps: Vec<DriveStep>,
}

/// One step of a drive script: an action to perform and the stop it must produce.
#[derive(Clone, Debug)]
pub(crate) struct DriveStep {
    /// The action driven this step (`#! do:`).
    pub(crate) action: DriveAction,
    /// The stop this step must produce (`#! expect:`).
    pub(crate) expect: StopAssertion,
    /// The optional stack shape at the stop (`#! stack:`), innermost frame first.
    pub(crate) stack: Option<Vec<StackElem>>,
}

/// A drive-script action (`#! do:`, `#! resolve:`, `#! resolve-raise:`) — a driving directive
/// (E§7.3). Most drive the machine forward; `Resolve`/`ResolveRaise` fulfil the capability the
/// previous step suspended on (D-M7-21), resuming via the engine's `resolve(Resolution)`. (Imports
/// resolve transparently via sibling files unless a step expects an `import` stop.)
#[derive(Clone, Debug)]
pub(crate) enum DriveAction {
    /// `run` — `RunToCompletion`.
    Run,
    /// `continue` — `Continue`.
    Continue,
    /// `step` — `Step`.
    Step,
    /// `into` — `StepInto`.
    Into,
    /// `over` — `StepOver`.
    Over,
    /// `out` — `StepOut`.
    Out,
    /// `resolve: <value>` — resolve the suspended capability with a value (`Resolution::Value`).
    Resolve(ScriptValue),
    /// `resolve-raise: <msg>` — raise `msg` at the suspended capability's call site
    /// (`Resolution::Raise`).
    ResolveRaise(String),
}

/// The stop a [`DriveStep`] must produce (`#! expect:`) — one entry of the outcome transcript.
#[derive(Clone, Debug)]
pub(crate) enum StopAssertion {
    /// `completed` — the driven unit finished (a module top level completes with no value).
    Completed,
    /// `paused <reason> @ <pos>` — a `Paused` stop with this reason at this position.
    Paused { reason: String, pos: Position },
    /// `raised <substring> @ <pos>` — an uncaught raise whose message contains `substring`.
    Raised { substring: String, pos: Position },
    /// `suspended <capability> @ <pos>` — a **capability** request (`Outcome::Suspended`) for the
    /// named capability, at this position (D-M7-21).
    Suspended { capability: String, pos: Position },
    /// `import <path> @ <pos>` — an **import** suspension (`Outcome::SuspendedImport`) for this
    /// dotted module path, at this position (D-M7-21). Asserting it suppresses the transparent
    /// sibling-file resolution for that step.
    Import { path: String, pos: Position },
    /// `faulted <kind>` — a non-resumable engine fault of this kind.
    Faulted { kind: String },
}

/// One element of an asserted stack shape (`#! stack:`), a call frame: its call-site `line`, and
/// optionally the callee `name` and a tail-iteration count `tail` (E§8.2/§8.3). The matcher checks
/// whatever is given — bare `L`, `name@L`, or `name@L×N` — so a fixture asserts only what it means.
#[derive(Clone, Debug)]
pub(crate) struct StackElem {
    /// The callee name, if the element pins it (`name@L`).
    pub(crate) name: Option<String>,
    /// The 1-based call-site line (always asserted).
    pub(crate) line: u32,
    /// The tail-iteration count, if the element pins it (`…×N`).
    pub(crate) tail: Option<u64>,
}
