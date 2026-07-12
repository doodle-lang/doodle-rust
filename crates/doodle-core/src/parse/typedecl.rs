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
        let end = self.expect_end_span("protocol");
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

    /// A protocol member (L§10.1): a `to`/`fn` signature, required (no body) or
    /// default (a NON-empty body closed by `end`).
    ///
    /// PROVISIONAL disambiguation of a genuine L§10.1 grammar ambiguity (the
    /// grammar's `(body)? 'end'?` leaves both optional; see claude-todo, needs a
    /// user ruling): after the signature, a following member (`to`/`fn`), `end`,
    /// or EOF means "required, no body" and any `end` belongs to the enclosing
    /// protocol — matching the spec's `Iterable` example. Consequences: an
    /// empty-body default (`sig end`) is not expressible, and a member written
    /// with an explicit trailing `end` before another member is misparsed (that
    /// `end` closes the protocol early). Resolve in L§10.1 before this ships.
    fn proto_member(&mut self, kind: CallableKind) -> ProtoMember {
        self.advance(); // `to` / `fn`
        let (name, _) = self.expect_name("expected a member name");
        let params = self.param_list();
        self.skip_separators();
        let body = if matches!(
            self.peek_kind(),
            Some(TokenKind::Keyword(Keyword::To | Keyword::Fn | Keyword::End) | TokenKind::Eof)
                | None
        ) {
            None
        } else {
            let b = self.block(is_end_terminator);
            self.expect_end_span("protocol member");
            Some(b)
        };
        ProtoMember {
            kind,
            name,
            params,
            body,
        }
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
