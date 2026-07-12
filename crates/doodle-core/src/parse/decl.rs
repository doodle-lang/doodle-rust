//! Callable declarations and the shared parameter grammar (L§8.1/§8.2/§6.10):
//! named `to`/`fn` declarations and anonymous `fn` expressions, all built from
//! one `param_list` helper. A block parameter `do name` (§8.5) may appear at
//! most once and must be last.
//!
//! Named `to`/`fn` are statements (dispatched from `stmt`); an anonymous `fn`
//! is an expression primary (dispatched from `parse::Parser::primary`). Records,
//! protocols, and modules — which reuse `param_list` for their members — arrive
//! in later M1.8 pieces; call-site block *arguments* are M1.9.

use super::stmt::is_end_terminator;
use crate::ast::{CallableKind, Node, NodeId, Param};
use crate::lex::{Keyword, TokenKind};
use crate::span::Span;

impl super::Parser<'_> {
    /// A named declaration `to name(params) body end` or `fn name(params) body
    /// end` (L§8.1). The docstring is captured in M1.8c.
    pub(super) fn callable_decl(&mut self, kind: CallableKind) -> NodeId {
        let start = self.peek_span().start;
        self.advance(); // `to` / `fn`
        let (name, _) = self.expect_name(match kind {
            CallableKind::Proc => "expected a name after `to`",
            CallableKind::Func => "expected a name after `fn`",
        });
        let params = self.param_list();
        let body = self.block(is_end_terminator);
        let end = self.expect_end_span(kind_word(kind));
        self.push(
            Node::Callable {
                kind,
                name: Some(name),
                params,
                body,
                doc: None,
            },
            Span::new(start, end),
        )
    }

    /// An anonymous function `fn(params) body end` (L§6.10) — a first-class
    /// function value in expression position.
    pub(super) fn anon_fn(&mut self) -> NodeId {
        let start = self.peek_span().start;
        self.advance(); // `fn`
        let params = self.param_list();
        let body = self.block(is_end_terminator);
        let end = self.expect_end_span("fn");
        self.push(
            Node::Callable {
                kind: CallableKind::Func,
                name: None,
                params,
                body,
                doc: None,
            },
            Span::new(start, end),
        )
    }

    /// Parses `'(' params? ')'`, returning the parameters. On a missing `(` it
    /// reports and returns no parameters (the body parse then recovers). Shared
    /// with protocol members (typedecl).
    pub(super) fn param_list(&mut self) -> Vec<Param> {
        if !matches!(self.peek_kind(), Some(TokenKind::LParen)) {
            let span = self.peek_span();
            self.error(span, "expected `(` to begin the parameter list");
            return Vec::new();
        }
        self.advance(); // `(`
        let mut params = Vec::new();
        let mut saw_block = false;
        while !matches!(self.peek_kind(), Some(TokenKind::RParen) | None) {
            let param_span = self.peek_span();
            let param = self.param();
            // A block parameter must be the last parameter (L§8.2): flag a block
            // param that is not last, and a second block param.
            if saw_block {
                self.error(
                    param_span,
                    "the block parameter `do …` must be the last parameter",
                );
            }
            if matches!(param, Param::Block { .. }) {
                saw_block = true;
            }
            params.push(param);
            if !matches!(self.peek_kind(), Some(TokenKind::Comma)) {
                break;
            }
            self.advance(); // `,`
        }
        self.expect_close(
            TokenKind::RParen,
            "expected `)` to close the parameter list",
        );
        params
    }

    /// One parameter: `do name` (block), or `name` / `name = default` (ordinary).
    fn param(&mut self) -> Param {
        if matches!(self.peek_kind(), Some(TokenKind::Keyword(Keyword::Do))) {
            self.advance(); // `do`
            let (name, _) = self.expect_name("expected a name after `do` for the block parameter");
            return Param::Block { name };
        }
        let (name, _) = self.expect_name("expected a parameter name");
        let default = if matches!(self.peek_kind(), Some(TokenKind::Eq)) {
            self.advance(); // `=`
            Some(self.expr(0))
        } else {
            None
        };
        Param::Ordinary { name, default }
    }
}

/// The keyword that introduces a callable of `kind`, for diagnostics.
fn kind_word(kind: CallableKind) -> &'static str {
    match kind {
        CallableKind::Proc => "to",
        CallableKind::Func => "fn",
    }
}
