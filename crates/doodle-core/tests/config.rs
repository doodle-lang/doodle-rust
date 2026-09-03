//! Instance config-surface tests (engine spec E§3.1, S-41): `create` validates the
//! config's **optional** target Unicode version against the engine's build-pinned
//! version, and the engine reports that pinned version.

use doodle_core::diag::Severity;
use doodle_core::drive::{Config, ConfigError};
use doodle_core::machine::{Instance, Registry};
use doodle_core::parse::parse_program;
use doodle_core::resolve::{ResolvedModule, resolve};
use doodle_core::source::normalize;
use doodle_core::span::ModuleId;
use doodle_core::unicode::{UNICODE_VERSION, UnicodeVersion};

/// Resolves `src` through the real pipeline into a module ready for `create`.
fn resolved_module(src: &str) -> ResolvedModule {
    let nfc = normalize(src);
    let parsed = parse_program(nfc.as_ref(), ModuleId(0));
    assert!(
        parsed
            .diagnostics
            .iter()
            .all(|d| d.severity != Severity::Error),
        "unexpected parse error(s): {:?}",
        parsed.diagnostics
    );
    let resolved = resolve(parsed.ast, parsed.root, ModuleId(0));
    assert!(
        resolved.diagnostics.is_empty(),
        "unexpected resolve diagnostic(s): {:?}",
        resolved.diagnostics
    );
    resolved.module
}

/// The default config (no version requested) creates an instance — `None` uses the
/// engine's pinned version.
#[test]
fn create_with_default_config_succeeds() {
    assert!(
        Instance::create(
            resolved_module("1\n"),
            Config::default(),
            Registry::new(),
            "main"
        )
        .is_ok()
    );
}

/// Requesting exactly the engine's pinned version succeeds (S-41).
#[test]
fn create_accepts_the_pinned_unicode_version() {
    let config = Config {
        unicode_version: Some(UNICODE_VERSION),
        ..Config::default()
    };
    assert!(Instance::create(resolved_module("1\n"), config, Registry::new(), "main").is_ok());
}

/// Requesting a version the engine does not support fails at create time with the
/// pinned version reported (S-41): a loud failure instead of a silent
/// grapheme/normalization divergence.
#[test]
fn create_rejects_an_unsupported_unicode_version() {
    let wrong = UnicodeVersion {
        major: 1,
        minor: 0,
        micro: 0,
    };
    let config = Config {
        unicode_version: Some(wrong),
        ..Config::default()
    };
    match Instance::create(resolved_module("1\n"), config, Registry::new(), "main") {
        Ok(_) => panic!("an unsupported Unicode version must be rejected"),
        Err(ConfigError::UnsupportedUnicodeVersion { requested, pinned }) => {
            assert_eq!(requested, wrong);
            assert_eq!(pinned, UNICODE_VERSION);
        }
    }
}

/// The engine reports its build-pinned Unicode version (the config field is
/// validated against this), and it is a real UCD release, not the zero default.
#[test]
fn the_engine_reports_its_pinned_unicode_version() {
    assert_eq!(Instance::unicode_version(), UNICODE_VERSION);
    let zero = UnicodeVersion {
        major: 0,
        minor: 0,
        micro: 0,
    };
    assert_ne!(
        Instance::unicode_version(),
        zero,
        "the pinned version should be a real UCD release"
    );
}
