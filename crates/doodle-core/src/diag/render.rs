//! Plain-text rendering of [`Diagnostic`]s for the CLI/CI — no ANSI, so
//! snapshots are byte-stable.
//!
//! The engine exposes positions, not source (engine spec E§8.1); source is
//! injected via [`SourceView`]. Every span in one render call is assumed to
//! index that `SourceView`'s module. The IDE renders its own view from the
//! structured diagnostics and does not use this.
//!
//! Positions come from the canonical source model
//! ([`crate::source`]): line/column are 1-based, columns counted in **code
//! points** (L§3.1, S-1). Display-width caret alignment (tabs, wide / combining
//! characters) is a non-normative refinement layered on `source::col_width`, not
//! a position-unit question. Out-of-range or non-char-boundary offsets are
//! clamped, so a malformed span can never panic.

use super::{Diagnostic, LoadError, Note, Severity};
use crate::source::{LineIndex, clamp_boundary, col_width};
use crate::span::Span;

/// A borrowed view of one module's source, for rendering. `name` is the display
/// path shown in the locator; `source` is the NFC text the byte spans index.
pub struct SourceView<'a> {
    /// The display name/path shown in the `-->` locator line.
    pub name: &'a str,
    /// The NFC-normalized source text the spans index into.
    pub source: &'a str,
}

/// Renders one diagnostic to plain text (trailing newline included).
#[must_use]
pub fn render_diagnostic(diagnostic: &Diagnostic, src: &SourceView<'_>) -> String {
    render_with(diagnostic, src, &LineIndex::new(src.source))
}

/// Renders a slice of diagnostics in the given order, blank-line separated.
/// Order is the producer's contract; the renderer never re-sorts.
#[must_use]
pub fn render_diagnostics(diagnostics: &[Diagnostic], src: &SourceView<'_>) -> String {
    // Build the line index once and share it across all diagnostics.
    let index = LineIndex::new(src.source);
    diagnostics
        .iter()
        .map(|d| render_with(d, src, &index))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_with(diagnostic: &Diagnostic, src: &SourceView<'_>, index: &LineIndex) -> String {
    let mut out = String::new();
    let severity = match diagnostic.severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
    };
    out.push_str(&format!(
        "{severity}[{}]: {}\n",
        diagnostic.code.slug(),
        diagnostic.message
    ));
    if let Some(span) = diagnostic.span {
        push_snippet(&mut out, src, index, span, '^');
    }
    for note in &diagnostic.notes {
        push_note(&mut out, src, index, note);
    }
    if let Some(suggestion) = &diagnostic.suggestion {
        out.push_str(&format!("  = help: {}\n", suggestion.message));
    }
    out
}

/// Renders every diagnostic in a [`LoadError`], in order.
#[must_use]
pub fn render_load_error(error: &LoadError, src: &SourceView<'_>) -> String {
    render_diagnostics(&error.diagnostics, src)
}

fn push_note(out: &mut String, src: &SourceView<'_>, index: &LineIndex, note: &Note) {
    match note.span {
        Some(span) => {
            out.push_str(&format!("note: {}\n", note.message));
            push_snippet(out, src, index, span, '-');
        }
        None => out.push_str(&format!("  = note: {}\n", note.message)),
    }
}

/// Appends a `--> loc` line, the source line, and an underline of `caret`
/// under the span — at least one, so a zero-width / EOF span still points
/// somewhere. A multi-line span underlines its first line only.
fn push_snippet(
    out: &mut String,
    src: &SourceView<'_>,
    index: &LineIndex,
    span: Span,
    caret: char,
) {
    let pos = index.position_at(src.source, span.start);
    let (line_start, line_end) = index.line_bounds(src.source, span.start);
    let line_text = &src.source[line_start..line_end];

    let number = pos.line.to_string();
    let gutter = " ".repeat(number.len());

    out.push_str(&format!("  --> {}:{}:{}\n", src.name, pos.line, pos.column));
    out.push_str(&format!("{number} | {line_text}\n"));

    let pad = (pos.column - 1) as usize;
    let start = clamp_boundary(src.source, span.start as usize);
    let span_end = clamp_boundary(src.source, (span.end as usize).min(line_end));
    let run = col_width(&src.source[start..span_end.max(start)]).max(1);
    out.push_str(&format!(
        "{gutter} | {}{}\n",
        " ".repeat(pad),
        caret.to_string().repeat(run)
    ));
}
