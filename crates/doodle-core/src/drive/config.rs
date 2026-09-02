//! Instance configuration and resource limits (engine spec E§3.1/§10.2). Split from
//! `drive.rs` (the outcomes and the drive loop) so that file stays within the hygiene
//! length limit; re-exported from [`crate::drive`], so the public paths are unchanged.

use crate::unicode::UnicodeVersion;

/// Which limit was exceeded (engine spec E§10.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LimitKind {
    /// The step budget (safe points executed).
    StepBudget,
    /// The heap limit (bytes or objects).
    Heap,
    /// The non-tail-call stack-depth limit.
    StackDepth,
    /// The tail-history bound (E§8.3).
    TailHistory,
    /// A **single operation's result** would exceed the per-operation result-size cap
    /// ([`Limits::max_op_result_bytes`]) — the *latency rail*, distinct from running out of heap:
    /// the result fits in memory but is too big to compute in one atomic step (a huge `**`/`*` or
    /// a huge string repetition). An atomic operation cannot yield mid-way (S-40), so this
    /// pre-admission cap is what bounds the longest single operation's latency.
    OpResult,
}

/// Resource limits for an instance (engine spec E§10.2), enforced by the machine
/// at statement-level safe points (E§7.4). This is the limits **subset** of the
/// `create(config)` surface (E§3.1); the rest — the module-resolver hook, target
/// Unicode version (S-41), observation mode, and host data — lands with the full
/// config surface at **M2a.11**.
///
/// Exceeding any limit yields a `Faulted(LimitExceeded(..))` outcome (see [`crate::drive`]).
/// Proper tail calls reuse frames (L§8.7), so a tail loop never trips `stack_depth`;
/// a runaway **non-tail** recursion does. The tail-history bound (E§8.3) is a fixed
/// ring capacity that overwrites its oldest entry rather than faulting, so it is not
/// a field here.
///
/// The [`Default`] values are **provisional** engineering ceilings: generous enough
/// that ordinary kid-authored programs never trip them, yet finite so a runaway
/// still faults. E§10.2 leaves the concrete values to host config; the real
/// host-chosen values arrive with the M2a.11 config surface.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// Maximum statement-level safe points executed (E§7.4) before
    /// `LimitExceeded(StepBudget)`. The engine owns no clock, so a host enforces a
    /// wall-clock timeout via this budget or by cancelling (E§10.2).
    pub step_budget: u64,
    /// Maximum heap payload bytes ([`Heap::bytes_allocated`](crate::heap::Heap::bytes_allocated),
    /// which excludes pure caches, MD §5) before `LimitExceeded(Heap)`.
    pub heap_bytes: u64,
    /// Maximum non-tail frame-stack depth before `LimitExceeded(StackDepth)`.
    pub stack_depth: u32,
    /// Maximum bytes a **single result-growing operation** may produce before
    /// `LimitExceeded(OpResult)` — the **latency rail** (E§10.2): heap bounds *space*, the step
    /// budget bounds *total work*, and this bounds *one operation's latency*. A bignum `*`/`**` or
    /// a string repetition estimates its result size up front and faults **before** computing when
    /// it would exceed this, so a result that fits the heap but would take seconds to compute (an
    /// atomic op cannot be interrupted, S-40) does not freeze the host. Default [`u64::MAX`] leaves
    /// it bounded only by `heap_bytes` (no change for hosts that do not set it); an untrusted host
    /// (a browser demo) sets it small.
    pub max_op_result_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            // ~1.1e12 safe points, 1 GiB payload bytes, 100k non-tail frames — a
            // generous embedder backstop (a browser demo or other untrusted host sets
            // tighter values); see the type's "provisional" note.
            step_budget: 1 << 40,
            heap_bytes: 1 << 30,
            stack_depth: 100_000,
            // The latency rail is off by default (bounded only by the heap); an untrusted host
            // sets a small value to keep a single op's compute time bounded.
            max_op_result_bytes: u64::MAX,
        }
    }
}

/// The observation-mode granularity (engine spec E§8.8, S-62): where the engine may
/// place safe points a `Step*`/host-pause can stop at. The single observation axis —
/// there is no eager/lazy local-capture axis, since inspection is pull-based (§8): the
/// host reads live frame state on demand when stopped (E§8.8, ratified `aac6766`).
/// Switching mode changes only *where stepping may stop* (§7.4); it never changes what
/// the program computes or when a limit trips — resource accounting stays at statement
/// safe points in every mode (S-20/S-62), so a fault lands at the same instant either way.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ObservationMode {
    /// Per-statement safe points only (the default): between statements, at call entry, and
    /// at return. The coarse mode a host runs when nobody is watching — it pays nothing extra.
    #[default]
    Statement,
    /// Adds **observation-only** fine safe points at the completion of every non-leaf
    /// subexpression (E§7.4, S-62) — operator applications, field access, index steps,
    /// interpolation pieces — the "watch your expression evaluate" primitive. Paid only while
    /// this mode is on.
    Subexpression,
}

/// Instance configuration (engine spec E§3.1). The resource-limits subset landed at
/// M2a.9; the target Unicode version (S-41) and observation mode (S-62) followed. The
/// module-resolver hook and opaque host-data value join with their features.
#[derive(Clone, Copy, Debug, Default)]
pub struct Config {
    /// Resource limits (E§10.2).
    pub limits: Limits,
    /// The observation-mode granularity (E§8.8, S-62): per-statement (default) or
    /// per-subexpression safe points. Adjustable at run time via
    /// [`set_observation_mode`](crate::machine::Instance::set_observation_mode).
    pub observation_mode: ObservationMode,
    /// The requested target Unicode version (E§3.1, L§4.4). `None` uses the engine's
    /// build-pinned version ([`crate::unicode::UNICODE_VERSION`]); `Some(v)` that is
    /// not that pinned version fails [`create`](crate::machine::Instance::create)
    /// with [`ConfigError::UnsupportedUnicodeVersion`] (S-41). Naming it lets a host
    /// assert the version a recording was made under at create time — a loud failure
    /// instead of a silent grapheme/normalization divergence (determinism, E§11).
    pub unicode_version: Option<UnicodeVersion>,
}

/// Why [`create`](crate::machine::Instance::create) rejected a [`Config`] (E§3.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConfigError {
    /// The config named a Unicode version the engine does not support — at M2a, any
    /// version other than the single build-pinned one (S-41).
    UnsupportedUnicodeVersion {
        /// The version the host requested.
        requested: UnicodeVersion,
        /// The engine's build-pinned version.
        pinned: UnicodeVersion,
    },
}
