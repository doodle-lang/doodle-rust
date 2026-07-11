//! Lexer tests: token-dump snapshots (the authoritative S-2 continuation
//! evidence — a mis-emitted NEWLINE is invisible at `stage: lex` conformance)
//! plus unit checks for keywords, operator munch, numbers, strings, and idents.

use doodle_core::lex::{Keyword, TokenKind, lex};
use doodle_core::source::{LineIndex, normalize};
use doodle_core::span::ModuleId;

const M: ModuleId = ModuleId(0);

/// A readable token dump: `kind  "text" @ line:col`, then any diagnostics.
fn dump(source: &str) -> String {
    let nfc = normalize(source);
    let lexed = lex(nfc.as_ref(), M);
    let index = LineIndex::new(nfc.as_ref());
    let mut out = String::new();
    for t in &lexed.tokens {
        let p = index.position_at(nfc.as_ref(), t.span.start);
        let text = &nfc[t.span.start as usize..t.span.end as usize];
        out.push_str(&format!(
            "{:<20} {text:?} @ {}:{}\n",
            format!("{:?}", t.kind),
            p.line,
            p.column
        ));
    }
    for d in &lexed.diagnostics {
        let p = d.span.map(|s| index.position_at(nfc.as_ref(), s.start));
        out.push_str(&format!("! {} @ {p:?}: {}\n", d.code.slug(), d.message));
    }
    out
}

fn kinds(source: &str) -> Vec<TokenKind> {
    let nfc = normalize(source);
    lex(nfc.as_ref(), M).tokens.iter().map(|t| t.kind).collect()
}

fn diag_codes(source: &str) -> Vec<&'static str> {
    let nfc = normalize(source);
    lex(nfc.as_ref(), M)
        .diagnostics
        .iter()
        .map(|d| d.code.slug())
        .collect()
}

// --- Snapshots: S-2 continuation ---

#[test]
fn continuation_triggers_suppress_the_newline() {
    // Each line ends with a continuation trigger, so no NEWLINE is emitted.
    let lines = [
        "a +", "b -", "c *", "d /", "e //", "f %", "g **", "h ==", "i !=", "j <", "k >", "l <=",
        "m >=", "n ,", "o and", "p or", "q is", "r",
    ];
    insta::assert_snapshot!(dump(&lines.join("\n")));
}

#[test]
fn non_triggers_end_the_statement() {
    // `=`, `.`, `:`, `not`, a closer, an identifier, and a literal at line end
    // each emit a NEWLINE.
    let source = "a =\nb .\nc :\nd not\n) \ne\n1\n";
    insta::assert_snapshot!(dump(source));
}

#[test]
fn brackets_suppress_newlines() {
    let source = "f(\n  a,\n  b\n)\n";
    insta::assert_snapshot!(dump(source));
}

#[test]
fn a_trailing_comment_is_transparent_to_continuation() {
    // `a +  # note` continues to `b`: one statement, no NEWLINE between.
    let source = "a +   # keep going\nb\n";
    insta::assert_snapshot!(dump(source));
}

#[test]
fn a_leading_operator_does_not_join_the_previous_line() {
    // `a` ends a statement (NEWLINE), then `+ b` is a new one.
    let source = "a\n+ b\n";
    insta::assert_snapshot!(dump(source));
}

#[test]
fn numbers_operators_and_a_shebang() {
    let source = "#!/usr/bin/env doodle\nlet x = 0xFF + 3.14 // 2 ** n\n";
    insta::assert_snapshot!(dump(source));
}

#[test]
fn continuation_holds_across_blank_and_comment_lines() {
    // A continuation trigger keeps holding through blank lines and comment-only
    // lines (they leave `last_significant` untouched): `a + b`, one statement.
    let source = "a +\n\n   # note\n\nb\n";
    insta::assert_snapshot!(dump(source));
}

// --- Unit tests ---

