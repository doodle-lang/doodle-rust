//! String and bytes literal scanning (L§3.6.3/§3.6.5).
//!
//! A double-quoted string is lexed as a **stream** — `StrStart`, then runs of
//! `StrText` interleaved with interpolations, then `StrEnd` — so an escape or
//! interpolation error inside `{…}` gets a real source position at `stage: lex`.
//! Interpolations recurse (a string may appear inside `{…}`), which the Rust
//! call stack models directly; [`MAX_INTERP_DEPTH`] bounds it against
//! adversarial input. Escape *values* are decoded by the parser (M1.6); here we
//! validate shape only, as with numeric literals.
//!
//! A bytes literal `b"…"` is one `Bytes` token: ASCII source, the closed escape
//! set minus `\u`, and no interpolation.

use crate::diag::code::DiagnosticCode;
use crate::lex::escape;
use crate::lex::token::TokenKind;
use crate::span::Span;

/// Bounds interpolation nesting (`"{"{…}"}"`) so adversarial input can't
/// overflow the stack. Real code never approaches it.
const MAX_INTERP_DEPTH: u32 = 32;

/// Where a run of literal text stopped.
enum Boundary {
    /// A closing `"`.
    Close,
    /// A newline — a single-line literal can't span lines.
    Newline,
    /// End of input.
    Eof,
    /// A single `{` beginning an interpolation.
    Interp,
}

/// Whether a string/interpolation closed normally, or was cut short by a line
/// terminator or EOF — in which case an enclosing interpolation must unwind too.
#[derive(PartialEq, Eq)]
pub(super) enum StrOutcome {
    Closed,
    Broken,
}

impl<'a> super::Lexer<'a> {
    /// Scans a double-quoted string at `start` (a `"`), emitting the structured
    /// stream `StrStart (StrText | interpolation)* StrEnd`. `depth` is the
    /// interpolation nesting level.
    pub(super) fn scan_string(&mut self, start: usize, depth: u32) -> StrOutcome {
        debug_assert_eq!(self.bytes.get(start), Some(&b'"'));
        self.emit_len(TokenKind::StrStart, 1);
        // Past the nesting cap, stop treating `{` as interpolation, so no further
        // recursion happens and the body lexes as plain text.
        let interp = depth <= MAX_INTERP_DEPTH;
        if !interp {
            self.error(
                DiagnosticCode::UnterminatedInterpolation,
                Span::new(start as u32, (start + 1) as u32),
                "string interpolation is nested too deeply here",
            );
        }
        loop {
            let text_start = self.pos;
            let boundary = self.scan_text_run(interp, false, false);
            if self.pos > text_start {
                self.emit(
                    TokenKind::StrText,
                    Span::new(text_start as u32, self.pos as u32),
                );
            }
            match boundary {
                Boundary::Close => {
                    self.emit_len(TokenKind::StrEnd, 1);
                    return StrOutcome::Closed;
                }
                Boundary::Newline | Boundary::Eof => {
                    self.error(
                        DiagnosticCode::UnterminatedString,
                        Span::new(start as u32, self.pos as u32),
                        "this string is never closed",
                    );
                    self.close_synthetic(TokenKind::StrEnd);
                    return StrOutcome::Broken;
                }
                Boundary::Interp => {
                    if self.scan_interp(depth) == StrOutcome::Broken {
                        self.close_synthetic(TokenKind::StrEnd);
                        return StrOutcome::Broken;
                    }
                }
            }
        }
    }

    /// Scans a bytes literal `b"…"`; `start` is the `b`. Emits one `Bytes` token.
    pub(super) fn scan_bytes(&mut self, start: usize) {
        self.pos = start + 2; // past `b"`
        let boundary = self.scan_text_run(false, true, false);
        let end = match boundary {
            Boundary::Close => self.pos + 1, // include the closing `"`
            _ => {
                self.error(
                    DiagnosticCode::UnterminatedString,
                    Span::new(start as u32, self.pos as u32),
                    "this bytes literal is never closed",
                );
                self.pos
            }
        };
        self.emit(TokenKind::Bytes, Span::new(start as u32, end as u32));
    }

