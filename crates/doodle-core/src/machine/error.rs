//! Runtime errors: the exception a failing operation raises and the trace it
//! carries to the drive boundary (engine spec E§9; L§12).
//!
//! **Scope (M2a.3a).** Engine-generated runtime errors (type mismatch, division
//! by zero, a nonfinite float result, a Void-in-expression use) become a
//! [`Raise`] that propagates **uncaught** — there are no handlers yet
//! (`try`/`rescue` is M4) — so the drive returns [`Raised`]. Two things grow
//! later: the §12 **unwind** mechanism (handler search + `with`/block cleanup)
//! replaces the plain propagation at M2a.6, and a Doodle exception is ultimately
//! a **value** (E§9) — the value form `rescue` binds arrives with error records
//! at M4. Until then an exception is this host-facing kind + message, which no
//! Doodle code can yet observe.
//!
//! [`Raised`]: crate::drive::Outcome::Raised

use crate::drive::EngineFault;
use crate::span::Span;

/// The kind of a runtime error (a Doodle exception, E§9).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExceptionKind {
    /// An operation got an operand of the wrong type (e.g. `1 + true`).
    TypeMismatch,
    /// Division or modulo by zero (L§4.2).
    DivisionByZero,
    /// A float operation's result would be nonfinite — ±∞ or NaN (S-56, L§4.2).
    NonFiniteFloat,
    /// Ordering (`<`/`>`/`<=`/`>=`) applied where it is undefined, e.g. a NaN
    /// operand (L§6.6). (Raised from the comparison ops at M2a.3b.)
    UndefinedOrdering,
    /// A procedure result (Void) was used where a value is required (L§6.11).
    ProcedureInExpression,
    /// A name was used that is not defined (no such binding). At M2a this is a
    /// runtime error; the linter's undeclared-read diagnostic is separate (AD5).
    NameNotDefined,
    /// A binding was used before its declaration executed — the temporal dead
    /// zone (cell present but uninitialized).
    UsedBeforeDefined,
    /// A call whose callee is not a callable value (L§6.4/§8).
    NotCallable,
    /// A call's arguments do not match the callee's parameters (L§8.3): a missing
    /// required argument, an unknown keyword, a duplicate binding, or too many
    /// positional arguments.
    ArgumentError,
    /// A non-hashable value (e.g. a list) was used as a dict key (L§4.8). At M4.1
    /// the hashable kinds are the scalars; records join at M4.4.
    UnhashableKey,
    /// A dict was indexed (`d[k]`) with a key it does not contain (L§4.8).
    KeyNotFound,
    /// A list/string/bytes was indexed outside `0 <= k < length` — too large or negative
    /// (L§6.3, S-58): both directions are "no such position", one slug. Completes the
    /// access-miss triad with `KeyNotFound` (dict) and `NoSuchField` (record).
    IndexOutOfRange,
    /// A field access (`r.name`) named a field the record's type does not declare
    /// (L§9). Records are dynamically typed, so this is a runtime error.
    NoSuchField,
    /// A valued `break` exited a block-consuming call whose callee is a **procedure**
    /// (yields no value), so the value has no destination (L§7.10, §8.5). This is
    /// the **open** S-10 to-consumer half; the machine raises **provisionally**
    /// (rather than silently discard the value) pending the user's ruling.
    NoValueDestination,
    /// String repetition (`*`) was given a negative count (L§4.4, S-59): a miscomputed
    /// count is a bug, not a request for `""`, so it raises rather than clamping — an
    /// operand-domain error parallel to `DivisionByZero`.
    NegativeCount,
    /// `decode(bytes)` was given bytes that are not well-formed UTF-8 (L§4.4, S-58): a
    /// **data** error, not a call-shape one. First of the malformed-data family. The
    /// host-side `make_string` reports the same failure as an error return (S-30), not a
    /// raise, since a host call has no drive to raise into.
    InvalidUtf8,
    /// A function (`fn`) reached its completion without producing a value (L§8.4):
    /// it fell off the end. The resolver catches this statically where it can
    /// (`function-falls-off-end`, S-5); this is the runtime backstop for the cases
    /// it cannot — an `fn` whose tail call turns out, at run time, to be a
    /// procedure (the fn-tail-`to` case, S-55).
    FunctionFellOffEnd,
    /// A host `resolve(Raise)` rejected a suspending-capability call (E§7.5): the host
    /// signalled the capability failed. **Provisional (M2b.4):** the host-raised value
    /// is rendered into the message; carrying it as the exception value `rescue` binds
    /// arrives with exceptions-as-values (M4, E§9).
    HostRaised,
    /// An `import` whose path the host resolved `NotFound` (E§6, S-60): the module the
    /// program asked for does not exist. Raised in the importing program at the `import`
    /// statement. The path and importer ride the message for now (per-kind structured
    /// `details: {path, importer}` awaits the message-rubric work, as for every kind).
    ModuleNotFound,
    /// An `import` of a module whose load is already in progress — a **circular import**
    /// (L§11.3, E§6). Raised at the re-entrant `import`, its message naming the cycle
    /// (`a imports b imports a`). Structured `details: {cycle: [paths]}` await the
    /// message-rubric work.
    CircularImport,
    /// A fetched module whose **source has static errors** (a syntax/resolve error in the
    /// host-supplied source) — the runtime face of E's `LoadError` (E§3.2). Raised at the
    /// `import` in the importer, since a broken imported module is the *module author's*
    /// program error, not a host-contract matter. This is the exception the module's
    /// `failed` state retains, so a re-import re-raises it unchanged (S-8). Structured
    /// `details: {path, canonical_id, diagnostics}` — the full list, so an IDE renders an
    /// imported module's errors as it renders the main program's — await the rubric work.
    ModuleLoadError,
    /// A name supplied by **two wildcard imports** was used (L§11.2, S-13): the reference
    /// is ambiguous. Raised at the *use* site, its message naming both source modules; an
    /// explicit or selective import (or a local declaration) of the name overrides the
    /// wildcards and avoids this. **Provisional slug (M5.2c, pending user ratification).**
    AmbiguousImport,
}

