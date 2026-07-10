//! The source model: NFC-normalized source text and the canonical
//! byte-offset → [`Position`] mapping (S-1, L§3.1).
//!
//! A [`Position`] is a 1-based line and a 1-based **code-point** column over the
//! NFC-normalized text (L§3.1 "Source positions"). The internal [`Span`] stays
//! byte offsets; code-point columns are derived here, at the boundary — exactly
//! the conversion S-1 licenses. `Span`s are `u32`, so source is assumed to fit
//! `u32` bytes.
//!
//! [`Span`]: crate::span::Span

use crate::unicode;
use std::borrow::Cow;

/// A 1-based source position: line and code-point column (L§3.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Position {
    /// 1-based line number.
    pub line: u32,
    /// 1-based column, counted in Unicode code points from the line start.
    pub column: u32,
}

/// NFC-normalizes raw source text on load (L§3.1). Borrows if already NFC.
pub fn normalize(raw: &str) -> Cow<'_, str> {
    unicode::nfc(raw)
}

/// The column width of a source slice, in code points (S-1). This is the single
/// site the S-1 display-width *caret refinement* (tabs, wide / combining chars)
/// would graft onto — a non-normative alignment concern, since L§3.1 fixes the
/// position *unit* at code points.
pub fn col_width(slice: &str) -> usize {
    slice.chars().count()
}

/// A precomputed index of line-start byte offsets over a source string, for
/// byte-offset → [`Position`] lookups without rescanning the whole source.
#[derive(Clone, Debug)]
pub struct LineIndex {
    /// Byte offset of the start of each line; line 1 starts at offset 0.
    line_starts: Vec<u32>,
}

impl LineIndex {
    /// Builds the index over `source` (assumed already NFC).
    #[must_use]
    pub fn new(source: &str) -> Self {
        debug_assert!(
            source.len() <= u32::MAX as usize,
            "source exceeds the u32 byte range"
        );
        let mut line_starts = vec![0u32];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push((i + 1) as u32);
            }
        }
        LineIndex { line_starts }
    }

    /// The 1-based [`Position`] of `byte_off` in `source`. Out-of-range or
    /// non-char-boundary offsets are clamped, so this never panics.
    #[must_use]
    pub fn position_at(&self, source: &str, byte_off: u32) -> Position {
        let off = clamp_boundary(source, byte_off as usize);
        let line_ix = self.line_of(off);
        let line_start = self.line_starts[line_ix] as usize;
        let column = col_width(&source[line_start..off]) + 1;
        Position {
            line: line_ix as u32 + 1,
            column: column as u32,
        }
    }

    /// Byte bounds `[start, end)` of the line containing `byte_off`, excluding
    /// the line terminator (the `\n` and a CRLF `\r`).
    #[must_use]
    pub fn line_bounds(&self, source: &str, byte_off: u32) -> (usize, usize) {
        let off = clamp_boundary(source, byte_off as usize);
        let start = self.line_starts[self.line_of(off)] as usize;
        let mut end = source[start..]
            .find('\n')
            .map_or(source.len(), |i| start + i);
        if end > start && source.as_bytes()[end - 1] == b'\r' {
            end -= 1;
        }
        (start, end)
    }

    /// The 0-based index of the line containing byte offset `off`.
    fn line_of(&self, off: usize) -> usize {
        self.line_starts
            .partition_point(|&start| start as usize <= off)
            .saturating_sub(1)
    }
}

/// Clamps `byte_off` to `source.len()` and snaps it down to a char boundary, so
/// slicing at the result never panics on a malformed or out-of-range offset.
pub(crate) fn clamp_boundary(source: &str, byte_off: usize) -> usize {
    let mut off = byte_off.min(source.len());
    while off > 0 && !source.is_char_boundary(off) {
        off -= 1;
    }
    off
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_is_code_points_not_bytes() {
        // `café θ x`: é and θ are each 2 bytes, so `x` is at byte 9 but the
        // code-point column is 8.
        let source = "café θ x";
        let index = LineIndex::new(source);
        let x_byte = source.find('x').unwrap() as u32;
        assert_eq!(x_byte, 9);
        assert_eq!(
            index.position_at(source, x_byte),
            Position { line: 1, column: 8 }
        );
    }

    #[test]
    fn positions_across_lines() {
        let source = "ab\ncdé\nf";
        let index = LineIndex::new(source);
        assert_eq!(
            index.position_at(source, 0),
            Position { line: 1, column: 1 }
        );
        assert_eq!(
            index.position_at(source, 3),
            Position { line: 2, column: 1 }
        ); // 'c'
        let f = source.find('f').unwrap() as u32;
        assert_eq!(
            index.position_at(source, f),
            Position { line: 3, column: 1 }
        );
    }

    #[test]
    fn out_of_range_and_mid_char_offsets_clamp_without_panic() {
        let source = "café";
        let index = LineIndex::new(source);
        let _ = index.position_at(source, 9999); // past end
        let _ = index.position_at(source, 4); // splitting é (bytes 3..5)
        let _ = index.line_bounds(source, 9999);
    }

    #[test]
    fn line_bounds_strip_crlf() {
        let source = "one\r\ntwo\r\n";
        let index = LineIndex::new(source);
        let (s, e) = index.line_bounds(source, 0);
        assert_eq!(&source[s..e], "one"); // no trailing \r
    }

    #[test]
    fn normalize_is_nfc() {
        assert_eq!(normalize("cafe\u{301}").as_ref(), "caf\u{e9}");
    }

    #[test]
    fn position_value_edges() {
        let s = "ab\ncd"; // no trailing newline
        let ix = LineIndex::new(s);
        assert_eq!(ix.position_at(s, 2), Position { line: 1, column: 3 }); // the '\n'
        assert_eq!(ix.position_at(s, 3), Position { line: 2, column: 1 }); // 'c'
        assert_eq!(ix.position_at(s, 5), Position { line: 2, column: 3 }); // EOF after 'cd'

        let t = "ab\n"; // trailing newline: EOF is the start of the (empty) line 2
        let tix = LineIndex::new(t);
        assert_eq!(tix.position_at(t, 3), Position { line: 2, column: 1 });

        let e = ""; // empty source
        let eix = LineIndex::new(e);
        assert_eq!(eix.position_at(e, 0), Position { line: 1, column: 1 });
        assert_eq!(eix.line_bounds(e, 0), (0, 0));
    }

    #[test]
    fn crlf_column_counts_the_carriage_return() {
        // With no load-time CRLF->LF (deferred spec-delta), the CR is a code
        // point on its line: in "a\r\nb", the CR is line 1 column 2 and `b` is
        // line 2 column 1.
        let s = "a\r\nb";
        let ix = LineIndex::new(s);
        assert_eq!(ix.position_at(s, 1), Position { line: 1, column: 2 }); // CR
        assert_eq!(ix.position_at(s, 3), Position { line: 2, column: 1 }); // 'b'
    }
}
