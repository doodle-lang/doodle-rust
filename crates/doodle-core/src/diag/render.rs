//! Plain-text rendering of [`Diagnostic`]s for the CLI/CI — no ANSI, so
//! snapshots are byte-stable.
//!
//! The engine exposes positions, not source (engine spec E§8.1); source is
//! injected via [`SourceView`]. Every span in one render call is assumed to
//! index that `SourceView`'s module. The IDE renders its own view from the
//! structured diagnostics and does not use this.
//!
//! **Column model (M1.1 provisional — the M1.1→M1.2 seam):** line/column and
//! caret widths are counted in **code points** over the NFC source
//! (S-1-aligned) by the single `col_width` helper — the one site the M1.2/S-1
//! display-width model (tabs, wide / combining characters) grafts onto. Out-of
//! -range or non-char-boundary byte offsets are clamped defensively, so a
//! malformed span can never panic.

use super::{Diagnostic, LoadError, Note, Severity};
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
        push_snippet(&mut out, src, span, '^');
    }
    for note in &diagnostic.notes {
        push_note(&mut out, src, note);
    }
    if let Some(suggestion) = &diagnostic.suggestion {
        out.push_str(&format!("  = help: {}\n", suggestion.message));
    }
    out
}

/// Renders a slice of diagnostics in the given order, blank-line separated.
/// Order is the producer's contract; the renderer never re-sorts.
#[must_use]
pub fn render_diagnostics(diagnostics: &[Diagnostic], src: &SourceView<'_>) -> String {
    diagnostics
        .iter()
        .map(|d| render_diagnostic(d, src))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders every diagnostic in a [`LoadError`], in order.
#[must_use]
pub fn render_load_error(error: &LoadError, src: &SourceView<'_>) -> String {
    render_diagnostics(&error.diagnostics, src)
}

fn push_note(out: &mut String, src: &SourceView<'_>, note: &Note) {
    match note.span {
        Some(span) => {
            out.push_str(&format!("note: {}\n", note.message));
            push_snippet(out, src, span, '-');
        }
        None => out.push_str(&format!("  = note: {}\n", note.message)),
    }
}

/// Appends a `--> loc` line, the source line, and an underline of `caret`
/// under the span — at least one, so a zero-width / EOF span still points
/// somewhere. A multi-line span underlines its first line only.
fn push_snippet(out: &mut String, src: &SourceView<'_>, span: Span, caret: char) {
    let start = clamp_boundary(src.source, span.start as usize);
    let (line, col) = line_col(src.source, start);
    let (line_start, line_end) = line_bounds(src.source, start);
    let line_text = &src.source[line_start..line_end];

    let number = line.to_string();
    let gutter = " ".repeat(number.len());

    out.push_str(&format!("  --> {}:{line}:{col}\n", src.name));
    out.push_str(&format!("{number} | {line_text}\n"));

    let pad = col_width(&src.source[line_start..start]);
    let span_end = clamp_boundary(src.source, (span.end as usize).min(line_end));
    let run = col_width(&src.source[start..span_end.max(start)]).max(1);
    out.push_str(&format!(
        "{gutter} | {}{}\n",
        " ".repeat(pad),
        caret.to_string().repeat(run)
    ));
}

/// 1-based (line, code-point column) of `byte_off` in `source`.
fn line_col(source: &str, byte_off: usize) -> (u32, u32) {
    let off = clamp_boundary(source, byte_off);
    let newlines = source.as_bytes()[..off]
        .iter()
        .filter(|&&b| b == b'\n')
        .count();
    let line_start = source[..off].rfind('\n').map_or(0, |i| i + 1);
    (
        1 + newlines as u32,
        1 + col_width(&source[line_start..off]) as u32,
    )
}

/// Byte bounds `[start, end)` of the line containing `byte_off`, excluding the
/// line terminator (both the `\n` and a CRLF `\r`).
fn line_bounds(source: &str, byte_off: usize) -> (usize, usize) {
    let off = clamp_boundary(source, byte_off);
    let start = source[..off].rfind('\n').map_or(0, |i| i + 1);
    let mut end = source[off..].find('\n').map_or(source.len(), |i| off + i);
    // Drop a CRLF carriage return so it neither prints nor widens the caret.
    // (Whether load normalizes CRLF->LF is an open L§3.1 question, M1.2; until
    // then the renderer handles a stray CR defensively.)
    if end > start && source.as_bytes()[end - 1] == b'\r' {
        end -= 1;
    }
    (start, end)
}

/// The column width of a source slice, in code points (S-1-aligned). The single
/// site the M1.2/S-1 display-width model grafts onto (swap the body for a
/// grapheme / east-asian-width count and both column and caret follow).
fn col_width(slice: &str) -> usize {
    slice.chars().count()
}

/// Clamps `byte_off` to `source.len()` and snaps it down to a char boundary, so
/// slicing at the result never panics on a malformed or out-of-range span.
fn clamp_boundary(source: &str, byte_off: usize) -> usize {
    let mut off = byte_off.min(source.len());
    while off > 0 && !source.is_char_boundary(off) {
        off -= 1;
    }
    off
}
