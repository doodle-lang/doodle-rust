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
    /// `**` with an exponent that overflows `u32` — the guard that keeps a single
    /// `Int ** Int` transition's bignum finite (S-12 resource half). The M2a.9
    /// heap/step limits now bound `**` deterministically (it faults at a
    /// deterministic step); because that check is post-allocation (at a safe point),
    /// this guard also caps the one-transition overshoot. A tight pre-allocation
    /// size estimate is a tracked refinement (claude-todo).
    ExponentTooLarge,
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
    /// A valued `break` exited a block-consuming call whose callee is a **procedure**
    /// (yields no value), so the value has no destination (L§7.10, §8.5). This is
    /// the **open** S-10 to-consumer half; the machine raises **provisionally**
    /// (rather than silently discard the value) pending the user's ruling.
    NoValueDestination,
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
    /// An uncaught Doodle exception (no handler; `try`/`rescue` is M4).
    Raise(Raise),
    /// An engine fault — a configured limit was exceeded (E§10.2).
    Fault(EngineFault),
}

impl From<Raise> for Halt {
    fn from(raise: Raise) -> Self {
        Halt::Raise(raise)
    }
}

impl From<EngineFault> for Halt {
    fn from(fault: EngineFault) -> Self {
        Halt::Fault(fault)
    }
}
