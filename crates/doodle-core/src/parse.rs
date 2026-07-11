//! The parser (L§6): a Pratt expression parser over the lexer's token stream,
//! producing the [`Ast`] arena. Numeric value lowering (digits → `i64`/bignum/
//! `f64`) happens here — the lexer validated only shape (M1.3).
//!
//! M1.6a–b cover the operator-precedence tower (L§6.5) — literal/identifier
//! primaries, prefix `-`/`+`/`not`, the nine binary levels with right-associative
//! `**` and non-associative comparisons (`a < b < c` is a static error), and
//! parenthesized grouping — plus string and bytes literals, with escape decoding
//! and `{ … }` interpolation assembled from the lexer's structured stream (the
//! `decode` submodule). Calls/postfix, list/dict literals, and the `if`/`try`/
//! anonymous-`fn` forms arrive in later M1.6 pieces.

mod decode;

use crate::ast::{Ast, BinaryOp, Node, NodeId, StrPart, UnaryOp};
use crate::diag::Diagnostic;
use crate::diag::code::DiagnosticCode;
use crate::lex::{Keyword, Lexed, TokenKind, lex};
use crate::span::{ModuleId, Span};

/// The result of parsing an expression: the arena, its root, and diagnostics.
#[derive(Clone, Debug)]
pub struct Parsed {
    /// The AST arena.
    pub ast: Ast,
    /// The parsed expression's root node.
    pub root: NodeId,
    /// Lexical and syntactic diagnostics, in source order.
    pub diagnostics: Vec<Diagnostic>,
}

/// Lexes and parses `source` (which must be load-normalized, see
/// [`crate::source::normalize`]) as a single expression.
#[must_use]
pub fn parse_expression(source: &str, module: ModuleId) -> Parsed {
    let Lexed {
        tokens,
        diagnostics,
    } = lex(source, module);
    let mut p = Parser {
        source,
        tokens: &tokens,
        pos: 0,
        ast: Ast::new(),
        diagnostics,
        module,
        depth: 0,
        bailed: false,
    };
    p.skip_newlines();
    let root = p.expr(0);
    p.skip_newlines();
    if !matches!(p.peek_kind(), Some(TokenKind::Eof) | None) {
        let span = p.peek_span();
        p.error(span, "unexpected input after the expression");
    }
    Parsed {
        ast: p.ast,
        root,
        diagnostics: p.diagnostics,
    }
}

// Binding powers, higher = binds tighter, matching the L§6.5 precedence table.
const BP_OR: u8 = 10;
const BP_AND: u8 = 20;
const BP_NOT: u8 = 30;
const BP_COMPARE: u8 = 40;
const BP_ADD: u8 = 50;
const BP_MUL: u8 = 60;
const BP_UNARY: u8 = 70;
const BP_POW: u8 = 80;

/// Bounds parser recursion so deeply nested input (`((((…))))`, long unary or
/// `**` chains) can't overflow the stack — an uncatchable abort in a
/// host-embedded, fuzzed engine. Far above any real expression's nesting.
const MAX_DEPTH: u32 = 256;

struct Parser<'a> {
    source: &'a str,
    tokens: &'a [crate::lex::Token],
    pos: usize,
    ast: Ast,
    diagnostics: Vec<Diagnostic>,
    module: ModuleId,
    /// Current expression-nesting depth (see [`MAX_DEPTH`]).
    depth: u32,
    /// Set once the depth limit is hit; suppresses the cascade of follow-on
    /// diagnostics while the over-deep call stack unwinds.
    bailed: bool,
}

