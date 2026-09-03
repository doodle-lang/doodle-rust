//! The `#[repr(C)]` ABI types (freeze convention 1) and their mappings from the
//! `doodle-core` enums. Each type here is the C-visible mirror of a core type whose
//! default Rust layout is not ABI-stable; the explicit discriminants are the frozen
//! contract, so **only append** variants (never renumber) and put new struct fields in
//! [`DoodleOutcome::reserved`].

use doodle_core::drive::{Directive, EngineFault, LimitKind, PauseReason};
use doodle_core::machine::{BlockOutcome, Kind, Position, ValueError};
use doodle_core::resolve::{BodyKind, GlobalKind};

/// An opaque, per-instance value handle (E§4.2), crossing the ABI as a plain `uint64_t`.
/// Round-trips the engine's internal `Handle` bits; the host treats it as opaque and must
/// [`doodle_release`](crate::value::doodle_release) handles it no longer needs. `0` is the
/// reserved null handle (no value), never a live handle.
pub type DoodleHandle = u64;

/// The null handle: the absence of a value (a `to`/module result, an empty outcome slot).
pub const DOODLE_NULL_HANDLE: DoodleHandle = 0;

/// A foreign value's finalizer (E§4.5): run **exactly once** when the value dies (a GC that
/// reclaims it, or `doodle_free`), given only the value's opaque `ptr` — never the instance,
/// so it structurally cannot re-enter the engine (hence its timing never affects any result or
/// determinism, §11). It **must not** unwind across the FFI boundary. `ptr` is the same
/// `uint64_t` passed to `doodle_make_foreign` (a host casts its own pointer to/from it).
pub type DoodleFinalizer = extern "C" fn(ptr: u64);

/// The result of a fallible C-ABI call: `Ok` on success, else the reason. Fallible calls
/// return this and write their result through an out-parameter (freeze convention 5).
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DoodleStatus {
    /// The call succeeded; any out-parameters are written.
    Ok = 0,
    /// A handle named a freed/reused slot — a use-after-release, forged, or (M7.6) a
    /// cross-instance handle (mirrors `ValueError::Stale`).
    ErrStaleHandle = 1,
    /// A typed reader was applied to a value of a different kind (e.g. read an int from a
    /// string). The value's actual kind is available via `doodle_kind_of`.
    ErrWrongKind = 2,
    /// `doodle_as_int` on an integer whose magnitude exceeds `int64_t` (a bignum — read it
    /// with `doodle_as_int_decimal`).
    ErrIntOutOfRange = 3,
    /// A list/string index was past the end.
    ErrIndexOutOfBounds = 4,
    /// `doodle_make_string` was given bytes that are not well-formed UTF-8.
    ErrInvalidUtf8 = 5,
    /// `doodle_make_int_decimal` was given text that is not a base-10 integer literal.
    ErrMalformedInt = 6,
    /// The config named a Unicode version the engine does not support (S-41).
    ErrUnsupportedUnicode = 7,
    /// The program failed to load: a lex/parse/resolve error (its text is copied into
    /// `doodle_load`'s `err_buf`).
    ErrLoad = 8,
    /// A caller buffer was too small; the required length is written to the `needed`
    /// out-parameter and the buffer is left untouched (copy-out, freeze convention 4).
    ErrBufferTooSmall = 9,
    /// A required pointer argument was NULL.
    ErrNullPointer = 10,
    /// A host-contract violation the engine caught (e.g. resolving a non-suspended instance).
    ErrContract = 11,
    /// A Rust panic was caught at the boundary (an engine bug; the firewall of last resort,
    /// [`crate::guard`]). The instance should be considered unusable.
    ErrPanic = 12,
    /// A pause-scoped observation read used a **stale generation** token: a `drive`/`resolve`
    /// advanced the stack since the token was obtained (D-M7-12). **Benign and expected** — the
    /// host re-walks the stack (`doodle_stack_frame_count`) and retries — NOT a contract
    /// violation (`ErrContract`) and distinct from `ErrStaleHandle` (a released/forged handle).
    ErrStale = 13,
}

