//! Snapshot conventions (insta): a stable `Debug` snapshot of the M0.3
//! hand-built one-statement AST, establishing the
//! `crates/doodle-core/tests/snapshots/` layout. Snapshot tests run as ordinary
//! `cargo test` tests; review pending changes with `cargo insta review`.

use doodle_core::ast::{Ast, Node};
use doodle_core::span::Span;

#[test]
fn hand_built_one_statement_ast_debug() {
    let mut ast = Ast::new();
    let lit = ast.push(Node::IntLit(42), Span::new(0, 2));
    let stmt = ast.push(Node::ExprStmt(lit), Span::new(0, 2));
    let root = ast.push(
        Node::Module {
            stmts: vec![stmt],
            doc: None,
        },
        Span::new(0, 2),
    );
    ast.set_root(root);

    insta::assert_debug_snapshot!(ast);
}
