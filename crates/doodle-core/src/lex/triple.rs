//! Triple-quoted string scanning (L§3.6.4). Two forms (S-53): the **single-line**
//! form opens and closes on one line (inline value, no margin); the
//! **multi-line** form spans lines with S-3 margin stripping. Both emit the same
//! `StrStart (StrText | interpolation)* StrEnd` stream as a plain string
//! (`string.rs`), reusing its `scan_text_run`/`scan_interp`/`close_synthetic`.

use super::string::{Boundary, StrOutcome};
use crate::diag::code::DiagnosticCode;
use crate::lex::escape;
use crate::lex::token::TokenKind;
use crate::span::Span;

impl super::Lexer<'_> {
    /// Scans a triple-quoted string at `start` (the first of three `"`), in one
    /// of two forms (§3.6.4, S-53): the **single-line** form closes on the
    /// opening line (inline value, no margin); the **multi-line** form spans
    /// lines with S-3 margin stripping. Value decoding stays with the parser.
    ///
    /// The single-line form is scanned speculatively: it emits tokens as it goes
    /// and, if the line ends before a top-level closing `"""` (or an
    /// interpolation runs off the line), the emitted tokens/diagnostics are
    /// rolled back and the multi-line form is scanned instead. Scanning
    /// speculatively — rather than pre-computing the close position — lets the
    /// real interpolation lexer consume any `"""` *inside* a `{ … }`, so the
    /// close is never mistaken for one in an interpolation body.
    pub(super) fn scan_triple_string(&mut self, start: usize) {
        self.emit_len(TokenKind::StrStart, 3); // opening `"""`; pos = start + 3
        // Save point (just past the opening `"""`) for rollback. `scan_interp`
        // restores `bracket_depth` itself, so it is not part of the snapshot.
        let mark = (
            self.pos,
            self.tokens.len(),
            self.diagnostics.len(),
            self.last_significant,
        );
        if self.try_single_line_triple() {
            return;
        }
        let (pos, tokens, diags, last) = mark;
        self.pos = pos;
        self.tokens.truncate(tokens);
        self.diagnostics.truncate(diags);
        self.last_significant = last;
        self.scan_multi_line_triple(start);
    }

    /// Attempts the single-line form from the current position (just past the
    /// opening `"""`): emits `StrText`/interpolation parts, and on reaching a
    /// top-level closing `"""` emits `StrEnd` and returns `true`. Returns `false`
    /// (for the caller to roll back and try multi-line) if the line ends — via a
    /// newline/EOF, or an interpolation that runs off the line — with no close.
    fn try_single_line_triple(&mut self) -> bool {
        loop {
            let text_start = self.pos;
            let boundary = self.scan_single_line_text();
            if self.pos > text_start {
                self.emit(
                    TokenKind::StrText,
                    Span::new(text_start as u32, self.pos as u32),
                );
            }
            match boundary {
                Boundary::Close => {
                    self.emit_len(TokenKind::StrEnd, 3); // closing `"""`
                    return true;
                }
                Boundary::Interp => {
                    if self.scan_interp(0) == StrOutcome::Broken {
                        return false; // the interpolation ran off the line
                    }
                }
                Boundary::Newline | Boundary::Eof => return false,
            }
        }
    }

    /// Scans single-line triple content until a **top-level** closing `"""`, an
    /// interpolation `{`, or a newline/EOF. A single/double `"` is literal;
    /// escapes and `{{`/`}}` behave as in any string. A `"""` inside a `{ … }` is
    /// consumed by `scan_interp` (from the `Interp` boundary), never seen here.
    fn scan_single_line_text(&mut self) -> Boundary {
        loop {
            match self.bytes.get(self.pos).copied() {
                None => return Boundary::Eof,
                Some(b'\n') => return Boundary::Newline,
                Some(b'"')
                    if self.bytes.get(self.pos + 1) == Some(&b'"')
                        && self.bytes.get(self.pos + 2) == Some(&b'"') =>
                {
                    return Boundary::Close;
                }
                Some(b'\\') => {
                    let esc = escape::scan_escape(self.source, self.pos, false);
                    if let Some(e) = esc.error {
                        self.error(e.code, e.span, e.message);
                    }
                    self.pos = esc.end;
                }
                Some(b'{') => {
                    if self.bytes.get(self.pos + 1) == Some(&b'{') {
                        self.pos += 2; // `{{` — a literal brace
                    } else {
                        return Boundary::Interp;
                    }
                }
                Some(b'}') if self.bytes.get(self.pos + 1) == Some(&b'}') => {
                    self.pos += 2; // `}}` — a literal brace
                }
                Some(_) => self.pos += self.char_at(self.pos).len_utf8(),
            }
        }
    }

    /// Scans the multi-line form: the opening `"""` must be the last token on its
    /// line (else, since it did not close on the line, a hybrid error), the
    /// contents begin on the next line, and S-3 margin stripping applies.
    fn scan_multi_line_triple(&mut self, start: usize) {
        // Only whitespace may follow the opening `"""` (the contents begin on the
        // next line). Any other content — with no same-line close — is the S-53
        // hybrid error.
        let mut p = self.pos;
        while matches!(self.bytes.get(p), Some(b' ' | b'\t' | b'\r')) {
            p += 1;
        }
        if !matches!(self.bytes.get(p), Some(b'\n') | None) {
            // `p` is a code-point boundary (only ASCII whitespace was skipped).
            let end = p + self.char_at(p).len_utf8();
            self.error(
                DiagnosticCode::MalformedTripleQuote,
                Span::new(p as u32, end as u32),
                "content follows the opening `\"\"\"` but the string does not close on \
                 this line — close it here with `\"\"\"`, or start the contents on the \
                 next line for a multi-line string",
            );
            while !matches!(self.bytes.get(p), None | Some(b'\n')) {
                p += 1;
            }
        }
        if self.bytes.get(p).is_none() {
            return self.unterminated_triple(start);
        }
        let content_start = p + 1; // past the newline that ends the opening line

        let Some((margin_start, closing_pos)) = self.find_triple_close(content_start) else {
            return self.unterminated_triple(start);
        };
        // The newline immediately before the closing line is not part of the value.
        let content_end = margin_start.saturating_sub(1);
        self.emit_triple_content(content_start, content_end, margin_start, closing_pos);

        self.pos = closing_pos;
        self.emit_len(TokenKind::StrEnd, 3); // closing `"""`
    }

    /// Reports an unclosed triple-quoted string and closes the stream.
    fn unterminated_triple(&mut self, start: usize) {
        let end = self.source.len();
        self.error(
            DiagnosticCode::UnterminatedString,
            Span::new(start as u32, end as u32),
            "this triple-quoted string is never closed",
        );
        self.pos = end;
        self.close_synthetic(TokenKind::StrEnd);
    }

    /// Scans physical lines from `from` for the closing `"""` — the first line
    /// whose first non-whitespace content is `"""`. Returns `(line_start,
    /// close_pos)`, where the margin is `source[line_start..close_pos]`, or
    /// `None` at EOF (unterminated).
    fn find_triple_close(&self, from: usize) -> Option<(usize, usize)> {
        let mut line_start = from;
        loop {
            let mut q = line_start;
            while matches!(self.bytes.get(q), Some(b' ' | b'\t')) {
                q += 1;
            }
            if self.bytes.get(q) == Some(&b'"')
                && self.bytes.get(q + 1) == Some(&b'"')
                && self.bytes.get(q + 2) == Some(&b'"')
            {
                return Some((line_start, q));
            }
            let mut r = line_start;
            while !matches!(self.bytes.get(r), None | Some(b'\n')) {
                r += 1;
            }
            self.bytes.get(r)?; // EOF before a closing `"""`
            line_start = r + 1;
        }
    }

    /// Emits the content region as `StrText` chunks and interpolations, stripping
    /// the margin from each nonempty line and joining lines with a `\n` chunk.
    /// `content_end` is the newline before the closing line (not in the value).
    fn emit_triple_content(
        &mut self,
        content_start: usize,
        content_end: usize,
        margin_start: usize,
        closing_pos: usize,
    ) {
        let margin_len = closing_pos - margin_start;
        self.pos = content_start;
        while self.pos < content_end {
            // A truly empty line (an immediate newline) is exempt from the margin.
            if self.bytes.get(self.pos) != Some(&b'\n') {
                self.strip_margin(margin_start, margin_len);
            }
            loop {
                let text_start = self.pos;
                let boundary = self.scan_text_run(true, false, true);
                if self.pos > text_start {
                    self.emit(
                        TokenKind::StrText,
                        Span::new(text_start as u32, self.pos as u32),
                    );
                }
                match boundary {
                    Boundary::Interp => {
                        self.scan_interp(0);
                    }
                    _ => break, // Newline or Eof ends the physical line
                }
            }
            if self.pos < content_end {
                // The newline joining two content lines is part of the value.
                // `emit` advances `self.pos` to the span end (past the newline).
                self.emit(
                    TokenKind::StrText,
                    Span::new(self.pos as u32, (self.pos + 1) as u32),
                );
            }
        }
    }

    /// Consumes the margin at the start of a content line, or reports the first
    /// character that fails the byte-for-byte match and stops (leaving the rest
    /// as content for recovery).
    fn strip_margin(&mut self, margin_start: usize, margin_len: usize) {
        for i in 0..margin_len {
            let expected = self.bytes.get(margin_start + i).copied();
            let actual = self.bytes.get(self.pos).copied();
            if actual == expected && !matches!(actual, None | Some(b'\n')) {
                self.pos += 1;
                continue;
            }
            let message = match (expected, actual) {
                (Some(b' '), Some(b'\t')) => "a tab where the closing `\"\"\"` margin has a space",
                (Some(b'\t'), Some(b' ')) => "a space where the closing `\"\"\"` margin has a tab",
                _ => "this line doesn't reach the closing `\"\"\"` margin",
            };
            // Span the whole offending code point (it may be multibyte); `self.pos`
            // is a boundary here (margin bytes are ASCII) and within content.
            let end = self.pos + self.char_at(self.pos).len_utf8();
            self.error(
                DiagnosticCode::MarginMismatch,
                Span::new(self.pos as u32, end as u32),
                message,
            );
            return;
        }
    }
}
