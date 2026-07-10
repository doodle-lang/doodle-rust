//! Diagnostics: the structured error/warning model the front end produces, plus
//! a plain-text renderer for the CLI/CI ([`render`]).
//!
//! The structured form is the normative surface (engine spec E§3.2: a
//! `LoadError` carrying positions); rendering is a separate, non-normative
//! concern (E§8.1: the host holds source and renders, and the IDE renders its
//! own view from the structured diagnostics). This module models **load-time**
//! (lex / parse / resolve) diagnostics only — runtime exceptions and traces are
//! `drive::Outcome::Raised`, not diagnostics.

pub mod code;
pub mod render;

pub use code::DiagnosticCode;

use crate::span::{ModuleId, Span};

/// The severity of a [`Diagnostic`]. Two tiers suffice for the front end:
/// errors fail the load; warnings (e.g. the L§5.1 shadowing lint) do not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    /// A static error: the module cannot be loaded.
    Error,
    /// A warning: reported, but does not prevent loading.
    Warning,
}

/// A secondary annotation on a [`Diagnostic`] — an "…and note that" line,
/// optionally pointing at a second location (e.g. "the original binding is
/// here" for a duplicate declaration, or the outer binding a shadow hides). A
/// `None` span renders as a spanless note.
///
/// The span is assumed to index the parent diagnostic's module (the renderer
/// draws it against the same `SourceView`); a `ModuleId` is added here when
/// cross-module notes land (M5).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Note {
    /// The note text.
    pub message: String,
    /// A secondary location the note points at, if any.
    pub span: Option<Span>,
}

/// A machine-applicable edit backing a [`Suggestion`]: replace the text in
/// `span` with `text`. Carried in the structured form so an IDE can offer a
/// real quick-fix (E§8.1); the plain-text renderer shows only the suggestion's
/// prose, not the edit.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Replacement {
    /// The span whose text is replaced.
    pub span: Span,
    /// The replacement text.
    pub text: String,
}

/// The "here is how to fix it" element of a diagnostic (rubric element (c)).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Suggestion {
    /// The human-readable fix advice.
    pub message: String,
    /// A single machine-applicable edit realizing the fix, if one is well
    /// defined. M1 models exactly one contiguous edit; a multi-edit quick-fix
    /// would reshape this into a list.
    pub replacement: Option<Replacement>,
}

/// A single diagnostic tied to a source location.
///
/// The four error-message-rubric elements map to fields: name the
/// value/operation and stay kid-readable (`message`), point at it (`span` +
/// `notes`), suggest the fix (`suggestion`). `code` is the stable machine
/// identifier. The structured form is normative; rendering (via [`render`]) is
/// separate.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Diagnostic {
    /// Error (fails the load) or Warning (does not).
    pub severity: Severity,
    /// The stable machine-readable class identifier.
    pub code: DiagnosticCode,
    /// The kid-readable primary message.
    pub message: String,
    /// The module the primary span refers to (needed to fetch source text).
    pub module: Option<ModuleId>,
    /// The primary source span the diagnostic points at.
    pub span: Option<Span>,
    /// Secondary annotations / labeled locations.
    pub notes: Vec<Note>,
    /// The suggested fix, if any.
    pub suggestion: Option<Suggestion>,
}

impl Diagnostic {
    /// An `Error`-severity diagnostic for `module` at `span`.
    #[must_use]
    pub fn error(
        code: DiagnosticCode,
        module: ModuleId,
        span: Span,
        message: impl Into<String>,
    ) -> Self {
        Diagnostic {
            severity: Severity::Error,
            code,
            message: message.into(),
            module: Some(module),
            span: Some(span),
            notes: Vec::new(),
            suggestion: None,
        }
    }

    /// A `Warning`-severity diagnostic (does not fail the load).
    #[must_use]
    pub fn warning(
        code: DiagnosticCode,
        module: ModuleId,
        span: Span,
        message: impl Into<String>,
    ) -> Self {
        Diagnostic {
            severity: Severity::Warning,
            code,
            message: message.into(),
            module: Some(module),
            span: Some(span),
            notes: Vec::new(),
            suggestion: None,
        }
    }

    /// Adds a secondary note (builder style).
    #[must_use]
    pub fn with_note(mut self, note: Note) -> Self {
        self.notes.push(note);
        self
    }

    /// Attaches a suggested fix (builder style).
    #[must_use]
    pub fn with_suggestion(mut self, suggestion: Suggestion) -> Self {
        self.suggestion = Some(suggestion);
        self
    }
}

/// The error returned when a module fails to load (lex / parse / resolve).
///
/// Carries every diagnostic produced by one load (the front end keeps reporting
/// after recovery), including at least one `Error`. Diagnostics are held in
/// producer order, which the front end guarantees is deterministic and
/// source-ordered; the renderer never re-sorts (see [`render`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LoadError {
    /// The diagnostics describing the failure (at least one `Error`).
    pub diagnostics: Vec<Diagnostic>,
}
