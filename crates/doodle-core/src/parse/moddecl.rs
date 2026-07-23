//! Module-structure declarations (L§11.1/§11.2/§5.5): the explicit `module Name
//! … end` form, `parameter name = default`, `exports name, …`, and `import`.
//! Together with `record`/`protocol`/`implement` (typedecl) these are the
//! module-level-only declarations (L§7.1); each reports via
//! `require_module_level` when nested.

use super::stmt::is_end_terminator;
use crate::ast::{ImportTarget, Node, NodeId};
use crate::lex::{Keyword, TokenKind};
use crate::span::Span;

impl super::Parser<'_> {
    /// An explicit named module `module Name body end` (L§11.1). Its body is
    /// module-level (nested modules' contents are still module-level).
    pub(super) fn module_decl(&mut self) -> NodeId {
        self.require_module_level("module");
        let open = self.peek_span();
        let start = open.start;
        self.advance(); // `module`
        let (name, _) = self.expect_name("expected a module name after `module`");
        let (body, doc) = self.module_body(is_end_terminator);
        let end = self.expect_end_span("module", open);
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

    /// An `import target, …` declaration (L§11.2): comma-separated targets, each
    /// a dotted path optionally ending in `.*` or renamed with `as`.
    pub(super) fn import_stmt(&mut self) -> NodeId {
        self.require_module_level("import");
        let start = self.peek_span().start;
        self.advance(); // `import`
        let mut targets = Vec::new();
        let mut end;
        loop {
            let (target, target_end) = self.import_target();
            end = target_end;
            targets.push(target);
            if !matches!(self.peek_kind(), Some(TokenKind::Comma)) {
                break;
            }
            self.advance(); // `,`
        }
        self.push(Node::Import(targets), Span::new(start, end))
    }

    /// One import target (L§11.2). The parser produces the dotted path only; a
    /// qualified module vs. a member is resolved at load, not here (S-7).
    /// Returns the target and its end offset.
    fn import_target(&mut self) -> (ImportTarget, u32) {
        let (first, first_span) = self.expect_name("expected a module name after `import`");
        let mut path = vec![first];
        let mut end = first_span.end;
        let mut wildcard = false;
        loop {
            match self.peek_kind() {
                Some(TokenKind::Dot) => {
                    self.advance(); // `.`
                    let (seg, seg_span) =
                        self.expect_name("expected a name after `.` in the import path");
                    end = seg_span.end;
                    path.push(seg);
                }
                Some(TokenKind::DotStar) => {
                    end = self.peek_span().end;
                    self.advance(); // `.*` ends the path
                    wildcard = true;
                    break;
                }
                _ => break,
            }
        }
        let alias = if matches!(self.peek_kind(), Some(TokenKind::Keyword(Keyword::As))) {
            let as_span = self.peek_span();
            self.advance(); // `as`
            if wildcard {
                self.error(
                    as_span,
                    "`import … .*` brings in all exported members and can't be renamed with `as`",
                );
            }
            let (name, name_span) = self.expect_name("expected a name after `as`");
            end = name_span.end;
            Some(name)
        } else {
            None
        };
        (
            ImportTarget {
                path,
                wildcard,
                alias,
            },
            end,
        )
    }
}