impl ExceptionKind {
    /// The stable **kebab-case slug** naming this error class (L§12.1, S-58): the
    /// `kind` field of the `Error` record an engine raise materializes, and the host's
    /// error tag. The catalog is part of the language's stable surface.
    pub fn slug(self) -> &'static str {
        match self {
            ExceptionKind::TypeMismatch => "type-mismatch",
            ExceptionKind::DivisionByZero => "division-by-zero",
            ExceptionKind::NonFiniteFloat => "non-finite-float",
            ExceptionKind::UndefinedOrdering => "undefined-ordering",
            ExceptionKind::ProcedureInExpression => "procedure-in-expression",
            ExceptionKind::NameNotDefined => "name-not-defined",
            ExceptionKind::UsedBeforeDefined => "used-before-defined",
            ExceptionKind::NotCallable => "not-callable",
            ExceptionKind::ArgumentError => "argument-error",
            ExceptionKind::UnhashableKey => "unhashable-key",
            ExceptionKind::KeyNotFound => "key-not-found",
            ExceptionKind::IndexOutOfRange => "index-out-of-range",
            ExceptionKind::NoSuchField => "no-such-field",
            ExceptionKind::NoValueDestination => "no-value-destination",
            ExceptionKind::NegativeCount => "negative-count",
            ExceptionKind::InvalidUtf8 => "invalid-utf8",
            ExceptionKind::FunctionFellOffEnd => "function-fell-off-end",
            ExceptionKind::HostRaised => "host-raised",
            ExceptionKind::ModuleNotFound => "module-not-found",
            ExceptionKind::CircularImport => "circular-import",
            ExceptionKind::ModuleLoadError => "module-load-error",
            ExceptionKind::AmbiguousImport => "ambiguous-import",
        }
    }
}

