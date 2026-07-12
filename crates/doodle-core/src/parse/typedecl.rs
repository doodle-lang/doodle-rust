//! Type and protocol declarations (L§9.1/§10.1/§10.2): `record`/`ref record`,
//! `protocol`/`extends`, and `implement … for …`.
//!
//! A record carries fields and an optional docstring-only body. A protocol
//! carries member signatures — required (no body) or default (with a body). An
//! `implement` block carries `to`/`fn` method declarations, reusing
//! `callable_decl`. All three are module-level declarations (L§7.1); the
//! placement rule is enforced in M1.8c.

use super::stmt::is_end_terminator;
use crate::ast::{CallableKind, Node, NodeId, ProtoMember};
use crate::lex::{Keyword, TokenKind};
use crate::span::Span;

impl super::Parser<'_> {
    /// A record declaration `[ref] record Name with f1, f2, … [doc] end` (L§9.1).
    pub(super) fn record_decl(&mut self) -> NodeId {
        let start = self.peek_span().start;
        let is_ref = matches!(self.peek_kind(), Some(TokenKind::Keyword(Keyword::Ref)));
        if is_ref {
            self.advance(); // `ref`
        }
        self.expect_keyword(Keyword::Record, "expected `record`");
        let (name, _) = self.expect_name("expected a record name");
        self.expect_keyword(Keyword::With, "expected `with` before the field list");
        let fields = self.field_list();
        let doc = self.record_body();
        let end = self.expect_end_span("record");
        self.push(
            Node::Record {
                is_ref,
                name,
                fields,
                doc,
            },
            Span::new(start, end),
        )
    }

    /// The `with` field list: one or more comma-separated names (L§9.1).
    fn field_list(&mut self) -> Vec<Box<str>> {
        let mut fields = Vec::new();
        loop {
            let (name, _) = self.expect_name("expected a field name");
            fields.push(name);
            if !matches!(self.peek_kind(), Some(TokenKind::Comma)) {
                break;
            }
            self.advance(); // `,`
        }
        fields
    }

    /// The optional docstring-only record body (L§9.1): captures a leading
    /// docstring; any further content before `end` is an error (records have no
    /// methods), recovered by consuming it.
    fn record_body(&mut self) -> Option<Span> {
        self.skip_separators();
        let doc = self.capture_docstring();
        self.skip_separators();
        if !self.at_block_end() {
            let span = self.peek_span();
            self.error(
                span,
                "a record body may contain only a docstring (records have no methods)",
            );
            self.block(is_end_terminator); // consume the rest up to `end`
        }
        doc
    }

    /// A protocol declaration `protocol Name [extends P] [doc] members… end`
    /// (L§10.1).
    pub(super) fn protocol_decl(&mut self) -> NodeId {
        let start = self.peek_span().start;
        self.advance(); // `protocol`
        let (name, _) = self.expect_name("expected a protocol name");
        let extends = if matches!(self.peek_kind(), Some(TokenKind::Keyword(Keyword::Extends))) {
            self.advance();
            Some(
                self.expect_name("expected a protocol name after `extends`")
                    .0,
            )
        } else {
            None
        };
        self.skip_separators();
        let doc = self.capture_docstring();
        let mut members = Vec::new();
        loop {
            self.skip_separators();
            let kind = match self.peek_kind() {
                Some(TokenKind::Keyword(Keyword::To)) => CallableKind::Proc,
                Some(TokenKind::Keyword(Keyword::Fn)) => CallableKind::Func,
                _ => break, // `end`, EOF, or unexpected — the loop ends here
            };
            members.push(self.proto_member(kind));
        }
        let end = self.close_protocol();
        self.push(
            Node::Protocol {
                name,
                extends,
                members,
                doc,
            },
            Span::new(start, end),
        )
    }

    /// Consumes the protocol's closing `end`, returning its end offset. On a
    /// missing `end` the message names the member-`end` requirement, since a
    /// bare member signature (missing its own `end`, S-52) silently eats this
    /// `end` and closes the protocol early.
    fn close_protocol(&mut self) -> u32 {
        let span = self.peek_span();
        if matches!(self.peek_kind(), Some(TokenKind::Keyword(Keyword::End))) {
            self.advance();
            span.end
        } else {
            self.error(
                span,
                "expected `end` to close this `protocol` — each `to`/`fn` member \
                 needs its own `end` too (an empty body is a required member)",
            );
            span.start
        }
    }

    /// A protocol member (L§10.1): a `to`/`fn` signature terminated by its own
    /// `end` (S-52). An empty body (a docstring aside) is a **required** member;
    /// a non-empty body is a **default**. A member's leading string is always its
    /// docstring (a signature has no result), so it is captured raw regardless of
    /// `to`/`fn`.
    fn proto_member(&mut self, kind: CallableKind) -> ProtoMember {
        self.advance(); // `to` / `fn`
        let (name, _) = self.expect_name("expected a member name");
        let params = self.param_list();
        let (block, doc) = self.body_with_doc(is_end_terminator, false);
        self.expect_end_span("protocol member");
        let body = if self.block_is_empty(block) {
            None
        } else {
            Some(block)
        };
        ProtoMember {
            kind,
            name,
            params,
            body,
            doc,
        }
    }

    /// Whether `id` is an empty [`Node::Block`].
    fn block_is_empty(&self, id: NodeId) -> bool {
        matches!(self.ast.node(id), Node::Block(stmts) if stmts.is_empty())
    }

    /// An `implement P for T methods… end` block (L§10.2): the methods are
    /// `to`/`fn` declarations registering `(T, P member) → callable`.
    pub(super) fn implement_decl(&mut self) -> NodeId {
        let start = self.peek_span().start;
        self.advance(); // `implement`
        let (protocol, _) = self.expect_name("expected a protocol name after `implement`");
        self.expect_keyword(Keyword::For, "expected `for` in this `implement`");
        let (type_name, _) = self.expect_name("expected a type name after `for`");
        let mut methods = Vec::new();
        loop {
            self.skip_separators();
            let kind = match self.peek_kind() {
                Some(TokenKind::Keyword(Keyword::To)) => CallableKind::Proc,
                Some(TokenKind::Keyword(Keyword::Fn)) => CallableKind::Func,
                _ => break,
            };
            methods.push(self.callable_decl(kind));
        }
        let end = self.expect_end_span("implement");
        self.push(
            Node::Implement {
                protocol,
                type_name,
                methods,
            },
            Span::new(start, end),
        )
    }

    /// Whether the cursor is at a body terminator: `end`, EOF, or end-of-input.
    fn at_block_end(&self) -> bool {
        matches!(
            self.peek_kind(),
            Some(TokenKind::Keyword(Keyword::End) | TokenKind::Eof) | None
        )
    }
}