/// A value's kind (E§4.4), the C mirror of `doodle_core`'s `Kind`.
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DoodleKind {
    /// `nil` (L§4.9).
    Nil = 0,
    /// A boolean (L§4.1).
    Bool = 1,
    /// An integer of any magnitude (L§4.2).
    Int = 2,
    /// A float (L§4.3).
    Float = 3,
    /// A string (L§4.4).
    String = 4,
    /// A byte string (L§4.5).
    Bytes = 5,
    /// A list (L§4.6).
    List = 6,
    /// A dict (L§4.7).
    Dict = 7,
    /// A record (L§4.14).
    Record = 8,
    /// A callable (L§6).
    Callable = 9,
    /// A module value (L§9).
    Module = 10,
    /// A type value (L§4.12).
    Type = 11,
    /// A foreign (host) value (E§4.5).
    Foreign = 12,
}

/// A driving directive (E§7.3): how far to run before returning to the host.
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DoodleDirective {
    /// Run to the next capability / raise / fault / completion (a fast run).
    RunToCompletion = 0,
    /// Like `RunToCompletion` but also stop at breakpoints and the raise-trap.
    Continue = 1,
    /// Stop at the next safe point, in any frame (synonym of `StepInto`).
    Step = 2,
    /// Step, descending into calls.
    StepInto = 3,
    /// Step, treating a call as one step.
    StepOver = 4,
    /// Run until the current frame returns.
    StepOut = 5,
}

/// The observation-mode granularity (E§8.8): per-statement (default) or per-subexpression.
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DoodleObservationMode {
    /// Per-statement safe points only (the default).
    Statement = 0,
    /// Adds per-subexpression fine safe points (the "watch your expression evaluate" mode).
    Subexpression = 1,
}

/// Whether a host foreign function is a procedure (`to`, yields Void) or a function (`fn`,
/// yields a value) — the C mirror of the core `BodyKind` a foreign descriptor takes
/// (`doodle_foreign_desc_new`).
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DoodleBodyKind {
    /// A procedure (`to`): yields no value; its call is a statement (L§8.4).
    Proc = 0,
    /// A function (`fn`): yields a value the call consumes.
    Func = 1,
}

/// The outcome of a host-invoked reentrant block (`doodle_call_block`, E§5.4/§7.6) — the C
/// mirror of the core `BlockOutcome`. On `NonLocalExit` or `Halted` the host callback **must
/// return promptly with no result**; the engine's apply-site backstop (S-46/S-15) faults a
/// host that drives on regardless, so a violation is a defined `Faulted`, never UB.
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DoodleBlockOutcome {
    /// The block completed (fell off its end, or `continue`d).
    Completed = 0,
    /// A `break`/`return`/raise exited the block across the host boundary; it is parked for
    /// the call site to resume once the callback returns.
    NonLocalExit = 1,
    /// A fault parked (a limit, the S-15 `NestedSuspend`, or a stale block-argument handle);
    /// the drive surfaces it once the callback returns.
    Halted = 2,
}

/// Which kind of stop a drive reached (E§7.2) — the tag of [`DoodleOutcome`].
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DoodleOutcomeKind {
    /// The driven unit finished (`DoodleOutcome::value` is its result, `0` for Void).
    Completed = 0,
    /// A capability must be fulfilled before continuing (`capability`/`request_count`;
    /// the arg marshalling is M7.2).
    Suspended = 1,
    /// An `import` reached an unloaded module (`request_count` path segments; the resolver
    /// is M7.2).
    SuspendedImport = 2,
    /// Stopped at a safe point (`pause_reason`; `breakpoint_id` when a breakpoint).
    Paused = 3,
    /// An uncaught exception reached the boundary (E§9); `span_*` is its site. Read its
    /// described form with `doodle_raised_kind` / `doodle_raised_message`.
    Raised = 4,
    /// A non-resumable engine fault (`fault`).
    Faulted = 5,
}

