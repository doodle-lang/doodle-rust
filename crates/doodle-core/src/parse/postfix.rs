//! Postfix operators (L§6.5 level 1, the tightest-binding): member access
//! `object.name`, indexing `object[…]` (L§6.3), and calls `callee(…)` with
//! positional-then-keyword arguments (L§6.4). They chain left-to-right.

use crate::ast::{Arg, Node, NodeId};
use crate::lex::TokenKind;
use crate::span::Span;

impl super::Parser<'_> {
    /// Parses a primary and any trailing postfix operators, left-associatively.
    pub(super) fn postfix_chain(&mut self) -> NodeId {
        let mut node = self.primary();
        loop {
            match self.peek_kind() {
                Some(TokenKind::Dot) => node = self.field_access(node),
                Some(TokenKind::LBracket) => node = self.index_access(node),
                Some(TokenKind::LParen) => node = self.call(node),
                _ => break,
            }
        }
        node
    }

    fn field_access(&mut self, object: NodeId) -> NodeId {
        self.advance(); // `.`
        let (name, name_span) = self.expect_field_name();
        let span = Span::new(self.node_start(object), name_span.end);
        self.push(Node::Field { object, name }, span)
    }

    fn index_access(&mut self, object: NodeId) -> NodeId {
        self.advance(); // `[`
        let index = self.expr(0);
        let end = self.expect_close(TokenKind::RBracket, "expected `]` to close this index");
        let span = Span::new(self.node_start(object), end);
        self.push(Node::Index { object, index }, span)
    }

    fn call(&mut self, callee: NodeId) -> NodeId {
        self.advance(); // `(`
        let mut args = Vec::new();
        let mut saw_keyword = false;
        loop {
            if matches!(self.peek_kind(), Some(TokenKind::RParen) | None) {
                break;
            }
            if self.at_ident_colon() {
                let name = self.ident_text_at();
                self.advance(); // name
                self.advance(); // `:`
                let value = self.expr(0);
                saw_keyword = true;
                args.push(Arg::Keyword { name, value });
            } else {
                let value = self.expr(0);
                if saw_keyword {
                    let span = self.node_span(value);
                    self.error(
                        span,
                        "positional arguments must come before keyword arguments",
                    );
                }
                args.push(Arg::Positional(value));
            }
            if matches!(self.peek_kind(), Some(TokenKind::Comma)) {
                self.advance();
            } else {
                break;
            }
        }
        let end = self.expect_close(TokenKind::RParen, "expected `)` to close this call");
        let span = Span::new(self.node_start(callee), end);
        self.push(Node::Call { callee, args }, span)
    }

    /// Whether the cursor is at `IDENT :` — a keyword argument (or a bare-word
    /// dict key, L§4.8). Unambiguous since `:` is not an expression operator.
    pub(super) fn at_ident_colon(&self) -> bool {
        self.peek_kind() == Some(TokenKind::Ident)
            && self.tokens.get(self.pos + 1).map(|t| t.kind) == Some(TokenKind::Colon)
    }

    /// The identifier text of the token at the cursor (does not advance).
    pub(super) fn ident_text_at(&self) -> Box<str> {
        let span = self.tokens[self.pos].span;
        self.source[span.start as usize..span.end as usize].into()
    }

    fn expect_field_name(&mut self) -> (Box<str>, Span) {
        if self.peek_kind() == Some(TokenKind::Ident) {
            let name = self.ident_text_at();
            let span = self.tokens[self.pos].span;
            self.advance();
            (name, span)
        } else {
            let span = self.peek_span();
            self.error(span, "expected a field name after `.`");
            // Consume the offending token (e.g. the `1` in `a.1`) so it doesn't
            // cascade into a spurious top-level "unexpected input".
            if !matches!(self.peek_kind(), Some(TokenKind::Eof) | None) {
                self.advance();
            }
            (Box::from(""), span)
        }
    }

    /// Consumes a `kind` token, returning its end offset; or reports `message`
    /// and returns the current start offset.
    pub(super) fn expect_close(&mut self, kind: TokenKind, message: &str) -> u32 {
        let span = self.peek_span();
        if self.peek_kind() == Some(kind) {
            self.advance();
            span.end
        } else {
            self.error(span, message);
            span.start
        }
    }

    fn node_start(&self, id: NodeId) -> u32 {
        self.ast.span(id).start
    }

    fn node_span(&self, id: NodeId) -> Span {
        self.ast.span(id)
    }
}
