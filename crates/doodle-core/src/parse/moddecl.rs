//! Module-structure declarations (L§11.1/§5.5): the explicit `module Name …
//! end` form, `parameter name = default`, and `exports name, …`. Together with
//! `record`/`protocol`/`implement` (typedecl) these are the module-level-only
//! declarations (L§7.1); each reports via `require_module_level` when nested.
//! (`import` is M1.9.)

use super::stmt::is_end_terminator;
use crate::ast::{Node, NodeId};
use crate::lex::TokenKind;
use crate::span::Span;

impl super::Parser<'_> {
    /// An explicit named module `module Name body end` (L§11.1). Its body is
    /// module-level (nested modules' contents are still module-level).
    pub(super) fn module_decl(&mut self) -> NodeId {
        self.require_module_level("module");
        let start = self.peek_span().start;
        self.advance(); // `module`
        let (name, _) = self.expect_name("expected a module name after `module`");
        let (body, doc) = self.module_body(is_end_terminator);
        let end = self.expect_end_span("module");
        self.push(Node::ModuleDecl { name, body, doc }, Span::new(start, end))
    }

    /// A dynamic-parameter declaration `parameter name = default` (L§5.5).
    pub(super) fn parameter_decl(&mut self) -> NodeId {
        self.require_module_level("parameter");
        let start = self.peek_span().start;
        self.advance(); // `parameter`
        let (name, _) = self.expect_name("expected a dynamic-parameter name after `parameter`");
        self.expect_eq("parameter");
        let default = self.expr(0);
        let span = Span::new(start, self.ast.span(default).end);
        self.push(Node::Parameter { name, default }, span)
    }

    /// An `exports name, …` declaration (L§11.1): the module's public surface.
    pub(super) fn exports_stmt(&mut self) -> NodeId {
        self.require_module_level("exports");
        let start = self.peek_span().start;
        self.advance(); // `exports`
        let mut names = Vec::new();
        // The loop always runs once (so `end` is always assigned before use).
        let mut end;
        loop {
            let (name, span) = self.expect_name("expected an exported name");
            end = span.end;
            names.push(name);
            if !matches!(self.peek_kind(), Some(TokenKind::Comma)) {
                break;
            }
            self.advance(); // `,`
        }
        self.push(Node::Exports(names), Span::new(start, end))
    }
}
