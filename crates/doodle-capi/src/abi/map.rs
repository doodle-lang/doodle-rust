//! Conversions between the `#[repr(C)]` ABI mirror types (defined in the parent [`abi`](super))
//! and the `doodle-core` enums they front. Split out so [`abi`](super) holds the frozen type
//! definitions and this holds the (append-only) mapping logic; every function is re-exported from
//! `abi` (`abi::value_error`, `abi::fault`, …), so call sites spell them `abi::…` unchanged.

use super::{
    DoodleBlockOutcome, DoodleBodyKind, DoodleDirective, DoodleFault, DoodleKind,
    DoodleObservationMode, DoodlePauseReason, DoodlePosition, DoodleSeverity, DoodleStatus,
};
use doodle_core::diag::Severity;
use doodle_core::drive::{Directive, EngineFault, LimitKind, ObservationMode, PauseReason};
use doodle_core::machine::{BlockOutcome, HandleError, Kind, Position, ValueError};
use doodle_core::resolve::BodyKind;

/// Maps a core [`Severity`] to its ABI mirror.
pub(crate) fn severity(s: Severity) -> DoodleSeverity {
    match s {
        Severity::Error => DoodleSeverity::Error,
        Severity::Warning => DoodleSeverity::Warning,
    }
}

/// Maps a [`DoodleObservationMode`] to the core [`ObservationMode`].
pub(crate) fn observation_mode(mode: DoodleObservationMode) -> ObservationMode {
    match mode {
        DoodleObservationMode::Statement => ObservationMode::Statement,
        DoodleObservationMode::Subexpression => ObservationMode::Subexpression,
    }
}

/// Maps a core [`ObservationMode`] to its ABI mirror.
pub(crate) fn observation_mode_of(mode: ObservationMode) -> DoodleObservationMode {
    match mode {
        ObservationMode::Statement => DoodleObservationMode::Statement,
        ObservationMode::Subexpression => DoodleObservationMode::Subexpression,
    }
}

/// Maps a core [`Position`] to its ABI mirror (the module id becomes the opaque token).
pub(crate) fn position(pos: Position) -> DoodlePosition {
    DoodlePosition {
        span_start: pos.span.start,
        span_end: pos.span.end,
        module: pos.module.0,
    }
}

/// Maps a core [`Kind`] to its ABI mirror.
pub(crate) fn kind(k: Kind) -> DoodleKind {
    match k {
        Kind::Nil => DoodleKind::Nil,
        Kind::Bool => DoodleKind::Bool,
        Kind::Int => DoodleKind::Int,
        Kind::Float => DoodleKind::Float,
        Kind::String => DoodleKind::String,
        Kind::Bytes => DoodleKind::Bytes,
        Kind::List => DoodleKind::List,
        Kind::Dict => DoodleKind::Dict,
        Kind::Record => DoodleKind::Record,
        Kind::Callable => DoodleKind::Callable,
        Kind::Module => DoodleKind::Module,
        Kind::Type => DoodleKind::Type,
        Kind::Foreign => DoodleKind::Foreign,
    }
}

/// Maps a [`DoodleDirective`] to the core [`Directive`].
pub(crate) fn directive(d: DoodleDirective) -> Directive {
    match d {
        DoodleDirective::RunToCompletion => Directive::RunToCompletion,
        DoodleDirective::Continue => Directive::Continue,
        DoodleDirective::Step => Directive::Step,
        DoodleDirective::StepInto => Directive::StepInto,
        DoodleDirective::StepOver => Directive::StepOver,
        DoodleDirective::StepOut => Directive::StepOut,
    }
}

/// Maps a core [`PauseReason`] to its ABI mirror, returning the breakpoint id alongside
/// (`0` when the pause is not a breakpoint).
pub(crate) fn pause_reason(reason: PauseReason) -> (DoodlePauseReason, u32) {
    match reason {
        PauseReason::Step => (DoodlePauseReason::Step, 0),
        PauseReason::Breakpoint(id) => (DoodlePauseReason::Breakpoint, id.0),
        PauseReason::RaiseTrap => (DoodlePauseReason::RaiseTrap, 0),
        PauseReason::HostPause => (DoodlePauseReason::HostPause, 0),
        PauseReason::SliceEnd => (DoodlePauseReason::SliceEnd, 0),
    }
}

/// Maps a core [`EngineFault`] to its flattened ABI [`DoodleFault`].
pub(crate) fn fault(fault: EngineFault) -> DoodleFault {
    match fault {
        EngineFault::LimitExceeded(LimitKind::StepBudget) => DoodleFault::LimitStepBudget,
        EngineFault::LimitExceeded(LimitKind::Heap) => DoodleFault::LimitHeap,
        EngineFault::LimitExceeded(LimitKind::StackDepth) => DoodleFault::LimitStackDepth,
        EngineFault::LimitExceeded(LimitKind::TailHistory) => DoodleFault::LimitTailHistory,
        EngineFault::LimitExceeded(LimitKind::OpResult) => DoodleFault::LimitOpResult,
        EngineFault::Cancelled => DoodleFault::Cancelled,
        EngineFault::NestedSuspend => DoodleFault::NestedSuspend,
        EngineFault::Internal => DoodleFault::Internal,
    }
}

/// Maps a [`DoodleBodyKind`] to the core [`BodyKind`].
pub(crate) fn body_kind(k: DoodleBodyKind) -> BodyKind {
    match k {
        DoodleBodyKind::Proc => BodyKind::Proc,
        DoodleBodyKind::Func => BodyKind::Func,
    }
}

/// Maps a core [`BlockOutcome`] to its ABI mirror.
pub(crate) fn block_outcome(outcome: BlockOutcome) -> DoodleBlockOutcome {
    match outcome {
        BlockOutcome::Completed => DoodleBlockOutcome::Completed,
        BlockOutcome::NonLocalExit => DoodleBlockOutcome::NonLocalExit,
        BlockOutcome::Halted => DoodleBlockOutcome::Halted,
    }
}

/// Maps a boundary [`ValueError`] to the ABI status a reader/constructor returns.
pub(crate) fn value_error(err: ValueError) -> DoodleStatus {
    match err {
        ValueError::Stale => DoodleStatus::ErrStaleHandle,
        // A cross-instance handle (debug builds, MD §16) is a host bug, not a routine stale
        // handle: report a contract violation so the host asserts rather than retries.
        ValueError::ForeignInstance => DoodleStatus::ErrContract,
        ValueError::WrongKind { .. } => DoodleStatus::ErrWrongKind,
        ValueError::IntOutOfRange => DoodleStatus::ErrIntOutOfRange,
        ValueError::IndexOutOfBounds => DoodleStatus::ErrIndexOutOfBounds,
        ValueError::InvalidUtf8 { .. } => DoodleStatus::ErrInvalidUtf8,
        ValueError::MalformedInt => DoodleStatus::ErrMalformedInt,
    }
}

/// Maps a raw [`HandleError`] to the ABI status a handle op (e.g. `doodle_release`) returns —
/// the [`value_error`] counterpart for the paths that surface a `HandleError` directly. Keeps
/// a cross-instance handle distinct from a stale one (see [`value_error`]).
pub(crate) fn handle_error(err: HandleError) -> DoodleStatus {
    match err {
        HandleError::Stale => DoodleStatus::ErrStaleHandle,
        HandleError::ForeignInstance => DoodleStatus::ErrContract,
    }
}
