//! Parser tests: expression trees rendered as S-expressions, so precedence,
//! associativity, and value lowering are visible in the expected output.

use doodle_core::ast::{Arg, Ast, BinaryOp, Node, NodeId, StrPart, UnaryOp};
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
        Node::Field { object, name } => format!("(. {} {name})", dump(ast, *object)),
        Node::Index { object, index } => {
            format!("([] {} {})", dump(ast, *object), dump(ast, *index))
        }
        Node::Call { callee, args } => {
            let mut s = format!("(call {}", dump(ast, *callee));
            for arg in args {
                match arg {
                    Arg::Positional(e) => {
                        s.push(' ');
                        s.push_str(&dump(ast, *e));
                    }
                    Arg::Keyword { name, value } => {
                        s.push_str(&format!(" {name}:{}", dump(ast, *value)));
                    }
                }
            }
            s.push(')');
            s
        }
        Node::StrLit(parts) => {
            let mut s = String::from("(str");
            for part in parts {
                match part {
                    StrPart::Text(t) => s.push_str(&format!(" {t:?}")),
                    StrPart::Interp(e) => s.push_str(&format!(" {{{}}}", dump(ast, *e))),
                }
            }
            s.push(')');
            s
        }
        Node::BytesLit(bytes) if bytes.is_empty() => "(bytes)".to_string(),
        Node::BytesLit(bytes) => {
            let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
            format!("(bytes {})", hex.join(" "))
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
fn string_literals_decode_and_interpolate() {
    // Plain text and escapes (decoded; the debug form shows control chars).
    assert_eq!(ast_of("\"hello\""), "(str \"hello\")");
    assert_eq!(ast_of("\"a\\nb\\tc\""), "(str \"a\\nb\\tc\")");
    // `\x41` is code point U+0041 ('A'); `\u{42}` is 'B'.
    assert_eq!(ast_of("\"\\x41\\u{42}\""), "(str \"AB\")");
    // `{{`/`}}` collapse to literal braces, not interpolation.
    assert_eq!(ast_of("\"{{x}}\""), "(str \"{x}\")");
    // An empty string has no parts.
    assert_eq!(ast_of("\"\""), "(str)");
    // Interpolation splits into text and (parsed) expression parts.
    assert_eq!(ast_of("\"x {a + 1} y\""), "(str \"x \" {(+ a 1)} \" y\")");
    assert_eq!(ast_of("\"{name}\""), "(str {name})");
    // A nested string inside an interpolation.
    assert_eq!(ast_of("\"{\"in\"}\""), "(str {(str \"in\")})");
}

#[test]
fn triple_quoted_decode_and_line_final_backslash() {
    // Margins strip, lines join with `\n`, escapes decode, and adjacent text
    // (including the join) merges into one part.
    let ok = "\"\"\"\n    a\\tb\n    c\n    \"\"\"";
    assert_eq!(ast_of(ok), "(str \"a\\tb\\nc\")");
    assert!(diags_of(ok).is_empty());
    // A line-final `\` in a triple-quoted string is an error (closed set,
    // L§3.6.3, × S-3's no-line-join) — both mid-block and on the last content
    // line (a `\` at the very end of the value).
    let mid = "\"\"\"\n    a\\\n    b\n    \"\"\"";
    assert!(diags_of(mid).contains(&"syntax-error"));
    let last = "\"\"\"\n    a\\\n    \"\"\"";
    assert!(diags_of(last).contains(&"syntax-error"));
    // In a single-line string, unterminated-string takes precedence — no
    // separate backslash-at-end error.
    assert_eq!(diags_of("\"abc\\"), vec!["unterminated-string"]);
}

#[test]
fn postfix_access_call_index() {
    assert_eq!(ast_of("p.x"), "(. p x)");
    assert_eq!(ast_of("a[0]"), "([] a 0)");
    assert_eq!(ast_of("f()"), "(call f)");
    assert_eq!(ast_of("f(1, 2)"), "(call f 1 2)");
    // Postfix chains left-to-right and binds tighter than prefix/binary.
    assert_eq!(ast_of("a.b.c"), "(. (. a b) c)");
    assert_eq!(ast_of("f(x)(y)"), "(call (call f x) y)");
    assert_eq!(ast_of("a[i][j]"), "([] ([] a i) j)");
    assert_eq!(ast_of("obj.method(arg)"), "(call (. obj method) arg)");
    assert_eq!(ast_of("-a.b"), "(- (. a b))");
    assert_eq!(ast_of("a.b + c"), "(+ (. a b) c)");
    assert_eq!(ast_of("a ** b.c"), "(** a (. b c))");
}

#[test]
fn call_keyword_arguments() {
    assert_eq!(ast_of("Point(x: 3, y: 4)"), "(call Point x:3 y:4)");
    assert_eq!(ast_of("f(1, key: 2)"), "(call f 1 key:2)");
    // A trailing comma is allowed.
    assert_eq!(ast_of("f(a, b,)"), "(call f a b)");
    // Positional after keyword is a static error (L§6.4).
    assert_eq!(diags_of("f(k: 1, 2)"), vec!["syntax-error"]);
    assert!(diags_of("f(1, k: 2)").is_empty());
    // A missing `)` and a `.` with no field recover without panic. A number
    // after `.` reports once (the offending token is consumed, not cascaded).
    assert_eq!(diags_of("f(1"), vec!["syntax-error"]);
    assert_eq!(diags_of("a."), vec!["syntax-error"]);
    assert_eq!(diags_of("a.1"), vec!["syntax-error"]);
}

#[test]
fn malformed_escapes_recover_without_panic() {
    // A malformed `\x` before a multibyte char must not slice off a UTF-8
    // boundary (regression: the parser used to panic here).
    for bad in [
        "\"\\x1é\"",
        "\"\\xGé\"",
        "\"\\x😀\"",
        "\"\\x1😀\"",
        "\"\\x\"",
    ] {
        let _ = diags_of(bad); // just must not panic
    }
    // Each already carries a lexer diagnostic; the parser adds none of its own
    // beyond recovery, and never crashes.
    assert!(diags_of("\"\\x1é\"").contains(&"malformed-escape"));
    // Braces are literal in bytes (2b/7b/7d) — a `{x}` is three bytes.
    assert_eq!(ast_of("b\"a\\\"b\""), "(bytes 61 22 62)"); // an escaped quote is a byte
}

#[test]
fn bytes_literals_decode() {
    assert_eq!(ast_of("b\"GET\""), "(bytes 47 45 54)");
    assert_eq!(ast_of("b\"\\x00\\xff\\n\""), "(bytes 00 ff 0a)");
    assert_eq!(ast_of("b\"\""), "(bytes)");
    // Braces are literal in bytes (no interpolation).
    assert_eq!(ast_of("b\"{x}\""), "(bytes 7b 78 7d)");
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
