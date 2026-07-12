//! Statements and bodies (L§7): a body is a sequence of statements separated by
//! a newline or `;` (§7.1). This module parses that sequence — the module
//! program, construct bodies (via [`super::Parser::block`]), the binding and
//! assignment forms, and the non-local exits — and dispatches the rest
//! (`if`/`try` and the loop/`with` constructs) to their parsers.

use crate::ast::{CallableKind, Node, NodeId};
use crate::lex::{Keyword, TokenKind};
use crate::span::Span;

/// Which non-local exit (L§7.10 / §12.1) an exit statement is.
enum ExitKind {
    Return,
    Break,
    Continue,
    Raise,
}

impl super::Parser<'_> {
    /// Parses a whole program: a module body of statements up to end-of-input
    /// (L§7.1, Appendix A). Sets and returns the [`Node::Module`] root.
    pub(super) fn program(&mut self) -> NodeId {
        let start = self.peek_span().start;
        let stmts = self.statements(|_| false);
        let end = self.body_span_end(start, &stmts);
        let node = self.push(Node::Module(stmts), Span::new(start, end));
        self.ast.set_root(node);
        node
    }

    /// Parses a `body` (L§7.1): statements up to a terminator keyword (per
    /// `is_term`) or end-of-input, as a [`Node::Block`]. The terminator itself
    /// is left for the enclosing construct to consume.
    pub(super) fn block(&mut self, is_term: fn(TokenKind) -> bool) -> NodeId {
        let start = self.peek_span().start;
        let stmts = self.statements(is_term);
        let end = self.body_span_end(start, &stmts);
        self.push(Node::Block(stmts), Span::new(start, end))
    }

    /// The shared statement-sequence loop for [`program`](Self::program) and
    /// [`block`](Self::block).
    fn statements(&mut self, is_term: fn(TokenKind) -> bool) -> Vec<NodeId> {
        let mut stmts = Vec::new();
        loop {
            self.skip_separators();
            if self.bailed {
                break;
            }
            match self.peek_kind() {
                None | Some(TokenKind::Eof) => break,
                Some(k) if is_term(k) => break,
                _ => {}
            }
            let before = self.pos;
            let stmt = self.statement();
            stmts.push(stmt);
            if self.pos == before {
                // A statement that consumed nothing would spin the loop; force
                // one token of progress. (Recovery paths normally advance.)
                self.advance();
                continue;
            }
            self.require_separator(is_term);
        }
        stmts
    }

    /// After a statement, the next token must be a separator, a terminator, or
    /// end-of-input; anything else (two statements run together) is an error. We
    /// don't consume it — the next iteration re-parses it as a fresh statement.
    fn require_separator(&mut self, is_term: fn(TokenKind) -> bool) {
        match self.peek_kind() {
            None | Some(TokenKind::Eof | TokenKind::Newline | TokenKind::Semicolon) => {}
            Some(k) if is_term(k) => {}
            Some(_) => {
                let span = self.peek_span();
                self.error(span, "expected a statement separator");
            }
        }
    }

    fn statement(&mut self) -> NodeId {
        if let Some(err) = self.guard_depth("statement") {
            return err;
        }
        let node = self.statement_dispatch();
        self.depth -= 1;
        node
    }

    fn statement_dispatch(&mut self) -> NodeId {
        use Keyword as K;
        match self.peek_kind() {
            Some(TokenKind::Keyword(K::Let)) => self.let_stmt(false),
            Some(TokenKind::Keyword(K::Const)) => self.let_stmt(true),
            Some(TokenKind::Keyword(K::While)) => self.while_stmt(),
            Some(TokenKind::Keyword(K::Loop)) => self.loop_stmt(),
            Some(TokenKind::Keyword(K::With)) => self.with_stmt(),
            Some(TokenKind::Keyword(K::Return)) => self.exit_stmt(ExitKind::Return),
            Some(TokenKind::Keyword(K::Break)) => self.exit_stmt(ExitKind::Break),
            Some(TokenKind::Keyword(K::Continue)) => self.exit_stmt(ExitKind::Continue),
            Some(TokenKind::Keyword(K::Raise)) => self.exit_stmt(ExitKind::Raise),
            Some(TokenKind::Keyword(K::Do)) => self.stray_do(),
            Some(TokenKind::Keyword(K::To)) => self.callable_decl(CallableKind::Proc),
            // `fn name(…)` is a declaration; `fn(…)` is an anonymous function
            // (an expression), which falls through to the expression parser.
            Some(TokenKind::Keyword(K::Fn)) if self.next_is_ident() => {
                self.callable_decl(CallableKind::Func)
            }
            // `if`/`try`/anonymous-`fn` (also statements) fall through to the
            // expression parser, which handles them as primaries; the rest are
            // expression statements or assignments.
            _ => self.expr_or_assign(),
        }
    }

    /// A `let`/`const` binding (L§5.2). `is_const` selects the form.
    fn let_stmt(&mut self, is_const: bool) -> NodeId {
        let start = self.peek_span().start;
        self.advance(); // `let` / `const`
        let (name, _) = self.expect_name(if is_const {
            "expected a name after `const`"
        } else {
            "expected a name after `let`"
        });
        self.expect_eq(if is_const { "const" } else { "let" });
        let value = self.expr(0);
        let span = Span::new(start, self.ast.span(value).end);
        let node = if is_const {
            Node::Const { name, value }
        } else {
            Node::Let { name, value }
        };
        self.push(node, span)
    }

    /// An expression used as a statement, or an assignment `lvalue = expr`
    /// (L§5.3). `=` (not an expression operator) after a leading expression marks
    /// an assignment; the target must be an lvalue (name / field / index).
    fn expr_or_assign(&mut self) -> NodeId {
        let lhs = self.expr(0);
        if !matches!(self.peek_kind(), Some(TokenKind::Eq)) {
            let span = self.ast.span(lhs);
            return self.push(Node::ExprStmt(lhs), span);
        }
        self.advance(); // `=`
        if !self.is_lvalue(lhs) {
            let span = self.ast.span(lhs);
            self.error(
                span,
                "the left side of `=` must be a name, a field (`a.b`), or an index (`a[i]`)",
            );
        }
        let value = self.expr(0);
        let span = Span::new(self.ast.span(lhs).start, self.ast.span(value).end);
        self.push(Node::Assign { target: lhs, value }, span)
    }

    fn is_lvalue(&self, id: NodeId) -> bool {
        matches!(
            self.ast.node(id),
            Node::Ident(_) | Node::Field { .. } | Node::Index { .. }
        )
    }

    /// A non-local exit (L§7.10 / §12.1): `return`/`break`/`continue`/`raise`,
    /// each with an optional operand.
    fn exit_stmt(&mut self, kind: ExitKind) -> NodeId {
        let kw = self.peek_span();
        self.advance(); // the keyword
        let operand = if self.at_operand_boundary() {
            None
        } else {
            Some(self.expr(0))
        };
        let end = operand.map_or(kw.end, |o| self.ast.span(o).end);
        let node = match kind {
            ExitKind::Return => Node::Return(operand),
            ExitKind::Break => Node::Break(operand),
            ExitKind::Continue => Node::Continue(operand),
            ExitKind::Raise => Node::Raise(operand),
        };
        self.push(node, Span::new(kw.start, end))
    }

    /// Whether the cursor is where an optional exit operand ends (a separator,
    /// a body terminator, or end-of-input) — so `return`/`break`/… take no value.
    fn at_operand_boundary(&self) -> bool {
        matches!(
            self.peek_kind(),
            None | Some(
                TokenKind::Eof
                    | TokenKind::Newline
                    | TokenKind::Semicolon
                    | TokenKind::Keyword(Keyword::End)
                    | TokenKind::Keyword(Keyword::Else)
                    | TokenKind::Keyword(Keyword::Rescue)
            )
        )
    }

    /// A `do` opening a statement: it can't stand alone (S-4). This is the
    /// leftover-`do` case (a construct's body already closed, or a stray block).
    /// Report it, then consume the whole `do … end` so its body doesn't cascade
    /// into further errors.
    fn stray_do(&mut self) -> NodeId {
        let span = self.peek_span();
        // The parenthesized-block escape hatch (L§6.4) is deliberately not
        // suggested here: block arguments don't parse yet (M1.8), so pointing at
        // `(f() do … end)` would send the user toward a different error.
        self.error(
            span,
            "a `do … end` block can't start a statement — it opens a \
             `while`/`loop`/`with` body, or is a call's trailing block argument",
        );
        self.advance(); // `do`
        self.block(is_end_terminator);
        let end = self.expect_end_span("do");
        self.push(Node::Error, Span::new(span.start, end))
    }

    fn skip_separators(&mut self) {
        while matches!(
            self.peek_kind(),
            Some(TokenKind::Newline | TokenKind::Semicolon)
        ) {
            self.advance();
        }
    }

    /// Whether the token after the cursor is an identifier — the lookahead that
    /// tells a named `fn name(…)` declaration from an anonymous `fn(…)`.
    fn next_is_ident(&self) -> bool {
        self.tokens.get(self.pos + 1).map(|t| t.kind) == Some(TokenKind::Ident)
    }

    /// Consumes an identifier, returning its text and span; on a non-identifier
    /// reports `msg` (without consuming, so a following `=`/`do` still recovers)
    /// and returns an empty name.
    pub(super) fn expect_name(&mut self, msg: &str) -> (Box<str>, Span) {
        let span = self.peek_span();
        if self.peek_kind() == Some(TokenKind::Ident) {
            let name = self.ident_text_at();
            self.advance();
            (name, span)
        } else {
            self.error(span, msg);
            (Box::from(""), span)
        }
    }

    /// Consumes the `=` of a `let`/`const`/`with`; reports if it is missing.
    pub(super) fn expect_eq(&mut self, what: &str) {
        if matches!(self.peek_kind(), Some(TokenKind::Eq)) {
            self.advance();
        } else {
            let span = self.peek_span();
            self.error(span, &format!("expected `=` in this `{what}`"));
        }
    }

    /// Consumes the closing `end` of a construct, returning its end offset; on a
    /// missing `end` reports against `what` and returns the current start offset.
    pub(super) fn expect_end_span(&mut self, what: &str) -> u32 {
        let span = self.peek_span();
        if matches!(self.peek_kind(), Some(TokenKind::Keyword(Keyword::End))) {
            self.advance();
            span.end
        } else {
            self.error(span, &format!("expected `end` to close this `{what}`"));
            span.start
        }
    }

    fn body_span_end(&self, start: u32, stmts: &[NodeId]) -> u32 {
        stmts.last().map_or(start, |&s| self.ast.span(s).end)
    }
}

/// A body terminator predicate matching only `end` — the close of a
/// `while`/`loop`/`with`/`if`/`try` body.
pub(super) fn is_end_terminator(k: TokenKind) -> bool {
    matches!(k, TokenKind::Keyword(Keyword::End))
}
