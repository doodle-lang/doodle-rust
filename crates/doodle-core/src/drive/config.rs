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
        }
    }
}

/// Instance configuration (engine spec E§3.1). The resource-limits subset landed at
/// M2a.9; this adds the target Unicode version (S-41). The module-resolver hook,
/// observation mode, and opaque host-data value join with their features (M2b/M5).
#[derive(Clone, Copy, Debug, Default)]
pub struct Config {
    /// Resource limits (E§10.2).
    pub limits: Limits,
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
