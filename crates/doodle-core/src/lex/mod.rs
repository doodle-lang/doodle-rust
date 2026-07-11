//! The lexer (L§3): turns NFC-normalized source into a token stream and lexical
//! diagnostics. Value lowering (bignum/float parsing) is the parser's job
//! (M1.6); the lexer validates lexical shape and records spans only.

mod escape;
mod number;
mod string;
pub mod token;

pub use token::{Keyword, Token, TokenKind};

use crate::diag::Diagnostic;
use crate::diag::code::DiagnosticCode;
use crate::span::{ModuleId, Span};
use crate::unicode;

/// The result of lexing a module's source.
#[derive(Clone, Debug)]
pub struct Lexed {
    /// The token stream, terminated by [`TokenKind::Eof`].
    pub tokens: Vec<Token>,
    /// Lexical diagnostics (malformed numbers, unterminated strings, unexpected
    /// characters).
    pub diagnostics: Vec<Diagnostic>,
}

/// Lexes `source` for `module` into tokens + diagnostics.
///
/// `source` must be **load-normalized** (CRLF→LF and NFC, L§3.1); the lexer
/// counts on LF-only NFC text for correct spans, positions, and identifier
/// equality. Route raw source through [`crate::source::normalize`] first.
#[must_use]
pub fn lex(source: &str, module: ModuleId) -> Lexed {
    debug_assert!(
        unicode::is_nfc(source),
        "lex() requires NFC-normalized source"
    );
    Lexer::new(source, module).run()
}

/// Lexes `source` and returns only its diagnostics — the conformance runner's
/// lex-stage entry, decoupled from the token model. `source` must be
/// load-normalized (see [`lex`]).
#[must_use]
pub fn lex_to_diagnostics(source: &str) -> Vec<Diagnostic> {
    lex(source, ModuleId(0)).diagnostics
}

struct Lexer<'a> {
    source: &'a str,
    bytes: &'a [u8],
    module: ModuleId,
    pos: usize,
    /// Open-bracket nesting depth, for continuation suppression (L§3.2). A
    /// single count across `(`/`[`/`{` is enough for continuation; matching
    /// brackets and diagnosing mismatches is the parser's job (M1.6).
    bracket_depth: u32,
    tokens: Vec<Token>,
    diagnostics: Vec<Diagnostic>,
    /// The most recent non-`Newline` token kind, for the S-2 continuation check.
    last_significant: Option<TokenKind>,
}

impl<'a> Lexer<'a> {
    fn new(source: &'a str, module: ModuleId) -> Self {
        Lexer {
            source,
            bytes: source.as_bytes(),
            module,
            pos: 0,
            bracket_depth: 0,
            tokens: Vec::new(),
            diagnostics: Vec::new(),
            last_significant: None,
        }
    }

    fn run(mut self) -> Lexed {
        loop {
            self.skip_inline();
            let Some(&c) = self.bytes.get(self.pos) else {
                break;
            };
            if c == b'\n' {
                self.newline();
            } else {
                self.scan_token(c);
            }
        }
        let end = self.source.len() as u32;
        self.emit(TokenKind::Eof, Span::new(end, end));
        Lexed {
            tokens: self.tokens,
            diagnostics: self.diagnostics,
        }
    }