/// Why a drive paused (E§7.2).
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DoodlePauseReason {
    /// The active `Step*` directive reached its next safe point.
    Step = 0,
    /// A breakpoint was hit (`DoodleOutcome::breakpoint_id`).
    Breakpoint = 1,
    /// A raise was trapped before propagating (E§8.7).
    RaiseTrap = 2,
    /// The host requested a pause (E§8.8).
    HostPause = 3,
    /// The drive's bounded-run fuel was spent (S-40) — a resumable slice boundary; re-drive.
    SliceEnd = 4,
}

/// A non-resumable engine fault (E§7.2/§10). The core's `LimitExceeded(LimitKind)` is
/// flattened into distinct `Limit*` codes (as the wasm surface flattens to string tags).
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DoodleFault {
    /// The step budget was exhausted (E§10.2).
    LimitStepBudget = 0,
    /// The heap-byte ceiling was exceeded (E§10.2).
    LimitHeap = 1,
    /// The non-tail stack-depth limit was exceeded (E§10.2).
    LimitStackDepth = 2,
    /// The tail-history bound was exceeded (E§8.3).
    LimitTailHistory = 3,
    /// A single op's result would exceed the per-op result cap (the latency rail, E§10.2).
    LimitOpResult = 4,
    /// The host cancelled the drive (E§10.1).
    Cancelled = 5,
    /// A suspending capability was reached inside a foreign/native callback (E§5.4, S-15).
    NestedSuspend = 6,
    /// An internal invariant was violated (an engine bug).
    Internal = 7,
}

/// The result of a drive (E§7.2), written by `doodle_drive`/`doodle_resolve`. A flat
/// `#[repr(C)]` struct (not a union — a union in a frozen header cannot grow safely): each
/// field is meaningful only for the `kind`s named in its doc, others are `0`/false. The
/// `reserved` tail is growth room — a future field claims a reserved slot without changing
/// the struct's size or layout (freeze convention 2).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DoodleOutcome {
    /// Which stop was reached (the tag).
    pub kind: DoodleOutcomeKind,
    /// `Paused`: why.
    pub pause_reason: DoodlePauseReason,
    /// `Faulted`: which fault.
    pub fault: DoodleFault,
    /// `Paused` + `pause_reason == Breakpoint`: the breakpoint id.
    pub breakpoint_id: u32,
    /// `Suspended`: the capability id. `SuspendedImport`: the importing module id.
    pub capability: u32,
    /// `Suspended`: the number of bound argument handles. `SuspendedImport`: the number of
    /// dotted path segments. (The M7.2 accessors read the elements.)
    pub request_count: u32,
    /// `Raised`: whether `span_start`/`span_end` name the raising site.
    pub has_span: bool,
    /// `Raised`: the raising site's start byte offset (when `has_span`).
    pub span_start: u32,
    /// `Raised`: the raising site's end byte offset (when `has_span`).
    pub span_end: u32,
    /// `Completed`: the result value (`0` for Void; a reentrant `fn` return populates it once
    /// the boundary can intern one, M7.3). A **host-owned** handle when non-zero —
    /// `doodle_release` it. `0` for every other kind (a raised value is read in described form
    /// via `doodle_raised_kind`/`_message`).
    pub value: DoodleHandle,
    /// Reserved for additive growth (freeze convention 2); always written as `0`.
    pub reserved: [u64; 4],
}

impl DoodleOutcome {
    /// An all-zero outcome to pass as a `doodle_drive` out-parameter (every field defaults to
    /// "not applicable"; the drive overwrites it). A convenience for Rust embedders using the
    /// `rlib`; a C caller passes an uninitialized `DoodleOutcome` (not part of the C header).
    pub fn blank() -> Self {
        DoodleOutcome {
            kind: DoodleOutcomeKind::Completed,
            pause_reason: DoodlePauseReason::Step,
            fault: DoodleFault::Internal,
            breakpoint_id: 0,
            capability: 0,
            request_count: 0,
            has_span: false,
            span_start: 0,
            span_end: 0,
            value: DOODLE_NULL_HANDLE,
            reserved: [0; 4],
        }
    }
}