impl Parser<'_> {
    /// Parses an expression whose operators all bind at least as tightly as
    /// `min_bp` (precedence climbing), under the recursion-depth guard.
    fn expr(&mut self, min_bp: u8) -> NodeId {
        if self.bailed {
            return self.push(Node::Error, self.peek_span());
        }
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            let span = self.peek_span();
            self.error(span, "this expression is nested too deeply");
            self.bailed = true; // suppress the unwinding cascade
            return self.push(Node::Error, span);
        }
        let result = self.expr_climb(min_bp);
        self.depth -= 1;
        result
    }

    fn expr_climb(&mut self, min_bp: u8) -> NodeId {
        let mut lhs = self.prefix();
        // Comparisons are non-associative (L§6.5): a *second* comparison at this
        // same climb level is `a < b < c`, a static error. Tracking it per level
        // (not by node shape) keeps a parenthesized comparison — `(a == b) == c`,
        // which is valid since `==` is total (L§6.6) — from being misflagged.
        let mut saw_comparison = false;
        while let Some((lbp, rbp, op)) = self.peek_kind().and_then(infix_bp) {
            if lbp < min_bp || self.bailed {
                break;
            }
            if is_comparison(op) {
                if saw_comparison {
                    let span = self.peek_span();
                    self.error_code(
                        DiagnosticCode::ChainedComparison,
                        span,
                        "comparisons don't chain — write `a < b and b < c`",
                    );
                }
                saw_comparison = true;
            }
            self.advance();
            let rhs = self.expr(rbp);
            let span = Span::new(self.ast.span(lhs).start, self.ast.span(rhs).end);
            lhs = self.push(Node::Binary { op, lhs, rhs }, span);
        }
        lhs
    }

    /// Parses a prefix operator application or a primary.
    fn prefix(&mut self) -> NodeId {
        match self.peek_kind() {
            Some(TokenKind::Minus) => self.unary(UnaryOp::Neg, BP_UNARY),
            Some(TokenKind::Plus) => self.unary(UnaryOp::Pos, BP_UNARY),
            Some(TokenKind::Keyword(Keyword::Not)) => self.unary(UnaryOp::Not, BP_NOT),
            _ => self.primary(),
        }
    }

    fn unary(&mut self, op: UnaryOp, operand_bp: u8) -> NodeId {
        let op_start = self.peek_span().start;
        self.advance();
        let operand = self.expr(operand_bp);
        let span = Span::new(op_start, self.ast.span(operand).end);
        self.push(Node::Unary { op, operand }, span)
    }

    fn primary(&mut self) -> NodeId {
        let Some(tok) = self.tokens.get(self.pos).copied() else {
            let span = self.peek_span();
            self.error(span, "expected an expression");
            return self.push(Node::Error, span);
        };
        let span = tok.span;
        match tok.kind {
            TokenKind::Int => {
                self.advance();
                let node = lower_int(self.source, span);
                self.push(node, span)
            }
            TokenKind::Float => {
                self.advance();
                let node = lower_float(self.source, span);
                self.push(node, span)
            }
            TokenKind::Keyword(Keyword::True) => self.keyword_lit(Node::BoolLit(true), span),
            TokenKind::Keyword(Keyword::False) => self.keyword_lit(Node::BoolLit(false), span),
            TokenKind::Keyword(Keyword::Nil) => self.keyword_lit(Node::NilLit, span),
            TokenKind::Ident => {
                self.advance();
                let name = self.source[span.start as usize..span.end as usize].into();
                self.push(Node::Ident(name), span)
            }
            TokenKind::StrStart => self.string_lit(span),
            TokenKind::Bytes => {
                self.advance();
                let node = self.decode_bytes_literal(span);
                self.push(node, span)
            }
            TokenKind::LParen => self.grouping(),
            // These never start an expression and are not consumed, so the
            // caller's loops (and string_lit's interpolation handling) can act
            // on them without a double error.
            TokenKind::Eof | TokenKind::InterpEnd | TokenKind::StrEnd => {
                self.error(span, "expected an expression");
                self.push(Node::Error, span)
            }
            _ => {
                self.advance(); // consume the offending token to make progress
                self.error(span, "expected an expression");
                self.push(Node::Error, span)
            }
        }
    }

    fn keyword_lit(&mut self, node: Node, span: Span) -> NodeId {
        self.advance();
        self.push(node, span)
    }

    /// Parenthesized grouping (transparent — the parens only set precedence).
    fn grouping(&mut self) -> NodeId {
        self.advance(); // `(`
        let inner = self.expr(0);
        if matches!(self.peek_kind(), Some(TokenKind::RParen)) {
            self.advance();
        } else {
            let span = self.peek_span();
            self.error(span, "expected a `)` to close this group");
        }
        inner
    }

    /// Assembles a string literal from the structured stream `StrStart (StrText
    /// | interpolation)* StrEnd`, decoding escapes and parsing each `{ … }`.
    /// Adjacent decoded text (including triple-quoted `\n`-join chunks) merges
    /// into one `Text` part.
    fn string_lit(&mut self, start_span: Span) -> NodeId {
        let source = self.source;
        self.advance(); // StrStart
        let mut parts = Vec::new();
        let mut acc = String::new();
        let mut end = start_span.end;
        loop {
            match self.peek_kind() {
                Some(TokenKind::StrText) => {
                    let sp = self.peek_span();
                    self.advance();
                    let (text, dangling) =
                        decode::decode_text(&source[sp.start as usize..sp.end as usize]);
                    if let Some(off) = dangling {
                        let at = sp.start + off as u32;
                        self.error(
                            Span::new(at, at + 1),
                            "a backslash here isn't a valid escape",
                        );
                    }
                    acc.push_str(&text);
                }
                Some(TokenKind::InterpStart) => {
                    flush_text(&mut parts, &mut acc);
                    self.advance();
                    // An empty interpolation was already diagnosed by the lexer;
                    // skip it rather than pile on an "expected expression".
                    if matches!(self.peek_kind(), Some(TokenKind::InterpEnd)) {
                        self.advance();
                    } else {
                        let expr = self.expr(0);
                        if matches!(self.peek_kind(), Some(TokenKind::InterpEnd)) {
                            self.advance();
                        } else {
                            let sp = self.peek_span();
                            self.error(sp, "expected `}` to close this interpolation");
                        }
                        parts.push(StrPart::Interp(expr));
                    }
                }
                Some(TokenKind::StrEnd) => {
                    end = self.peek_span().end;
                    self.advance();
                    break;
                }
                // The lexer always balances StrStart/StrEnd (a synthetic StrEnd
                // even for an unterminated string), so this stops rather than
                // loops on an otherwise-impossible malformed stream.
                _ => break,
            }
        }
        flush_text(&mut parts, &mut acc);
        self.push(Node::StrLit(parts), Span::new(start_span.start, end))
    }

    /// Decodes a bytes literal `b"…"` to its byte sequence.
    fn decode_bytes_literal(&mut self, span: Span) -> Node {
        let text = &self.source[span.start as usize..span.end as usize];
        let inner = text.strip_prefix("b\"").unwrap_or(text);
        let (bytes, dangling) = decode::decode_bytes(inner);
        if let Some(off) = dangling {
            let at = span.start + 2 + off as u32; // past the `b"` prefix
            self.error(
                Span::new(at, at + 1),
                "a backslash here isn't a valid escape",
            );
        }
        Node::BytesLit(bytes)
    }

    fn peek_kind(&self) -> Option<TokenKind> {
        self.tokens.get(self.pos).map(|t| t.kind)
    }

    fn peek_span(&self) -> Span {
        self.tokens
            .get(self.pos)
            .or_else(|| self.tokens.last())
            .map_or(Span::new(0, 0), |t| t.span)
    }

    fn advance(&mut self) {
        self.pos += 1;
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek_kind(), Some(TokenKind::Newline)) {
            self.advance();
        }
    }

    fn push(&mut self, node: Node, span: Span) -> NodeId {
        self.ast.push(node, span)
    }

    fn error(&mut self, span: Span, message: &str) {
        self.error_code(DiagnosticCode::SyntaxError, span, message);
    }

    fn error_code(&mut self, code: DiagnosticCode, span: Span, message: &str) {
        // Once bailed (depth limit), stay silent so one over-deep expression
        // yields one diagnostic, not one per unwinding frame.
        if self.bailed {
            return;
        }
        self.diagnostics
            .push(Diagnostic::error(code, self.module, span, message));
    }
}

