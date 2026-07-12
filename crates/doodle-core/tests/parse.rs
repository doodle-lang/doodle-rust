//! Parser tests: expression trees rendered as S-expressions, so precedence,
//! associativity, and value lowering are visible in the expected output.

use doodle_core::ast::{
    Arg, Ast, BinaryOp, CallableKind, DictKey, Node, NodeId, Param, ProtoMember, StrPart, UnaryOp,
};
use doodle_core::parse::{parse_expression, parse_program};
use doodle_core::source::normalize;
use doodle_core::span::{ModuleId, Span};

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

/// Parses `src` as a whole program and renders the module AST.
fn program_of(src: &str) -> String {
    let nfc = normalize(src);
    let p = parse_program(nfc.as_ref(), M);
    dump(&p.ast, p.root)
}

fn prog_diags(src: &str) -> Vec<&'static str> {
    let nfc = normalize(src);
    parse_program(nfc.as_ref(), M)
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
        Node::List(elems) if elems.is_empty() => "(list)".to_string(),
        Node::List(elems) => {
            let items: Vec<String> = elems.iter().map(|e| dump(ast, *e)).collect();
            format!("(list {})", items.join(" "))
        }
        Node::Dict(entries) if entries.is_empty() => "(dict)".to_string(),
        Node::Dict(entries) => {
            let mut s = String::from("(dict");
            for e in entries {
                match &e.key {
                    DictKey::Bare(name) => s.push_str(&format!(" {name}:{}", dump(ast, e.value))),
                    DictKey::Expr(k) => {
                        s.push_str(&format!(" [{}]:{}", dump(ast, *k), dump(ast, e.value)));
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
        Node::Let { name, value } => format!("(let {name} {})", dump(ast, *value)),
        Node::Const { name, value } => format!("(const {name} {})", dump(ast, *value)),
        Node::Assign { target, value } => {
            format!("(assign {} {})", dump(ast, *target), dump(ast, *value))
        }
        Node::Block(stmts) => seq(ast, "block", stmts),
        Node::Module { stmts, doc } => {
            let mut s = format!("(module{}", doc_marker(doc));
            for st in stmts {
                s.push(' ');
                s.push_str(&dump(ast, *st));
            }
            s.push(')');
            s
        }
        Node::If { arms, else_body } => {
            let mut s = String::from("(if");
            for arm in arms {
                s.push_str(&format!(
                    " ({} {})",
                    dump(ast, arm.cond),
                    dump(ast, arm.body)
                ));
            }
            if let Some(e) = else_body {
                s.push_str(&format!(" else {}", dump(ast, *e)));
            }
            s.push(')');
            s
        }
        Node::While { cond, body } => format!("(while {} {})", dump(ast, *cond), dump(ast, *body)),
        Node::Loop { body } => format!("(loop {})", dump(ast, *body)),
        Node::With { name, value, body } => {
            format!("(with {name} {} {})", dump(ast, *value), dump(ast, *body))
        }
        Node::Try {
            body,
            rescue_name,
            rescue_body,
        } => format!(
            "(try {} rescue {rescue_name} {})",
            dump(ast, *body),
            dump(ast, *rescue_body)
        ),
        Node::Return(o) => format!("(return{})", opt(ast, o)),
        Node::Break(o) => format!("(break{})", opt(ast, o)),
        Node::Continue(o) => format!("(continue{})", opt(ast, o)),
        Node::Raise(o) => format!("(raise{})", opt(ast, o)),
        Node::Callable {
            kind,
            name,
            params,
            body,
            doc,
        } => {
            let kw = match kind {
                CallableKind::Proc => "to",
                CallableKind::Func => "fn",
            };
            let name_s = match name {
                Some(n) => format!(" {n}"),
                None => String::new(),
            };
            format!(
                "({kw}{name_s}{} {} {})",
                doc_marker(doc),
                params_dump(ast, params),
                dump(ast, *body)
            )
        }
        Node::Record {
            is_ref,
            name,
            fields,
            doc,
        } => {
            let kw = if *is_ref { "ref-record" } else { "record" };
            let fields_s = if fields.is_empty() {
                "(fields)".to_string()
            } else {
                format!("(fields {})", fields.join(" "))
            };
            format!("({kw} {name} {fields_s}{})", doc_marker(doc))
        }
        Node::Protocol {
            name,
            extends,
            members,
            doc,
        } => {
            let mut s = format!("(protocol {name}");
            if let Some(p) = extends {
                s.push_str(&format!(" extends:{p}"));
            }
            s.push_str(doc_marker(doc));
            for m in members {
                s.push(' ');
                s.push_str(&proto_member_dump(ast, m));
            }
            s.push(')');
            s
        }
        Node::Implement {
            protocol,
            type_name,
            methods,
        } => {
            let mut s = format!("(implement {protocol} for {type_name}");
            for m in methods {
                s.push(' ');
                s.push_str(&dump(ast, *m));
            }
            s.push(')');
            s
        }
        Node::ModuleDecl { name, body, doc } => {
            format!("(moddecl {name}{} {})", doc_marker(doc), dump(ast, *body))
        }
        Node::Parameter { name, default } => {
            format!("(parameter {name} {})", dump(ast, *default))
        }
        Node::Exports(names) => format!("(exports {})", names.join(" ")),
        Node::Import(targets) => {
            let mut s = String::from("(import");
            for t in targets {
                s.push(' ');
                s.push_str(&t.path.join("."));
                if t.wildcard {
                    s.push_str(".*");
                }
                if let Some(a) = &t.alias {
                    s.push_str(&format!(":{a}"));
                }
            }
            s.push(')');
            s
        }
        Node::Error => "<error>".to_string(),
        Node::ExprStmt(e) => dump(ast, *e),
    }
}

/// A ` doc` marker when a docstring span is present, else "".
fn doc_marker(doc: &Option<Span>) -> &'static str {
    if doc.is_some() { " doc" } else { "" }
}

/// Renders a protocol member: `(req to name[ doc] (params …))` (required) or
/// `(def fn name[ doc] (params …) body)` (default, with a body).
fn proto_member_dump(ast: &Ast, m: &ProtoMember) -> String {
    let kw = match m.kind {
        CallableKind::Proc => "to",
        CallableKind::Func => "fn",
    };
    let d = doc_marker(&m.doc);
    match m.body {
        None => format!("(req {kw} {}{d} {})", m.name, params_dump(ast, &m.params)),
        Some(b) => format!(
            "(def {kw} {}{d} {} {})",
            m.name,
            params_dump(ast, &m.params),
            dump(ast, b)
        ),
    }
}

/// Renders a parameter list: `(params a b=<default> do:blk)`, `(params)` empty.
fn params_dump(ast: &Ast, params: &[Param]) -> String {
    if params.is_empty() {
        return "(params)".to_string();
    }
    let items: Vec<String> = params
        .iter()
        .map(|p| match p {
            Param::Ordinary {
                name,
                default: None,
            } => name.to_string(),
            Param::Ordinary {
                name,
                default: Some(d),
            } => format!("{name}={}", dump(ast, *d)),
            Param::Block { name } => format!("do:{name}"),
        })
        .collect();
    format!("(params {})", items.join(" "))
}

/// Renders a keyword-tagged statement sequence: `(kw s1 s2 …)`, `(kw)` if empty.
fn seq(ast: &Ast, kw: &str, stmts: &[NodeId]) -> String {
    if stmts.is_empty() {
        return format!("({kw})");
    }
    let items: Vec<String> = stmts.iter().map(|s| dump(ast, *s)).collect();
    format!("({kw} {})", items.join(" "))
}

/// Renders an optional exit operand as a leading-space suffix, or "".
fn opt(ast: &Ast, operand: &Option<NodeId>) -> String {
    match operand {
        Some(v) => format!(" {}", dump(ast, *v)),
        None => String::new(),
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
fn list_and_dict_literals() {
    // Lists (trailing comma allowed; `[]` empty).
    assert_eq!(ast_of("[]"), "(list)");
    assert_eq!(ast_of("[1, 2, 3]"), "(list 1 2 3)");
    assert_eq!(ast_of("[1, 2,]"), "(list 1 2)");
    assert_eq!(ast_of("[1 + 2, f(x)]"), "(list (+ 1 2) (call f x))");
    assert_eq!(ast_of("[[1], [2]]"), "(list (list 1) (list 2))");
    // Dicts: a bare-word key is a string key (L§4.8); `{}` is the empty dict.
    assert_eq!(ast_of("{}"), "(dict)");
    assert_eq!(ast_of("{name: \"Alice\"}"), "(dict name:(str \"Alice\"))");
    assert_eq!(ast_of("{a: 1, b: 2,}"), "(dict a:1 b:2)");
    // Computed keys are expressions followed by `:`.
    assert_eq!(ast_of("{\"k\": 1}"), "(dict [(str \"k\")]:1)");
    assert_eq!(ast_of("{1 + 1: v}"), "(dict [(+ 1 1)]:v)");
    // A literal composes with postfix (indexing).
    assert_eq!(ast_of("[1, 2][0]"), "([] (list 1 2) 0)");
    // Recovery.
    assert_eq!(diags_of("[1, 2"), vec!["syntax-error"]);
    assert_eq!(diags_of("{a: 1"), vec!["syntax-error"]);
    assert!(diags_of("{a}").contains(&"syntax-error")); // missing `: value`
    assert!(diags_of("[1, 2, 3]").is_empty());
    assert!(diags_of("{a: 1, b: 2}").is_empty());
    // A stray closer isn't swallowed by the inner expression, so an enclosing
    // list recovers to the right shape (a broken dict, then `2`).
    assert_eq!(ast_of("[{a}, 2]"), "(list (dict [a]:<error>) 2)");
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

#[test]
fn statements_and_separators() {
    // A module is a sequence of statements; a newline or `;` separates them.
    assert_eq!(
        program_of("let x = 1\nx = x + 1"),
        "(module (let x 1) (assign x (+ x 1)))"
    );
    assert_eq!(program_of("f()\ng()"), "(module (call f) (call g))");
    assert_eq!(program_of("a; b"), "(module a b)");
    // Blank lines and leading/trailing separators are fine; the empty program
    // is an empty module (no error).
    assert_eq!(program_of("\n\nx\n\n"), "(module x)");
    assert_eq!(program_of(""), "(module)");
    assert!(prog_diags("let x = 1").is_empty());
    // Two statements on one line without a separator is an error (both still
    // parse, for recovery).
    assert_eq!(prog_diags("f() g()"), vec!["syntax-error"]);
    assert_eq!(program_of("f() g()"), "(module (call f) (call g))");
}

#[test]
fn binding_and_assignment_forms() {
    assert_eq!(program_of("let x = 1"), "(module (let x 1))");
    assert_eq!(program_of("const y = 2 + 3"), "(module (const y (+ 2 3)))");
    assert_eq!(program_of("a.b = c"), "(module (assign (. a b) c))");
    assert_eq!(program_of("a[i] = v"), "(module (assign ([] a i) v))");
    // Assignment is a statement, not an expression, so `==` still compares.
    assert_eq!(program_of("a == b"), "(module (== a b))");
    // Assigning to a non-lvalue is a static error (recovers to an assign shape).
    assert_eq!(prog_diags("1 = 2"), vec!["syntax-error"]);
    assert_eq!(prog_diags("f() = 2"), vec!["syntax-error"]);
    // A `let` missing its name or `=` recovers.
    assert!(prog_diags("let = 1").contains(&"syntax-error"));
    assert!(prog_diags("let x 1").contains(&"syntax-error"));
}

#[test]
fn if_forms() {
    // A body is a block (its own scope, L§5.4), so a single statement still
    // renders `(block …)`.
    assert_eq!(program_of("if a then b end"), "(module (if (a (block b))))");
    // `else if` flattens into arms; a trailing `else` is the else body.
    assert_eq!(
        program_of("if a then b else if c then d else e end"),
        "(module (if (a (block b)) (c (block d)) else (block e)))"
    );
    // The same `if` node serves expression position (here, a `let` initializer).
    assert_eq!(
        program_of("let x = if a then 1 else 2 end"),
        "(module (let x (if (a (block 1)) else (block 2))))"
    );
    // A multi-statement arm body.
    assert_eq!(
        program_of("if a then\n  f()\n  g()\nend"),
        "(module (if (a (block (call f) (call g)))))"
    );
    // `else` binds to the nearest open `if`: with two `end`s, the `else y` is
    // the inner `if`'s, and the outer `if` has no else.
    assert_eq!(
        program_of("if a then if b then x else y end end"),
        "(module (if (a (block (if (b (block x)) else (block y))))))"
    );
    // With only one `end`, the `else` still binds inward, leaving the outer `if`
    // unterminated — an error, not a dangling-else ambiguity.
    assert!(prog_diags("if a then if b then x else y end").contains(&"syntax-error"));
}

#[test]
fn loop_while_with_try() {
    assert_eq!(
        program_of("while a do b end"),
        "(module (while a (block b)))"
    );
    assert_eq!(
        program_of("loop do f() end"),
        "(module (loop (block (call f))))"
    );
    assert_eq!(
        program_of("with pen = red do draw() end"),
        "(module (with pen red (block (call draw))))"
    );
    assert_eq!(
        program_of("try risky() rescue e handle(e) end"),
        "(module (try (block (call risky)) rescue e (block (call handle e))))"
    );
}

#[test]
fn nonlocal_exits() {
    assert_eq!(program_of("return"), "(module (return))");
    assert_eq!(program_of("return n * 2"), "(module (return (* n 2)))");
    assert_eq!(program_of("break"), "(module (break))");
    assert_eq!(program_of("continue x"), "(module (continue x))");
    assert_eq!(program_of("raise err"), "(module (raise err))");
    // A bare exit before a separator takes no operand.
    assert_eq!(program_of("return\nx"), "(module (return) x)");
}

#[test]
fn s4_header_parses_in_no_trailing_block_mode() {
    // The `do … end` after the condition opens the `while` body, not a block
    // argument to the call `f()` (S-4) — so this is a clean while with body
    // `g()`, no diagnostics.
    assert_eq!(
        program_of("while f() do g() end"),
        "(module (while (call f) (block (call g))))"
    );
    assert!(prog_diags("while f() do g() end").is_empty());
    // A second, dangling `do … end` has nothing to attach to → the S-4 error.
    assert!(prog_diags("while f() do g() end\ndo h() end").contains(&"syntax-error"));
}

#[test]
fn statements_recover_and_bound_depth() {
    // A missing `end` is reported, not looped or panicked on.
    assert!(prog_diags("if a then b").contains(&"syntax-error"));
    assert!(prog_diags("while a do b").contains(&"syntax-error"));
    assert!(prog_diags("try a rescue e b").contains(&"syntax-error"));
    // Deeply nested bodies bail with a diagnostic, never a stack overflow
    // (bodies and expressions share the one depth budget).
    let deep = format!("{}x{}", "if a then ".repeat(5000), " end".repeat(5000));
    assert!(prog_diags(&deep).contains(&"syntax-error"));
}

#[test]
fn callable_declarations() {
    assert_eq!(
        program_of("to greet(name)\n  show(name)\nend"),
        "(module (to greet (params name) (block (call show name))))"
    );
    assert_eq!(
        program_of("fn double(n)\n  n * 2\nend"),
        "(module (fn double (params n) (block (* n 2))))"
    );
    // Zero parameters (parens still required).
    assert_eq!(
        program_of("to home() end"),
        "(module (to home (params) (block)))"
    );
    // Defaults, and a trailing block parameter (L§8.2).
    assert_eq!(
        program_of("fn f(a, b = 2, do body) end"),
        "(module (fn f (params a b=2 do:body) (block)))"
    );
}

#[test]
fn anonymous_functions() {
    // Anonymous `fn` is an expression (here a `let` initializer); it has no name.
    assert_eq!(
        program_of("let double = fn(x) x * 2 end"),
        "(module (let double (fn (params x) (block (* x 2)))))"
    );
    // `fn(…)` in statement position is the anonymous form (no name lookahead);
    // `fn name(…)` is a declaration.
    assert_eq!(program_of("fn() 1 end"), "(module (fn (params) (block 1)))");
    assert_eq!(
        program_of("fn g() 1 end"),
        "(module (fn g (params) (block 1)))"
    );
}

#[test]
fn declarations_nest_and_recover() {
    // `to`/`fn` may nest in any body (L§7.1).
    assert_eq!(
        program_of("to outer()\n  fn inner() 1 end\nend"),
        "(module (to outer (params) (block (fn inner (params) (block 1)))))"
    );
    assert!(prog_diags("to f(a, b) g(a) end").is_empty());
    // A block parameter must be the last parameter (L§8.2).
    assert!(prog_diags("fn f(do b, x) end").contains(&"syntax-error"));
    // Missing `)` / `(` / name recover without panic.
    assert!(prog_diags("to f(a end").contains(&"syntax-error"));
    assert!(prog_diags("to () end").contains(&"syntax-error"));
    assert!(prog_diags("fn 1() end").contains(&"syntax-error"));
}

#[test]
fn record_declarations() {
    assert_eq!(
        program_of("record Point with x, y end"),
        "(module (record Point (fields x y)))"
    );
    assert_eq!(
        program_of("ref record Turtle with position, heading, pen_down end"),
        "(module (ref-record Turtle (fields position heading pen_down)))"
    );
    // A docstring-only body is captured (rendered as a `doc` marker).
    assert_eq!(
        program_of("record P with x\n  \"A point.\"\nend"),
        "(module (record P (fields x) doc))"
    );
    assert!(prog_diags("record Point with x, y end").is_empty());
    // A record body may contain only a docstring (L§9.1).
    assert!(prog_diags("record P with x\n  compute()\nend").contains(&"syntax-error"));
}

#[test]
fn protocol_declarations() {
    // A required member (empty body + its own `end`) and a default member.
    assert_eq!(
        program_of("protocol Iterable\n  to each(self, do body) end\nend"),
        "(module (protocol Iterable (req to each (params self do:body))))"
    );
    assert_eq!(
        program_of("protocol Sized\n  fn size(self)\n    return 0\n  end\nend"),
        "(module (protocol Sized (def fn size (params self) (block (return 0)))))"
    );
    // `extends` and a leading docstring; a required member with its own `end`.
    assert_eq!(
        program_of("protocol Child extends Parent\n  \"Doc.\"\n  to m(self) end\nend"),
        "(module (protocol Child extends:Parent doc (req to m (params self))))"
    );
    // An empty protocol.
    assert_eq!(
        program_of("protocol Empty end"),
        "(module (protocol Empty))"
    );
    assert!(prog_diags("protocol Iterable\n  to each(self, do body) end\nend").is_empty());
}

#[test]
fn implement_declarations() {
    assert_eq!(
        program_of(
            "implement Iterable for Range\n  to each(r, do body)\n    body(r.start)\n  end\nend"
        ),
        "(module (implement Iterable for Range \
         (to each (params r do:body) (block (call body (. r start))))))"
    );
    assert!(prog_diags("implement P for T\n  fn m(self) 1 end\nend").is_empty());
    // Missing `for` recovers.
    assert!(prog_diags("implement P T end").contains(&"syntax-error"));
}

#[test]
fn module_parameter_exports_declarations() {
    assert_eq!(
        program_of("parameter pen_color = \"black\""),
        "(module (parameter pen_color (str \"black\")))"
    );
    assert_eq!(program_of("exports a, b, c"), "(module (exports a b c))");
    assert_eq!(
        program_of("module Geometry\n  record Point with x, y end\nend"),
        "(module (moddecl Geometry (block (record Point (fields x y)))))"
    );
    // A `module` may carry a docstring; its contents are module-level.
    assert_eq!(
        program_of("module M\n  \"A module.\"\n  let x = 1\nend"),
        "(module (moddecl M doc (block (let x 1))))"
    );
    assert!(prog_diags("parameter p = 0").is_empty());
    assert!(prog_diags("exports a, b").is_empty());
}

#[test]
fn import_forms_l112() {
    // The five target forms (L§11.2). The parser records the dotted path only;
    // module-vs-member is a load-time question (S-7).
    assert_eq!(program_of("import shapes"), "(module (import shapes))");
    assert_eq!(
        program_of("import shapes as s"),
        "(module (import shapes:s))"
    );
    assert_eq!(
        program_of("import shapes.circle"),
        "(module (import shapes.circle))"
    );
    assert_eq!(
        program_of("import shapes.circle as c"),
        "(module (import shapes.circle:c))"
    );
    assert_eq!(program_of("import shapes.*"), "(module (import shapes.*))");
    // Comma-separated targets, and a multi-segment path.
    assert_eq!(
        program_of("import shapes.circle, shapes.square as sq, colors"),
        "(module (import shapes.circle shapes.square:sq colors))"
    );
    assert_eq!(program_of("import a.b.c"), "(module (import a.b.c))");
    assert!(prog_diags("import shapes.circle, colors").is_empty());
    // `.*` may not be renamed with `as`.
    assert!(prog_diags("import shapes.* as s").contains(&"syntax-error"));
    // Missing name after `.` recovers.
    assert!(prog_diags("import shapes.").contains(&"syntax-error"));
}

#[test]
fn module_level_only_placement_rules_l71() {
    // record/protocol/implement/module/parameter/exports/import may appear only
    // at module level (L§7.1); nested in a body it is a static error (still
    // parsed).
    for src in [
        "to f()\n  record R with x end\nend",
        "to f()\n  import shapes\nend",
        "if c then\n  parameter p = 0\nend",
        "while c do\n  exports a\nend",
        "to f()\n  module M end\nend",
        "loop do\n  protocol P end\nend",
    ] {
        assert!(
            prog_diags(src).contains(&"syntax-error"),
            "a nested module-level declaration should error: {src:?}"
        );
    }
    // `let`/`const`/`to`/`fn` nest fine, and a `module`'s contents are
    // module-level, so a record inside a `module` is allowed.
    assert!(prog_diags("to f()\n  let x = 1\n  fn g() 1 end\nend").is_empty());
    assert!(prog_diags("module M\n  record R with x end\nend").is_empty());
}

#[test]
fn docstrings_classified_per_s27() {
    // A `to` body: a leading string is always the docstring (S-27).
    assert_eq!(
        program_of("to stub()\n  \"TODO.\"\nend"),
        "(module (to stub doc (params) (block)))"
    );
    // A module docstring: a leading string in the file.
    assert_eq!(
        program_of("\"Module doc.\"\nlet x = 1"),
        "(module doc (let x 1))"
    );
    // An `fn` body: a LONE string is the RESULT, not a docstring.
    assert_eq!(
        program_of("fn greeting()\n  \"hello\"\nend"),
        "(module (fn greeting (params) (block (str \"hello\"))))"
    );
    // An `fn` body: a leading string FOLLOWED by a statement is the docstring.
    assert_eq!(
        program_of("fn f()\n  \"Doc.\"\n  compute()\nend"),
        "(module (fn f doc (params) (block (call compute))))"
    );
    // doc + result: the first string is the docstring, the second the result.
    assert_eq!(
        program_of("fn f()\n  \"Doc.\"\n  \"result\"\nend"),
        "(module (fn f doc (params) (block (str \"result\"))))"
    );
}

#[test]
fn docstring_rawness_follows_classification_s27() {
    // An `fn`'s lone-string RESULT is an ordinary literal, so its interpolation
    // is parsed (and evaluates at run time).
    assert_eq!(
        program_of("fn f()\n  \"hi {name}\"\nend"),
        "(module (fn f (params) (block (str \"hi \" {name}))))"
    );
    // The same string as a DOCSTRING (followed) is captured as a raw span; its
    // `{ … }` is not a parsed part of the tree.
    assert_eq!(
        program_of("fn g()\n  \"hi {name}\"\n  act()\nend"),
        "(module (fn g doc (params) (block (call act))))"
    );
}

#[test]
fn docstring_interpolation_is_raw_never_parsed_s27() {
    // A docstring's `{ … }` is inert text (L§8.6): even when it is NOT a single
    // valid expression it must be captured raw, never parsed — no spurious
    // errors, no desync into the following statements. (Regression: a post-parse
    // extractor parsed the interpolation as code; a `{name}`-only test missed it.)
    for src in [
        "to f()\n  \"Returns {the answer} to life.\"\n  return 1\nend",
        "\"Docs about {the whole module}.\"\nlet x = 1",
        "fn g()\n  \"the set {1, 2, 3}\"\n  act()\nend",
        "record R with a\n  \"Returns {the answer}.\"\nend",
    ] {
        assert!(
            prog_diags(src).is_empty(),
            "a raw docstring must not be parsed as code: {src:?}"
        );
    }
    // The docstring is captured and the body is exactly the following statement.
    assert_eq!(
        program_of("to f()\n  \"Returns {the answer}.\"\n  return 1\nend"),
        "(module (to f doc (params) (block (return 1))))"
    );
}

#[test]
fn parenthesized_lvalue_is_allowed() {
    // Confirmed by the spec author: redundant parens around an assignment target
    // are fine (parens are transparent, so the target is the inner lvalue).
    assert_eq!(program_of("(a) = c"), "(module (assign a c))");
    assert!(prog_diags("(a) = c").is_empty());
    assert_eq!(program_of("(a.b) = c"), "(module (assign (. a b) c))");
}

#[test]
fn protocol_members_each_carry_their_own_end_s52() {
    // Every member is terminated by its own `end` (S-52): an empty body is a
    // required member, a non-empty body a default. Two members, each with `end`.
    assert_eq!(
        program_of("protocol P\n  to a(self) end\n  fn b(self) return 1 end\nend"),
        "(module (protocol P (req to a (params self)) \
         (def fn b (params self) (block (return 1)))))"
    );
    // A required member may carry a docstring (still an empty body).
    assert_eq!(
        program_of("protocol P\n  to m(self)\n    \"Does m.\"\n  end\nend"),
        "(module (protocol P (req to m doc (params self))))"
    );
    // A bare signature (no member `end`) eats the protocol's `end` and closes it
    // early → a targeted error naming the member-`end` requirement.
    let nfc = normalize("protocol P\n  to a(self)\nend");
    let p = parse_program(nfc.as_ref(), M);
    assert!(
        p.diagnostics
            .iter()
            .any(|d| d.message.contains("needs its own `end`"))
    );
}

#[test]
fn docstring_span_captures_the_raw_literal() {
    // The captured docstring span is the raw string literal; a docstring's
    // interpolation is NOT parsed (S-27), so the span still covers `{x}` as text.
    let src = "record P with x\n  \"A point.\"\nend";
    let nfc = normalize(src);
    let doc = record_doc_span(&parse_program(nfc.as_ref(), M).ast).expect("has a docstring");
    assert_eq!(&nfc[doc.start as usize..doc.end as usize], "\"A point.\"");

    let src2 = "record Q with x\n  \"Value {x}.\"\nend";
    let nfc2 = normalize(src2);
    let doc2 = record_doc_span(&parse_program(nfc2.as_ref(), M).ast).expect("has a docstring");
    assert_eq!(
        &nfc2[doc2.start as usize..doc2.end as usize],
        "\"Value {x}.\""
    );
}

/// The docstring span of the first top-level record, if any.
fn record_doc_span(ast: &Ast) -> Option<Span> {
    let root = ast.root()?;
    let Node::Module { stmts, .. } = ast.node(root) else {
        return None;
    };
    stmts.iter().find_map(|&s| match ast.node(s) {
        Node::Record { doc, .. } => *doc,
        _ => None,
    })
}
