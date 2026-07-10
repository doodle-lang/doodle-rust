//! M0.3 acceptance: hand-build a one-statement AST, drive it to `Completed`,
//! and observe the result value through the public API.

use doodle_core::ast::{Ast, Node};
use doodle_core::drive::{Directive, Outcome, run};
use doodle_core::machine::{Instance, InstanceState, Value};
use doodle_core::span::Span;

/// The program `42` (a single integer-literal expression statement) drives to
/// `Completed` carrying `Int(42)`, and the same value is readable from the
/// instance's result register.
#[test]
fn drives_integer_literal_statement_to_completed() {
    let mut ast = Ast::new();
    let lit = ast.push(Node::IntLit(42), Span::new(0, 2));
    let stmt = ast.push(Node::ExprStmt(lit), Span::new(0, 2));
    let root = ast.push(Node::Module(vec![stmt]), Span::new(0, 2));
    ast.set_root(root);

    let mut instance = Instance::new(ast);
    assert_eq!(instance.state(), InstanceState::Ready);

    let outcome = run(&mut instance, Directive::RunToCompletion);

    assert_eq!(instance.state(), InstanceState::Completed);
    match outcome {
        Outcome::Completed(Some(value)) => assert_eq!(value.as_int(), Some(42)),
        other => panic!("expected Completed(Some(Int(42))), got {other:?}"),
    }
    assert_eq!(instance.result().and_then(Value::as_int), Some(42));
}

/// An empty module body (no statements) drives to `Completed` with a Void
/// (`None`) result register.
#[test]
fn drives_empty_module_to_void_completion() {
    let mut ast = Ast::new();
    let root = ast.push(Node::Module(vec![]), Span::DUMMY);
    ast.set_root(root);

    let mut instance = Instance::new(ast);
    let outcome = run(&mut instance, Directive::RunToCompletion);

    assert_eq!(instance.state(), InstanceState::Completed);
    assert!(matches!(outcome, Outcome::Completed(None)));
    // `Value` has no `PartialEq` (machine-design §3), so inspect the option
    // directly rather than comparing to `None`.
    assert!(instance.result().is_none());
}