/// Flushes accumulated decoded text into a `Text` part, if any.
fn flush_text(parts: &mut Vec<StrPart>, acc: &mut String) {
    if !acc.is_empty() {
        parts.push(StrPart::Text(std::mem::take(acc).into()));
    }
}

fn infix_bp(kind: TokenKind) -> Option<(u8, u8, BinaryOp)> {
    use BinaryOp as B;
    use TokenKind as T;
    let (lbp, rbp, op) = match kind {
        T::Keyword(Keyword::Or) => (BP_OR, BP_OR + 1, B::Or),
        T::Keyword(Keyword::And) => (BP_AND, BP_AND + 1, B::And),
        T::Lt => (BP_COMPARE, BP_COMPARE + 1, B::Lt),
        T::Gt => (BP_COMPARE, BP_COMPARE + 1, B::Gt),
        T::Le => (BP_COMPARE, BP_COMPARE + 1, B::Le),
        T::Ge => (BP_COMPARE, BP_COMPARE + 1, B::Ge),
        T::EqEq => (BP_COMPARE, BP_COMPARE + 1, B::Eq),
        T::BangEq => (BP_COMPARE, BP_COMPARE + 1, B::Ne),
        T::Keyword(Keyword::Is) => (BP_COMPARE, BP_COMPARE + 1, B::Is),
        T::Plus => (BP_ADD, BP_ADD + 1, B::Add),
        T::Minus => (BP_ADD, BP_ADD + 1, B::Sub),
        T::Star => (BP_MUL, BP_MUL + 1, B::Mul),
        T::Slash => (BP_MUL, BP_MUL + 1, B::Div),
        T::SlashSlash => (BP_MUL, BP_MUL + 1, B::FloorDiv),
        T::Percent => (BP_MUL, BP_MUL + 1, B::Rem),
        // `**` is right-associative: a lower right binding power lets a same-level
        // `**` on the right win, so `2 ** 3 ** 2` is `2 ** (3 ** 2)`.
        T::StarStar => (BP_POW, BP_POW - 1, B::Pow),
        _ => return None,
    };
    Some((lbp, rbp, op))
}

