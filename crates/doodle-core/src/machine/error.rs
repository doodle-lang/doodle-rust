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
    /// `**` with an exponent too large to compute — the S-12 resource half,
    /// provisional until the M2a.9 heap/step limits bound it deterministically.
    ExponentTooLarge,
}

/// A Doodle exception reaching a drive boundary (E§9).
#[derive(Clone, Debug)]
pub struct Exception {
    /// The error kind.
    pub kind: ExceptionKind,
    /// A human-readable, kid-facing message.
    pub message: String,
}

/// The trace accompanying a raise (E§8.2/§9), captured at the raise site.
///
/// M2a.3a records the raising **position** only; the live-frame list and the
/// bounded tail-elided history (E§8.3) join with the call stack and unwinder
/// (M2a.5/M2a.6/M6).
#[derive(Clone, Debug)]
pub struct Trace {
    /// The source span the raise occurred at, if known.
    pub raised_at: Option<Span>,
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
            trace: Trace {
                raised_at: Some(span),
            },
        }
    }
}