/// A source position (E§8.1): a byte span into a module's NFC source plus an **opaque
/// instance-scoped module token** (D-M7-14). Equal `module` tokens ⇔ the same module within one
/// instance (stable for its lifetime), and nothing more — it is **not** a documented index;
/// resolve it to the host's canonical id with `doodle_module_canonical_id`. The host maps the
/// byte span to 1-based line/column using the source it holds (the engine exposes positions, not
/// text).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DoodlePosition {
    /// The construct's start byte offset into the module's NFC source.
    pub span_start: u32,
    /// The construct's end byte offset.
    pub span_end: u32,
    /// The opaque module token (see the type doc).
    pub module: u32,
}

/// One live stack frame (E§8.2), filled by `doodle_frame_at` — pure data; the callable is minted
/// separately by `doodle_frame_callable` (D-M7-13). Innermost frame is index 0. The `reserved`
/// tail is additive growth room (freeze convention 2).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DoodleFrame {
    /// Whether this frame runs a callable value (get it via `doodle_frame_callable`); `false` for
    /// the module top level and `do … end` block frames.
    pub has_callable: bool,
    /// Whether `call_site` names where this frame was entered (`false` for the module top level
    /// or a block invoked by native host code).
    pub has_call_site: bool,
    /// Where this frame was entered — meaningful only when `has_call_site`.
    pub call_site: DoodlePosition,
    /// Tail-iterations absorbed into this frame by proper-tail-call reuse (E§8.3): `0` for a
    /// fresh frame, `n` after `n` tail calls reused the same slot.
    pub tail_count: u64,
    /// The frame's home module, as an opaque module token (see [`DoodlePosition::module`]).
    pub module: u32,
    /// Reserved for additive growth (freeze convention 2); always written as `0`.
    pub reserved: [u64; 4],
}

/// What a module-level global is (L§5), the C mirror of the resolver's `GlobalKind` — so a host
/// filters a module's globals (e.g. show `Let`/`Const`/`Parameter` as data, hide `Proc`/`Fn`).
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DoodleGlobalKind {
    /// A mutable binding (`let`).
    Let = 0,
    /// A non-reassignable binding (`const`).
    Const = 1,
    /// A dynamic parameter (`parameter`).
    Parameter = 2,
    /// A procedure (`to`).
    Proc = 3,
    /// A function (`fn`).
    Func = 4,
    /// A record type (`record`/`ref record`).
    Record = 5,
    /// A protocol (`protocol`).
    Protocol = 6,
    /// A nested module (`module`).
    Module = 7,
}

/// One module-level global (E§8.2, L§5), filled by `doodle_module_global` — its `kind` and the
/// `decl_span` of its declaration. Its name copies out separately (`doodle_module_global_name`)
/// and its current value is fetched by the same index (`doodle_module_global_value`). The
/// `reserved` tail is additive growth room (freeze convention 2).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DoodleGlobal {
    /// What kind of declaration the global is.
    pub kind: DoodleGlobalKind,
    /// The source position of the declaration.
    pub decl_span: DoodlePosition,
    /// Reserved for additive growth; always written as `0`.
    pub reserved: [u64; 2],
}

/// Maps a core [`GlobalKind`] to its ABI mirror.
pub(crate) fn global_kind(k: GlobalKind) -> DoodleGlobalKind {
    match k {
        GlobalKind::Let => DoodleGlobalKind::Let,
        GlobalKind::Const => DoodleGlobalKind::Const,
        GlobalKind::Parameter => DoodleGlobalKind::Parameter,
        GlobalKind::Proc => DoodleGlobalKind::Proc,
        GlobalKind::Fn => DoodleGlobalKind::Func,
        GlobalKind::Record => DoodleGlobalKind::Record,
        GlobalKind::Protocol => DoodleGlobalKind::Protocol,
        GlobalKind::Module => DoodleGlobalKind::Module,
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
        ValueError::WrongKind { .. } => DoodleStatus::ErrWrongKind,
        ValueError::IntOutOfRange => DoodleStatus::ErrIntOutOfRange,
        ValueError::IndexOutOfBounds => DoodleStatus::ErrIndexOutOfBounds,
        ValueError::InvalidUtf8 { .. } => DoodleStatus::ErrInvalidUtf8,
        ValueError::MalformedInt => DoodleStatus::ErrMalformedInt,
    }
}