fn is_comparison(op: BinaryOp) -> bool {
    use BinaryOp::{Eq, Ge, Gt, Is, Le, Lt, Ne};
    matches!(op, Eq | Ne | Lt | Gt | Le | Ge | Is)
}

/// Lowers an integer-literal token's text to [`Node::IntLit`] (or
/// [`Node::BigIntLit`] on `i64` overflow). The lexer guarantees the shape, so
/// only overflow can fail the radix parse.
fn lower_int(source: &str, span: Span) -> Node {
    let text = &source[span.start as usize..span.end as usize];
    let cleaned: String = text.chars().filter(|c| *c != '_').collect();
    let (radix, digits) = match cleaned.as_bytes() {
        [b'0', b'x', ..] => (16u32, &cleaned[2..]),
        [b'0', b'b', ..] => (2, &cleaned[2..]),
        [b'0', b'o', ..] => (8, &cleaned[2..]),
        _ => (10, cleaned.as_str()),
    };
    if digits.is_empty() {
        return Node::Error; // a malformed literal (already lexer-diagnosed)
    }
    match i64::from_str_radix(digits, radix) {
        Ok(n) => Node::IntLit(n),
        // The lexer validated the digits for the radix, so the only parse
        // failure here is `i64` overflow — a genuine bignum.
        Err(_) => Node::BigIntLit {
            radix: radix as u8,
            digits: digits.into(),
        },
    }
}

/// Lowers a float-literal token's text to [`Node::FloatLit`].
fn lower_float(source: &str, span: Span) -> Node {
    let text = &source[span.start as usize..span.end as usize];
    let cleaned: String = text.chars().filter(|c| *c != '_').collect();
    // A malformed float (already lexer-diagnosed) recovers to Error rather than
    // a degenerate NaN literal.
    cleaned.parse().map(Node::FloatLit).unwrap_or(Node::Error)
}