/// A Doodle exception reaching a drive boundary (E§9).
#[derive(Clone, Debug)]
pub struct Exception {
    /// The error kind.
    pub kind: ExceptionKind,
    /// A human-readable, kid-facing message.
    pub message: String,
}

/// One frame in a raise's captured trace (E§8.2/§9), innermost first: where it was
/// entered and how many tail-call iterations it absorbed (E§8.3).
#[derive(Clone, Copy, Debug)]
pub struct TraceFrame {
    /// The call-site span the frame was entered at, if any (`None` for the module top
    /// level and for a block invoked by a native consumer — host code, no call site).
    pub call_site: Option<Span>,
    /// Tail-iterations absorbed into this frame by proper-tail-call reuse (E§8.3): `0`
    /// for a fresh frame, `n` after `n` tail calls reused the same slot.
    pub tail_count: u64,
}

/// The trace accompanying a raise (E§8.2/§9), captured **at the raise site**, before any
/// unwinding (L§12.1): the raising position, the live call stack, and the bounded
/// tail-elided history. Deterministic (E§11) — a pure function of the machine state.
#[derive(Clone, Debug)]
pub struct Trace {
    /// The source span the raise occurred at, if known.
    pub raised_at: Option<Span>,
    /// The live call stack at the raise (E§8.2), innermost first.
    pub frames: Vec<TraceFrame>,
    /// The bounded tail-elided history at the raise (E§8.3), most-recent first: the decl
    /// span of each callable whose activation a tail call overwrote.
    pub tail_elided: Vec<Span>,
}

impl Trace {
    /// A trace with only its raising position — the live frames and tail-elided history
    /// are captured (`observe::capture_trace`) when the raise enters the unwind channel.
    pub(crate) fn at(raised_at: Option<Span>) -> Self {
        Trace {
            raised_at,
            frames: Vec::new(),
            tail_elided: Vec::new(),
        }
    }
}

/// A raise in flight to the drive boundary: the exception and its trace. Carried
/// as the `Err` of a machine transition; the drive loop turns it into
/// [`Outcome::Raised`](crate::drive::Outcome::Raised).
#[derive(Clone, Debug)]
pub(crate) struct Raise {
    pub(crate) exception: Exception,
    pub(crate) trace: Trace,
}

impl Raise {
    /// Builds a raise of `kind` with `message`, raised at `span`.
    pub(crate) fn new(kind: ExceptionKind, message: impl Into<String>, span: Span) -> Self {
        Raise {
            exception: Exception {
                kind,
                message: message.into(),
            },
            trace: Trace::at(Some(span)),
        }
    }
}

/// Why a machine transition stopped the drive: an uncaught **raise** (a Doodle
/// exception → [`Outcome::Raised`]) or an **engine fault** (a resource limit
/// exceeded, → [`Outcome::Faulted`], E§10.2). `step` returns this as its `Err`;
/// the drive loop maps each arm to its outcome. The `From` impls let a transition
/// return either through `?`. Pause/suspend (slicing, capabilities) join this
/// channel when those land.
///
/// [`Outcome::Raised`]: crate::drive::Outcome::Raised
/// [`Outcome::Faulted`]: crate::drive::Outcome::Faulted
#[derive(Clone, Debug)]
pub(crate) enum Halt {
    /// An uncaught Doodle exception reaching the boundary (E§9): the raised **value**
    /// (an `Error` record, or any value a program `raise`d) and its trace.
    Raise(super::Value, Trace),
    /// An engine fault — a configured limit was exceeded (E§10.2).
    Fault(EngineFault),
}

impl From<EngineFault> for Halt {
    fn from(fault: EngineFault) -> Self {
        Halt::Fault(fault)
    }
}
