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
            let boundary = self.scan_text_run(interp, false);
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
        let boundary = self.scan_text_run(false, true);
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

    /// Scans literal text (escapes included) until a boundary, leaving `self.pos`
    /// on the boundary byte (unconsumed). `interp` enables `{…}`; `ascii_only`
    /// (bytes) reports non-ASCII source and selects the bytes escape rules.
    fn scan_text_run(&mut self, interp: bool, ascii_only: bool) -> Boundary {
        loop {
            match self.bytes.get(self.pos).copied() {
                None => return Boundary::Eof,
                Some(b'"') => return Boundary::Close,
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