    /// Scans a triple-quoted string at `start` (the first of three `"`),
    /// emitting the same structured stream as a plain string with S-3 margin
    /// stripping per physical line and inter-line `\n` joins as `StrText` chunks
    /// (§3.6.4). Value decoding stays with the parser (M1.6).
    pub(super) fn scan_triple_string(&mut self, start: usize) {
        self.emit_len(TokenKind::StrStart, 3); // opening `"""`; pos = start + 3

        // The opening `"""` must be the last token on its line — only whitespace
        // may follow; the contents begin on the next line.
        let mut p = self.pos;
        while matches!(self.bytes.get(p), Some(b' ' | b'\t' | b'\r')) {
            p += 1;
        }
        if !matches!(self.bytes.get(p), Some(b'\n') | None) {
            // `p` is a code-point boundary (only ASCII whitespace was skipped);
            // span the whole offending character.
            let end = p + self.char_at(p).len_utf8();
            self.error(
                DiagnosticCode::MalformedTripleQuote,
                Span::new(p as u32, end as u32),
                "nothing may follow the opening `\"\"\"` — the contents start on the next line",
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

    /// Scans literal text (escapes included) until a boundary, leaving `self.pos`
    /// on the boundary byte (unconsumed). `interp` enables `{…}`; `ascii_only`
    /// (bytes) reports non-ASCII source and selects the bytes escape rules;
    /// `triple` treats `"` as literal content (a triple-quoted body ends at a
    /// line-initial `"""`, found separately, not at any `"`).
    fn scan_text_run(&mut self, interp: bool, ascii_only: bool, triple: bool) -> Boundary {
        loop {
            match self.bytes.get(self.pos).copied() {
                None => return Boundary::Eof,
                Some(b'"') if !triple => return Boundary::Close,
                Some(b'\n') => return Boundary::Newline,
                Some(b'\\') => {
                    let esc = escape::scan_escape(self.source, self.pos, ascii_only);
                    if let Some(e) = esc.error {
                        self.error(e.code, e.span, e.message);
                    }
                    self.pos = esc.end;
                }
                Some(b'{') if interp => {
                    if self.bytes.get(self.pos + 1) == Some(&b'{') {
                        self.pos += 2; // `{{` — a literal brace
                    } else {
                        return Boundary::Interp;
                    }
                }
                Some(b'}') if interp && self.bytes.get(self.pos + 1) == Some(&b'}') => {
                    self.pos += 2; // `}}` — a literal brace
                }
                Some(c) if ascii_only && c > 0x7F => {
                    let ch = self.char_at(self.pos);
                    let end = self.pos + ch.len_utf8();
                    self.error(
                        DiagnosticCode::NonAsciiBytes,
                        Span::new(self.pos as u32, end as u32),
                        "a bytes literal must be ASCII — use `\\xHH` for a byte",
                    );
                    self.pos = end;
                }
                Some(_) => {
                    // An ordinary character (a lone `}` is literal text, per the
                    // CHAR grammar); advance one whole code point.
                    self.pos += self.char_at(self.pos).len_utf8();
                }
            }
        }
    }

    /// Scans an interpolation `{ expression }` at `self.pos` (a `{`): emits
    /// `InterpStart`, the expression's tokens, then `InterpEnd`.
    fn scan_interp(&mut self, depth: u32) -> StrOutcome {
        let brace = self.pos;
        self.emit_len(TokenKind::InterpStart, 1);
        // Track whether the body held any real content — a token, a nested
        // brace/string, or even a lexically bad character (which `scan_token`
        // consumes while emitting a diagnostic but no token). Counting tokens
        // alone would misread `{ @ }` as empty and pile a bogus "empty" error
        // on top of the real one.
        let mut saw_content = false;
        // A nested `{`/`(`/`[` inside the expression must not leak into the
        // enclosing string's continuation state; isolate and restore it.
        let saved_brackets = self.bracket_depth;
        let mut brace_depth = 0u32;
        let outcome = loop {
            // Whitespace only, not comments: inside an interpolation a `#` is a
            // diagnostic (S-50), not a comment — otherwise it would run to end
            // of line and swallow the closing `}`.
            self.skip_spaces();
            match self.bytes.get(self.pos).copied() {
                None => {
                    break self.unterminated_interp(brace, "this interpolation is never closed");
                }
                Some(b'\n') => {
                    break self.unterminated_interp(
                        brace,
                        "an interpolation can't span lines — close it with `}`",
                    );
                }
                Some(b'}') if brace_depth == 0 => {
                    if !saw_content {
                        self.error(
                            DiagnosticCode::EmptyInterpolation,
                            Span::new(brace as u32, (self.pos + 1) as u32),
                            "this interpolation is empty — put an expression here, \
                             or `{{` for a literal brace",
                        );
                    }
                    self.emit_len(TokenKind::InterpEnd, 1);
                    break StrOutcome::Closed;
                }
                Some(b'}') => {
                    saw_content = true;
                    brace_depth -= 1;
                    self.emit_len(TokenKind::RBrace, 1);
                }
                Some(b'{') => {
                    saw_content = true;
                    brace_depth += 1;
                    self.emit_len(TokenKind::LBrace, 1);
                }
                Some(b'"') => {
                    saw_content = true;
                    if self.scan_string(self.pos, depth + 1) == StrOutcome::Broken {
                        break self
                            .unterminated_interp(brace, "this interpolation is never closed");
                    }
                }
                Some(b'#') => {
                    saw_content = true;
                    self.comment_in_interp();
                }
                Some(c) => {
                    saw_content = true;
                    self.scan_token(c);
                }
            }
        };
        self.bracket_depth = saved_brackets;
        outcome
    }

    /// Reports a `#` inside an interpolation (S-50) and recovers by skipping the
    /// would-be comment up to — but not consuming — the closing `}`, a newline,
    /// or EOF, so the interpolation can still close on this line. The error
    /// points at the `#`; the skipped run is ASCII-terminated (`}`/`\n`), so
    /// `self.pos` lands on a code-point boundary.
    fn comment_in_interp(&mut self) {
        let hash = self.pos;
        self.error(
            DiagnosticCode::CommentInInterpolation,
            Span::new(hash as u32, (hash + 1) as u32),
            "a comment can't appear inside a string's `{…}` — move it outside, \
             or bind the value to a name first",
        );
        while !matches!(self.bytes.get(self.pos), None | Some(b'\n') | Some(b'}')) {
            self.pos += 1;
        }
    }

    /// Reports an interpolation cut short by EOF or a newline, closes it with a
    /// synthetic `InterpEnd`, and signals the break upward.
    fn unterminated_interp(&mut self, brace: usize, message: &'static str) -> StrOutcome {
        self.error(
            DiagnosticCode::UnterminatedInterpolation,
            Span::new(brace as u32, self.pos as u32),
            message,
        );
        self.close_synthetic(TokenKind::InterpEnd);
        StrOutcome::Broken
    }

    /// Emits a zero-width closing token at `self.pos`, keeping the structured
    /// stream balanced after a broken (unterminated) string or interpolation.
    fn close_synthetic(&mut self, kind: TokenKind) {
        self.emit(kind, Span::new(self.pos as u32, self.pos as u32));
    }
}