    /// Skips inline whitespace and a trailing comment (not the newline). A lone
    /// carriage return (a CRLF is normalized away on load, §3.1) is treated as
    /// whitespace.
    fn skip_inline(&mut self) {
        loop {
            match self.bytes.get(self.pos) {
                Some(b' ' | b'\t' | b'\r') => self.pos += 1,
                Some(b'#') => {
                    while matches!(self.bytes.get(self.pos), Some(b) if *b != b'\n') {
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
    }

    /// Handles a physical `\n`: emit a statement separator unless it is
    /// suppressed inside brackets or after a continuation trigger (S-2), and
    /// collapse leading/consecutive separators.
    fn newline(&mut self) {
        let span = Span::new(self.pos as u32, (self.pos + 1) as u32);
        self.pos += 1;
        if self.bracket_depth > 0 {
            return;
        }
        if self
            .last_significant
            .is_some_and(TokenKind::is_continuation_trigger)
        {
            return;
        }
        match self.tokens.last() {
            None => {}                                    // leading blank lines
            Some(t) if t.kind == TokenKind::Newline => {} // collapse consecutive
            Some(_) => self.tokens.push(Token {
                kind: TokenKind::Newline,
                span,
            }),
        }
    }

    fn scan_token(&mut self, c: u8) {
        let start = self.pos;
        match c {
            b'"' => {
                self.scan_string(start, 0);
            }
            // `b"…"` is a bytes literal; a bare `b` (or `by`, …) is an identifier.
            b'b' if self.bytes.get(start + 1) == Some(&b'"') => self.scan_bytes(start),
            b'0'..=b'9' => self.scan_num(start),
            b';' => self.emit_len(TokenKind::Semicolon, 1),
            b',' => self.emit_len(TokenKind::Comma, 1),
            b'.' => self.emit_len(TokenKind::Dot, 1),
            b':' => self.emit_len(TokenKind::Colon, 1),
            b'+' => self.emit_len(TokenKind::Plus, 1),
            b'-' => self.emit_len(TokenKind::Minus, 1),
            b'%' => self.emit_len(TokenKind::Percent, 1),
            b'(' => self.open(TokenKind::LParen),
            b'[' => self.open(TokenKind::LBracket),
            b'{' => self.open(TokenKind::LBrace),
            b')' => self.close(TokenKind::RParen),
            b']' => self.close(TokenKind::RBracket),
            b'}' => self.close(TokenKind::RBrace),
            b'*' => self.op2(b'*', TokenKind::StarStar, TokenKind::Star),
            b'/' => self.op2(b'/', TokenKind::SlashSlash, TokenKind::Slash),
            b'=' => self.op2(b'=', TokenKind::EqEq, TokenKind::Eq),
            b'<' => self.op2(b'=', TokenKind::Le, TokenKind::Lt),
            b'>' => self.op2(b'=', TokenKind::Ge, TokenKind::Gt),
            b'!' => {
                if self.bytes.get(start + 1) == Some(&b'=') {
                    self.emit_len(TokenKind::BangEq, 2);
                } else {
                    self.unexpected(start);
                }
            }
            _ => {
                let ch = self.char_at(start);
                if unicode::is_ident_start(ch) {
                    self.scan_ident(start);
                } else {
                    self.unexpected(start);
                }
            }
        }
    }

    fn scan_num(&mut self, start: usize) {
        let n = number::scan_number(self.source, start);
        let span = Span::new(start as u32, n.end as u32);
        if let Some(message) = n.error {
            self.error(DiagnosticCode::MalformedNumber, span, message);
        }
        self.emit(n.kind, span);
    }

    fn scan_ident(&mut self, start: usize) {
        let mut end = start;
        for ch in self.source[start..].chars() {
            if unicode::is_ident_continue(ch) {
                end += ch.len_utf8();
            } else {
                break;
            }
        }
        let kind = match token::keyword(&self.source[start..end]) {
            Some(kw) => TokenKind::Keyword(kw),
            None => TokenKind::Ident,
        };
        self.emit(kind, Span::new(start as u32, end as u32));
    }

    fn unexpected(&mut self, start: usize) {
        let end = start + self.char_at(start).len_utf8();
        self.error(
            DiagnosticCode::UnexpectedCharacter,
            Span::new(start as u32, end as u32),
            "this character can't start a token here",
        );
        self.pos = end; // recover: skip the bad character, emit no token
    }

    fn open(&mut self, kind: TokenKind) {
        self.bracket_depth += 1;
        self.emit_len(kind, 1);
    }

    fn close(&mut self, kind: TokenKind) {
        self.bracket_depth = self.bracket_depth.saturating_sub(1);
        self.emit_len(kind, 1);
    }

    /// Emits a two-byte token if the byte after the cursor is `second`, else a
    /// one-byte token.
    fn op2(&mut self, second: u8, if_two: TokenKind, if_one: TokenKind) {
        if self.bytes.get(self.pos + 1) == Some(&second) {
            self.emit_len(if_two, 2);
        } else {
            self.emit_len(if_one, 1);
        }
    }

    fn emit_len(&mut self, kind: TokenKind, len: usize) {
        let s = self.pos as u32;
        self.emit(kind, Span::new(s, s + len as u32));
    }

    fn emit(&mut self, kind: TokenKind, span: Span) {
        self.tokens.push(Token { kind, span });
        self.last_significant = Some(kind);
        self.pos = span.end as usize;
    }

    fn error(&mut self, code: DiagnosticCode, span: Span, message: &str) {
        self.diagnostics
            .push(Diagnostic::error(code, self.module, span, message));
    }

    /// The `char` at byte offset `off` (a char boundary by construction).
    fn char_at(&self, off: usize) -> char {
        self.source[off..]
            .chars()
            .next()
            .expect("off is within source")
    }
}
