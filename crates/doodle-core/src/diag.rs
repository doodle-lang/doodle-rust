//! Diagnostics and load-time errors.
//!
//! Shell for M0: the shapes the front end reports through. The staged front
//! end (M1) fills in the lexical / parse / resolve producers.

use crate::span::{ModuleId, Span};

/// The severity of a [`Diagnostic`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    /// A hard error: the affected unit cannot be loaded or run.
    Error,
    /// A warning (e.g. the L§5.1 shadowing lint): does not prevent loading.
    Warning,
}

/// A single diagnostic message tied to a source location.
#[derive(Clone, Debug)]
pub struct Diagnostic {
    /// How severe the diagnostic is.
    pub severity: Severity,
    /// Human-readable message text.
    pub message: String,
    /// The module the diagnostic refers to, if known.
    pub module: Option<ModuleId>,
    /// The source span the diagnostic refers to, if any.
    pub span: Option<Span>,
}

impl Diagnostic {
    /// Creates an `Error`-severity diagnostic for `module` at `span`.
    pub fn error(message: impl Into<String>, module: ModuleId, span: Span) -> Self {
        Diagnostic {
            severity: Severity::Error,
            message: message.into(),
            module: Some(module),
            span: Some(span),
        }
    }
}

/// The error returned when a module fails to load (lex / parse / resolve).
///
/// Carries the diagnostics that caused the failure (at least one `Error`).
#[derive(Clone, Debug)]
pub struct LoadError {
    /// The diagnostics describing why the load failed.
    pub diagnostics: Vec<Diagnostic>,
}
