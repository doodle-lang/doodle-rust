//! The block-bodied constructs: `if`/`try` (which are expressions as well as
//! statements, L§6.8/§6.9/§7.5/§7.9) and the loop/`with` statements
//! (L§7.6/§7.7/§5.5). Each parses a header, one or more `body` blocks, and a
//! closing `end`.
//!
//! S-4: a construct's header expression is parsed in no-trailing-block mode — a
//! `do … end` after the header opens the construct's body, never a block
//! argument to a call in the header. With block arguments unimplemented (M1.8),
//! that already holds (nothing here consumes a trailing `do`); the leftover-`do`
//! confusion is diagnosed in [`super::Parser::stray_do`](super::Parser).

use super::stmt::is_end_terminator;
use crate::ast::{IfArm, Node, NodeId};
use crate::lex::{Keyword, TokenKind};
use crate::span::Span;

impl super::Parser<'_> {
    /// An `if` (L§6.8/§7.5): `if cond then body (else if cond then body)*
    /// (else body)? end`, with `else if` flattened into the arm list.
    pub(super) fn if_expr(&mut self) -> NodeId {
        let start = self.peek_span().start;
        self.advance(); // `if`
        let mut arms = Vec::new();
        let else_body = loop {
            let cond = self.expr(0);
            self.expect_then();
            let body = self.block(is_else_or_end);
            arms.push(IfArm { cond, body });
            if !matches!(self.peek_kind(), Some(TokenKind::Keyword(Keyword::Else))) {
                break None; // next must be `end`
            }
            self.advance(); // `else`
            if matches!(self.peek_kind(), Some(TokenKind::Keyword(Keyword::If))) {
                self.advance(); // `else if` → another arm
                continue;
            }
            break Some(self.block(is_end_terminator));
        };
        let end = self.expect_end_span("if");
        self.push(Node::If { arms, else_body }, Span::new(start, end))
    }

    /// A `try` (L§6.9/§12.2): `try body rescue name handler end`.
    pub(super) fn try_expr(&mut self) -> NodeId {
        let start = self.peek_span().start;
        self.advance(); // `try`
        let body = self.block(is_rescue);
        self.expect_rescue();
        let (rescue_name, _) =
            self.expect_name("expected a name for the caught error after `rescue`");
        let rescue_body = self.block(is_end_terminator);
        let end = self.expect_end_span("try");
        self.push(
            Node::Try {
                body,
                rescue_name,
                rescue_body,
            },
            Span::new(start, end),
        )
    }

    /// A `while` loop (L§7.6): `while cond do body end`.
    pub(super) fn while_stmt(&mut self) -> NodeId {
        let start = self.peek_span().start;
        self.advance(); // `while`
        let cond = self.expr(0);
        self.expect_do("while");
        let body = self.block(is_end_terminator);
        let end = self.expect_end_span("while");
        self.push(Node::While { cond, body }, Span::new(start, end))
    }

    /// A `loop` (L§7.7): `loop do body end`.
    pub(super) fn loop_stmt(&mut self) -> NodeId {
        let start = self.peek_span().start;
        self.advance(); // `loop`
        self.expect_do("loop");
        let body = self.block(is_end_terminator);
        let end = self.expect_end_span("loop");
        self.push(Node::Loop { body }, Span::new(start, end))
    }

    /// A `with` (L§5.5): `with name = value do body end`.
    pub(super) fn with_stmt(&mut self) -> NodeId {
        let start = self.peek_span().start;
        self.advance(); // `with`
        let (name, _) = self.expect_name("expected a dynamic-parameter name after `with`");
        self.expect_eq("with");
        let value = self.expr(0);
        self.expect_do("with");
        let body = self.block(is_end_terminator);
        let end = self.expect_end_span("with");
        self.push(Node::With { name, value, body }, Span::new(start, end))
    }

    fn expect_then(&mut self) {
        if matches!(self.peek_kind(), Some(TokenKind::Keyword(Keyword::Then))) {
            self.advance();
        } else {
            let span = self.peek_span();
            self.error(span, "expected `then` after the `if` condition");
        }
    }

    fn expect_do(&mut self, what: &str) {
        if matches!(self.peek_kind(), Some(TokenKind::Keyword(Keyword::Do))) {
            self.advance();
        } else {
            let span = self.peek_span();
            self.error(span, &format!("expected `do` to open the `{what}` body"));
        }
    }

    fn expect_rescue(&mut self) {
        if matches!(self.peek_kind(), Some(TokenKind::Keyword(Keyword::Rescue))) {
            self.advance();
        } else {
            let span = self.peek_span();
            self.error(span, "expected `rescue` to begin the handler in this `try`");
        }
    }
}

/// An `if`-arm body terminator: `else` (the next arm or the final else) or `end`.
fn is_else_or_end(k: TokenKind) -> bool {
    matches!(k, TokenKind::Keyword(Keyword::Else | Keyword::End))
}

/// A `try` protected-body terminator: `rescue`.
fn is_rescue(k: TokenKind) -> bool {
    matches!(k, TokenKind::Keyword(Keyword::Rescue))
}
