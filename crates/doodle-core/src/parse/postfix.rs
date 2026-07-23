//! Postfix operators (L§6.5 level 1, the tightest-binding): member access
//! `object.name`, indexing `object[…]` (L§6.3), and calls `callee(…)` with
//! positional-then-keyword arguments (L§6.4). They chain left-to-right.

use super::stmt::is_end_terminator;
use crate::ast::{Arg, BlockArg, Node, NodeId};
use crate::lex::{Keyword, TokenKind};
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
        let open = self.peek_span();
        self.advance(); // `[`
        // The `]` delimits any inner block, so the index parses with block
        // arguments enabled even inside a construct header (S-4, §6.4).
        let index = self.delimited(|p| p.expr(0));
        let end = self.expect_close(
            TokenKind::RBracket,
            "expected `]` to close this index",
            open,
        );
        let span = Span::new(self.node_start(object), end);
        self.push(Node::Index { object, index }, span)
    }

    fn call(&mut self, callee: NodeId) -> NodeId {
        let open = self.peek_span();
        self.advance(); // `(`
        // The `)` delimits any block passed inside the argument list, so
        // arguments parse with block arguments enabled even inside a construct
        // header (S-4, §6.4).
        let args = self.delimited(Self::call_args);
        let mut end = self.expect_close(TokenKind::RParen, "expected `)` to close this call", open);
        // A trailing `do … end` is a block argument to this call (§6.4/§8.5),
        // unless we are in a construct header (no-trailing-block mode, S-4),
        // where the `do` opens the construct's body instead.
        let block = if !self.no_block_arg
            && matches!(self.peek_kind(), Some(TokenKind::Keyword(Keyword::Do)))
        {
            let (block, block_end) = self.block_arg();
            end = block_end;
            Some(block)
        } else {
            None
        };
        let span = Span::new(self.node_start(callee), end);
        self.push(
            Node::Call {
                callee,
                args,
                block,
            },
            span,
        )
    }

    /// Parses the argument list between `(` and `)` (positional then keyword,
    /// L§6.4); the cursor starts just past `(` and stops at `)`/end-of-input.
    fn call_args(&mut self) -> Vec<Arg> {
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
        args
    }

    /// Parses a trailing block argument `do ( '(' params ')' )? body 'end'`
    /// (§6.4/§8.5); the cursor is at `do`. Returns the block and the offset just
    /// past its closing `end`.
    fn block_arg(&mut self) -> (BlockArg, u32) {
        let open = self.peek_span();
        self.advance(); // `do`
        let params = if matches!(self.peek_kind(), Some(TokenKind::LParen)) {
            self.block_params()
        } else {
            Vec::new()
        };
        let body = self.block(is_end_terminator);
        let end = self.expect_end_span("do", open);
        (BlockArg { params, body }, end)
    }

    /// Parses a block parameter list `'(' ( IDENT ( ',' IDENT )* )? ')'` (§8.5) —
    /// plain names, no defaults; the cursor is at `(`.
    fn block_params(&mut self) -> Vec<Box<str>> {
        let open = self.peek_span();
        self.advance(); // `(`
        let mut params = Vec::new();
        loop {
            if matches!(self.peek_kind(), Some(TokenKind::RParen) | None) {
                break;
            }
            let (name, _) = self.expect_name("expected a block parameter name");
            params.push(name);
            if matches!(self.peek_kind(), Some(TokenKind::Comma)) {
                self.advance();
            } else {
                break;
            }
        }
        self.expect_close(
            TokenKind::RParen,
            "expected `)` to close the block parameters",
            open,
        );
        params
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

    /// Consumes the closing delimiter `kind` of a bracketed form opened at `open`,
    /// returning its end offset; on a missing closer reports `message` at `open` —
    /// the opening delimiter, so the caret lands on the unclosed bracket rather
    /// than the blank line at the unexpected token — and returns its start offset.
    pub(super) fn expect_close(&mut self, kind: TokenKind, message: &str, open: Span) -> u32 {
        let span = self.peek_span();
        if self.peek_kind() == Some(kind) {
            self.advance();
            span.end
        } else {
            self.error(open, message);
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
