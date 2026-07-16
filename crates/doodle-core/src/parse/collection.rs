//! List and dict literals (L§4.7/§4.8): `[a, b, c]` and `{ k: v }`, each with an
//! optional trailing comma and an empty form (`[]`, `{}` — note `{}` is the
//! empty dict, since a block is `do … end`, not `{ … }`). A bare-word dict key
//! is a string key; a computed key is an expression followed by `:`.

use crate::ast::{DictEntry, DictKey, Node, NodeId};
use crate::lex::TokenKind;
use crate::span::Span;

impl super::Parser<'_> {
    /// A list literal `[ … ]` — the cursor is at `[`.
    pub(super) fn list_lit(&mut self) -> NodeId {
        let start = self.peek_span().start;
        self.advance(); // `[`
        // The `]` delimits any inner block, so elements parse with block
        // arguments enabled even inside a construct header (S-4, §6.4).
        let elems = self.delimited(|p| {
            let mut elems = Vec::new();
            while !matches!(p.peek_kind(), Some(TokenKind::RBracket) | None) {
                elems.push(p.expr(0));
                if !p.eat_comma() {
                    break;
                }
            }
            elems
        });
        let end = self.expect_close(TokenKind::RBracket, "expected `]` to close this list");
        self.push(Node::List(elems), Span::new(start, end))
    }

    /// A dict literal `{ … }` — the cursor is at `{`.
    pub(super) fn dict_lit(&mut self) -> NodeId {
        let start = self.peek_span().start;
        self.advance(); // `{`
        // The `}` delimits any inner block, so entries parse with block
        // arguments enabled even inside a construct header (S-4, §6.4).
        let entries = self.delimited(|p| {
            let mut entries = Vec::new();
            while !matches!(p.peek_kind(), Some(TokenKind::RBrace) | None) {
                let key = p.dict_key();
                p.expect_colon();
                let value = p.expr(0);
                entries.push(DictEntry { key, value });
                if !p.eat_comma() {
                    break;
                }
            }
            entries
        });
        let end = self.expect_close(TokenKind::RBrace, "expected `}` to close this dict");
        self.push(Node::Dict(entries), Span::new(start, end))
    }

    /// A dict key: a bare `IDENT :` is a string key; anything else is a computed
    /// key expression.
    fn dict_key(&mut self) -> DictKey {
        if self.at_ident_colon() {
            let name = self.ident_text_at();
            self.advance(); // the identifier
            DictKey::Bare(name)
        } else {
            DictKey::Expr(self.expr(0))
        }
    }

    fn expect_colon(&mut self) {
        if matches!(self.peek_kind(), Some(TokenKind::Colon)) {
            self.advance();
        } else {
            let span = self.peek_span();
            self.error(span, "expected `:` after this dict key");
        }
    }

    /// Consumes a separating comma; returns whether one was present.
    fn eat_comma(&mut self) -> bool {
        if matches!(self.peek_kind(), Some(TokenKind::Comma)) {
            self.advance();
            true
        } else {
            false
        }
    }
}
