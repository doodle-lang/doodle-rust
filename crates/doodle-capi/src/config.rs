//! The instance configuration surface (E§3.1), as an **opaque handle + builder functions**
//! (D-M7-7, freeze convention 2): a by-value config struct in a frozen header cannot gain a
//! field without an ABI break, but a new `doodle_config_set_*` is purely additive. A host
//! builds a config, passes it to `doodle_load`, and frees it.
//!
//! M7.1 covers limits, observation mode, and the target Unicode version (S-41, the replay
//! guard). The module-resolver and host-data setters are additive M7.2 work.

use crate::abi::DoodleObservationMode;
use doodle_core::drive::{Config, Limits, ObservationMode};
use doodle_core::unicode::UnicodeVersion;

/// An opaque instance configuration under construction. Built with `doodle_config_new`,
/// mutated with the `doodle_config_set_*` setters, consumed by `doodle_load`, and released
/// with `doodle_config_free`.
pub struct DoodleConfig {
    pub(crate) inner: Config,
}

/// Borrows the core [`Config`] behind a raw `DoodleConfig`, or `None` if NULL — the one
/// documented deref the setters share.
fn config_mut<'a>(config: *mut DoodleConfig) -> Option<&'a mut Config> {
    // SAFETY: `as_mut` returns None for NULL; a non-null `config` is a live `DoodleConfig`
    // from `doodle_config_new` (not freed) by the caller's `# Safety` contract.
    unsafe { config.as_mut() }.map(|c| &mut c.inner)
}

/// Creates a config with the engine defaults (E§3.1): default limits, statement-granularity
/// observation, and the engine's pinned Unicode version. Returns NULL only on allocation
/// failure. Free it with `doodle_config_free` (loading does not consume it).
#[unsafe(no_mangle)]
pub extern "C" fn doodle_config_new() -> *mut DoodleConfig {
    Box::into_raw(Box::new(DoodleConfig {
        inner: Config::default(),
    }))
}

/// Frees a config created by `doodle_config_new`. NULL is a no-op. The config is the host's
/// to free whether or not it was passed to `doodle_load` (load copies what it needs).
///
/// # Safety
/// `config` must be a pointer returned by `doodle_config_new` and not already freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_config_free(config: *mut DoodleConfig) {
    if !config.is_null() {
        // SAFETY: `config` is non-null (checked) and, by the caller's `# Safety` contract, a
        // pointer from `doodle_config_new` (a `Box::into_raw`) not already freed.
        drop(unsafe { Box::from_raw(config) });
    }
}

/// Sets the resource limits (E§10.2): the step budget, heap-byte ceiling, non-tail stack
/// depth, and the per-operation result-size cap (the latency rail — `UINT64_MAX` disables
/// it). No-op on a NULL config.
///
/// # Safety
/// `config` must be a live pointer from `doodle_config_new` (or NULL).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_config_set_limits(
    config: *mut DoodleConfig,
    step_budget: u64,
    heap_bytes: u64,
    stack_depth: u32,
    max_op_result_bytes: u64,
) {
    if let Some(inner) = config_mut(config) {
        inner.limits = Limits {
            step_budget,
            heap_bytes,
            stack_depth,
            max_op_result_bytes,
        };
    }
}

/// Sets the observation-mode granularity (E§8.8). No-op on a NULL config.
///
/// # Safety
/// `config` must be a live pointer from `doodle_config_new` (or NULL).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_config_set_observation_mode(
    config: *mut DoodleConfig,
    mode: DoodleObservationMode,
) {
    if let Some(inner) = config_mut(config) {
        inner.observation_mode = match mode {
            DoodleObservationMode::Statement => ObservationMode::Statement,
            DoodleObservationMode::Subexpression => ObservationMode::Subexpression,
        };
    }
}

/// Sets the **target Unicode version** the host expects (S-41, the replay guard): a
/// recording made under one UCD version fails to load under another rather than diverging
/// silently on grapheme/normalization behavior (E§11). A `major` of `0` clears it (use the
/// engine's pinned version). No-op on a NULL config.
///
/// # Safety
/// `config` must be a live pointer from `doodle_config_new` (or NULL).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_config_set_target_unicode(
    config: *mut DoodleConfig,
    major: u16,
    minor: u16,
    micro: u16,
) {
    if let Some(inner) = config_mut(config) {
        inner.unicode_version = if major == 0 {
            None
        } else {
            Some(UnicodeVersion {
                major,
                minor,
                micro,
            })
        };
    }
}
