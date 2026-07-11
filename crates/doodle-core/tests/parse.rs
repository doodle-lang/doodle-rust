//! Parser tests: expression trees rendered as S-expressions, so precedence,
//! associativity, and value lowering are visible in the expected output.

use doodle_core::ast::{Ast, BinaryOp, Node, NodeId, UnaryOp};
use doodle_core::parse::parse_expression;
use doodle_core::source::normalize;
use doodle_core::span::ModuleId;

const M: ModuleId = ModuleId(0);

/// Parses `src` and renders its AST as a Lisp-style S-expression.
fn ast_of(src: &str) -> String {
    let nfc = normalize(src);
    let p = parse_expression(nfc.as_ref(), M);
    dump(&p.ast, p.root)
}

fn diags_of(src: &str) -> Vec<&'static str> {
    let nfc = normalize(src);
    parse_expression(nfc.as_ref(), M)
        .diagnostics
        .iter()
        .map(|d| d.code.slug())
        .collect()
}

fn dump(ast: &Ast, id: NodeId) -> String {
    match ast.node(id) {
        Node::IntLit(n) => n.to_string(),
        Node::BigIntLit { radix, digits } => format!("big:{radix}:{digits}"),
        Node::FloatLit(x) => format!("{x:?}"),
        Node::BoolLit(b) => b.to_string(),
        Node::NilLit => "nil".to_string(),
        Node::Ident(name) => name.to_string(),
        Node::Unary { op, operand } => format!("({} {})", unary_sym(*op), dump(ast, *operand)),
        Node::Binary { op, lhs, rhs } => {
            format!(
                "({} {} {})",
                binary_sym(*op),
                dump(ast, *lhs),
                dump(ast, *rhs)
            )
        }
        Node::Error => "<error>".to_string(),
        Node::ExprStmt(e) => dump(ast, *e),
        Node::Module(_) => "<module>".to_string(),
    }
}

fn unary_sym(op: UnaryOp) -> &'static str {
    match op {
        UnaryOp::Neg => "-",
        UnaryOp::Pos => "+",
        UnaryOp::Not => "not",
    }
}

fn binary_sym(op: BinaryOp) -> &'static str {
    use BinaryOp::*;
    match op {
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        FloorDiv => "//",
        Rem => "%",
        Pow => "**",
        Eq => "==",
        Ne => "!=",
        Lt => "<",
        Gt => ">",
        Le => "<=",
        Ge => ">=",
        Is => "is",
        And => "and",
        Or => "or",
    }
}

#[test]
fn literals_lower_their_values() {
    assert_eq!(ast_of("42"), "42");
    assert_eq!(ast_of("0xFF"), "255");
    assert_eq!(ast_of("0b1010"), "10");
    assert_eq!(ast_of("0o755"), "493");
    assert_eq!(ast_of("1_000_000"), "1000000");
    assert_eq!(ast_of("3.14"), "3.14");
    assert_eq!(ast_of("1e6"), "1000000.0");
    assert_eq!(ast_of("true"), "true");
    assert_eq!(ast_of("false"), "false");
    assert_eq!(ast_of("nil"), "nil");
    assert_eq!(ast_of("count"), "count");
    // An integer beyond i64 becomes a bignum literal (radix + digits).
    assert_eq!(
        ast_of("99999999999999999999"),
        "big:10:99999999999999999999"
    );
}

#[test]
fn precedence_and_associativity() {
    // Multiplicative binds tighter than additive.
    assert_eq!(ast_of("1 + 2 * 3"), "(+ 1 (* 2 3))");
    assert_eq!(ast_of("(1 + 2) * 3"), "(* (+ 1 2) 3)");
    // `+`/`-` and `*`/`/` are left-associative.
    assert_eq!(ast_of("1 - 2 - 3"), "(- (- 1 2) 3)");
    assert_eq!(ast_of("8 / 4 / 2"), "(/ (/ 8 4) 2)");
    // `**` is right-associative and binds tighter than unary `-`.
    assert_eq!(ast_of("2 ** 3 ** 2"), "(** 2 (** 3 2))");
    assert_eq!(ast_of("-2 ** 2"), "(- (** 2 2))");
    // ...but unary `-` binds tighter than `*`.
    assert_eq!(ast_of("-2 * 3"), "(* (- 2) 3)");
    assert_eq!(ast_of("- -2"), "(- (- 2))");
}

#[test]
fn boolean_and_comparison_precedence() {
    // `not` binds looser than comparison, tighter than `and`; `or` is loosest.
    assert_eq!(ast_of("not a == b"), "(not (== a b))");
    assert_eq!(ast_of("not a and b"), "(and (not a) b)");
    assert_eq!(ast_of("a and b or c"), "(or (and a b) c)");
    assert_eq!(ast_of("a or b and c"), "(or a (and b c))");
    assert_eq!(ast_of("a < b and c"), "(and (< a b) c)");
    assert_eq!(ast_of("x is Int"), "(is x Int)");
}

#[test]
fn comparisons_do_not_chain() {
    // `a < b < c` is a static error (L§6.5), still parsed for recovery.
    assert_eq!(diags_of("a < b < c"), vec!["chained-comparison"]);
    assert_eq!(ast_of("a < b < c"), "(< (< a b) c)");
    assert_eq!(diags_of("a == b != c"), vec!["chained-comparison"]);
    // A comparison on each side of `and` is fine — no chaining.
    assert!(diags_of("a < b and b < c").is_empty());
    // Explicit parentheses disambiguate: not a chain (`==` is total, L§6.6).
    assert!(diags_of("(a == b) == c").is_empty());
    assert!(diags_of("(a < b) < c").is_empty());
    assert!(diags_of("not (a < b) < c").is_empty());
}

#[test]
fn deep_nesting_bails_without_stack_overflow() {
    // Pathological nesting must yield a diagnostic and terminate, never abort
    // the process with a stack overflow.
    let src = format!("{}1{}", "(".repeat(5000), ")".repeat(5000));
    assert!(diags_of(&src).contains(&"syntax-error"));
    // Long unary and `**` chains bail the same way.
    assert!(diags_of(&"-".repeat(5000)).contains(&"syntax-error"));
}

#[test]
fn syntax_errors_recover() {
    // A missing operand and a missing `)` are reported, not panicked on.
    assert_eq!(diags_of("1 +"), vec!["syntax-error"]);
    assert_eq!(diags_of("(1 + 2"), vec!["syntax-error"]);
    assert_eq!(diags_of(""), vec!["syntax-error"]);
    assert!(diags_of("1 + 2").is_empty());
}