#[test]
fn every_keyword_is_recognized_and_near_misses_are_identifiers() {
    for (word, kw) in [
        ("and", Keyword::And),
        ("fn", Keyword::Fn),
        ("to", Keyword::To),
        ("end", Keyword::End),
        ("return", Keyword::Return),
        ("next", Keyword::Next),
        ("use", Keyword::Use),
    ] {
        assert_eq!(kinds(word), vec![TokenKind::Keyword(kw), TokenKind::Eof]);
    }
    for word in ["ends", "Self", "print", "fnx", "то"] {
        assert_eq!(kinds(word), vec![TokenKind::Ident, TokenKind::Eof]);
    }
}

#[test]
fn operators_use_maximal_munch() {
    use TokenKind::{BangEq, Eq, EqEq, Ge, Gt, Le, Lt, Slash, SlashSlash, Star, StarStar};
    assert_eq!(kinds("/ //"), vec![Slash, SlashSlash, TokenKind::Eof]);
    assert_eq!(kinds("* **"), vec![Star, StarStar, TokenKind::Eof]);
    assert_eq!(kinds("= =="), vec![Eq, EqEq, TokenKind::Eof]);
    assert_eq!(kinds("< <= > >="), vec![Lt, Le, Gt, Ge, TokenKind::Eof]);
    assert_eq!(kinds("!="), vec![BangEq, TokenKind::Eof]);
    assert_eq!(diag_codes("!"), vec!["unexpected-character"]); // bare `!`
}

#[test]
fn semicolons_and_closers_are_tokens() {
    use TokenKind::{Eof, Ident, RBrace, RBracket, RParen, Semicolon};
    // A `;` is an explicit separator token; consecutive statements on a line.
    assert_eq!(kinds("a; b"), vec![Ident, Semicolon, Ident, Eof]);
    // Closers are tokens even when unmatched (the parser diagnoses mismatch).
    assert_eq!(kinds(")]}"), vec![RParen, RBracket, RBrace, Eof]);
}

#[test]
fn number_shapes() {
    use TokenKind::{Dot, Eof, Float, Ident, Int};
    assert_eq!(kinds("42"), vec![Int, Eof]);
    assert_eq!(kinds("1_000_000"), vec![Int, Eof]);
    assert_eq!(kinds("0xdead_beef"), vec![Int, Eof]);
    assert_eq!(kinds("0b1010 0o755"), vec![Int, Int, Eof]);
    assert_eq!(kinds("3.14"), vec![Float, Eof]);
    assert_eq!(kinds("1e6 1.5e-3"), vec![Float, Float, Eof]);
    // `2.field` is `2` `.` `field`, not a float; `0XFF` is `0` then `XFF`.
    assert_eq!(kinds("2.field"), vec![Int, Dot, Ident, Eof]);
    assert_eq!(kinds("0XFF"), vec![Int, Ident, Eof]);
    assert!(diag_codes("42 3.14 0xFF 2.field 0XFF").is_empty());
}

#[test]
fn malformed_numbers_are_diagnosed() {
    for bad in [
        "1_", "1__0", "0x_FF", "0x", "0xG", "1e", "1e+", "1_.5", "1.5_", "1e1_",
    ] {
        assert_eq!(
            diag_codes(bad),
            vec!["malformed-number"],
            "expected malformed-number for {bad:?}"
        );
    }
}

#[test]
fn strings() {
    use TokenKind::{Eof, Str};
    assert_eq!(kinds("\"hello\""), vec![Str, Eof]);
    assert!(diag_codes("\"hello\"").is_empty());
    // An escaped quote does not close the string.
    assert_eq!(kinds("\"a\\\"b\""), vec![Str, Eof]);
    // Unterminated at newline and at EOF.
    assert_eq!(diag_codes("\"oops\nx"), vec!["unterminated-string"]);
    assert_eq!(diag_codes("\"oops"), vec!["unterminated-string"]);
}

#[test]
fn identifiers_are_nfc_and_unicode() {
    use TokenKind::{Eof, Ident};
    assert_eq!(
        kinds("café θ длина _x x2"),
        vec![Ident, Ident, Ident, Ident, Ident, Eof]
    );
    // composed and decomposed é lex to the same single identifier.
    assert_eq!(kinds("caf\u{e9}"), kinds("cafe\u{301}"));
    // an emoji cannot start (or be) an identifier.
    assert_eq!(diag_codes("🐢"), vec!["unexpected-character"]);
}
