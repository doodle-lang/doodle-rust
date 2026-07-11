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

/// Concatenates the raw text of a string's `StrText` chunks — the pre-decode
/// value (escapes are lowered at M1.6), enough to check triple-quoted margin
/// stripping and inter-line joins.
fn str_text(source: &str) -> String {
    let nfc = normalize(source);
    lex(nfc.as_ref(), M)
        .tokens
        .iter()
        .filter(|t| t.kind == TokenKind::StrText)
        .map(|t| &nfc[t.span.start as usize..t.span.end as usize])
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
fn plain_strings_lex_as_a_stream() {
    use TokenKind::{Eof, StrEnd, StrStart, StrText};
    // A plain string is StrStart, one text run, StrEnd.
    assert_eq!(kinds("\"hello\""), vec![StrStart, StrText, StrEnd, Eof]);
    assert!(diag_codes("\"hello\"").is_empty());
    // Empty string: no text run.
    assert_eq!(kinds("\"\""), vec![StrStart, StrEnd, Eof]);
    // An escaped quote does not close the string; the whole body is one run.
    assert_eq!(kinds("\"a\\\"b\""), vec![StrStart, StrText, StrEnd, Eof]);
    assert!(diag_codes("\"a\\\"b\"").is_empty());
    // Unterminated at newline and at EOF, each once.
    assert_eq!(diag_codes("\"oops\nx"), vec!["unterminated-string"]);
    assert_eq!(diag_codes("\"oops"), vec!["unterminated-string"]);
}

#[test]
fn string_escapes_validate_shape() {
    // The whole closed set, valid, produces no diagnostics.
    assert!(diag_codes("\"\\n\\t\\r\\0\\\\\\\"\\x1b\\u{1F600}\\u{E9}\"").is_empty());
    // Unknown and malformed escapes are diagnosed, with distinct codes.
    assert_eq!(diag_codes("\"\\q\""), vec!["unknown-escape"]);
    assert_eq!(diag_codes("\"\\x1\""), vec!["malformed-escape"]); // one hex digit
    assert_eq!(diag_codes("\"\\uABCD\""), vec!["malformed-escape"]); // braceless
    assert_eq!(diag_codes("\"\\u{}\""), vec!["malformed-escape"]); // empty
    assert_eq!(diag_codes("\"\\u{1234567}\""), vec!["malformed-escape"]); // > 6 digits
    assert_eq!(diag_codes("\"\\u{D800}\""), vec!["malformed-escape"]); // surrogate
}

#[test]
fn interpolation_lexes_a_structured_stream() {
    use TokenKind::{
        Eof, Ident, InterpEnd, InterpStart, LBrace, RBrace, StrEnd, StrStart, StrText,
    };
    // Text, an interpolation, more text.
    assert_eq!(
        kinds("\"a{x}b\""),
        vec![
            StrStart,
            StrText,
            InterpStart,
            Ident,
            InterpEnd,
            StrText,
            StrEnd,
            Eof
        ]
    );
    // `{{` and `}}` are literal braces — one text run, no interpolation.
    assert_eq!(kinds("\"{{}}\""), vec![StrStart, StrText, StrEnd, Eof]);
    assert!(diag_codes("\"{{}}\"").is_empty());
    // A nested dict inside an interpolation is not "empty".
    assert_eq!(
        kinds("\"{ {} }\""),
        vec![
            StrStart,
            InterpStart,
            LBrace,
            RBrace,
            InterpEnd,
            StrEnd,
            Eof
        ]
    );
    assert!(diag_codes("\"{ {} }\"").is_empty());
    // A nested string inside an interpolation (recursion).
    assert_eq!(
        kinds("\"{ \"hi\" }\""),
        vec![
            StrStart,
            InterpStart,
            StrStart,
            StrText,
            StrEnd,
            InterpEnd,
            StrEnd,
            Eof
        ]
    );
}

#[test]
fn interpolation_errors() {
    // Empty and whitespace-only interpolations.
    assert_eq!(diag_codes("\"{}\""), vec!["empty-interpolation"]);
    assert_eq!(diag_codes("\"{  }\""), vec!["empty-interpolation"]);
    // A lexically bad character in the body is NOT "empty": it emits its own
    // error and no spurious empty-interpolation is piled on (regression — the
    // bad char is consumed by recovery without emitting a token).
    assert_eq!(diag_codes("\"{ @ }\""), vec!["unexpected-character"]);
    assert_eq!(diag_codes("\"{@}\""), vec!["unexpected-character"]);
    // A line terminator can't appear in an interpolation; reported once, and
    // recovery is bounded to the line (no separate unterminated-string).
    assert_eq!(diag_codes("\"{x\n"), vec!["unterminated-interpolation"]);
    // EOF inside an interpolation.
    assert_eq!(diag_codes("\"{x"), vec!["unterminated-interpolation"]);
}

#[test]
fn a_comment_inside_an_interpolation_is_an_error() {
    use TokenKind::{Eof, Ident, InterpEnd, InterpStart, StrEnd, StrStart};
    // S-50 (b): `#` inside an interpolation is a distinct error, not a comment
    // that would swallow the closing `}`; recovery still closes the interp so
    // there is no cascading unterminated error.
    assert_eq!(diag_codes("\"{ x #c }\""), vec!["comment-in-interpolation"]);
    assert_eq!(
        kinds("\"{ x #c }\""),
        vec![StrStart, InterpStart, Ident, InterpEnd, StrEnd, Eof]
    );
}

#[test]
fn bytes_literals() {
    use TokenKind::{Bytes, Eof, Ident};
    assert_eq!(kinds("b\"GET\\r\\n\""), vec![Bytes, Eof]);
    assert!(diag_codes("b\"\\x00\\xff\"").is_empty());
    // A bare `b` (or a longer word) is an identifier, not a bytes literal.
    assert_eq!(kinds("b"), vec![Ident, Eof]);
    assert_eq!(kinds("buffer"), vec![Ident, Eof]);
    // `\u` is not valid in bytes; source must be ASCII; no interpolation.
    assert_eq!(diag_codes("b\"\\u{41}\""), vec!["malformed-escape"]);
    assert_eq!(diag_codes("b\"caf\u{e9}\""), vec!["non-ascii-bytes"]);
    assert!(diag_codes("b\"a{x}b\"").is_empty()); // braces are literal in bytes
    assert_eq!(diag_codes("b\"oops"), vec!["unterminated-string"]);
}

#[test]
fn triple_quoted_strips_margin() {
    use TokenKind::{Eof, StrEnd, StrStart, StrText};
    // Margin = the closing """ indentation (4 spaces), stripped from each line;
    // content beyond the margin (the 2 extra spaces on "flour") is preserved.
    let src = "\"\"\"\n    Ingredients:\n      flour\n    \"\"\"";
    assert_eq!(str_text(src), "Ingredients:\n  flour");
    assert!(diag_codes(src).is_empty());
    // text, a \n-join chunk, text — wrapped in StrStart/StrEnd.
    assert_eq!(
        kinds(src),
        vec![StrStart, StrText, StrText, StrText, StrEnd, Eof]
    );
}

#[test]
fn triple_quoted_empty_and_whitespace_lines() {
    // A truly empty line contributes "" (no margin required).
    let empty = "\"\"\"\n    a\n\n    b\n    \"\"\"";
    assert_eq!(str_text(empty), "a\n\nb");
    assert!(diag_codes(empty).is_empty());
    // A whitespace-only *nonempty* line keeps its post-margin spaces (here the
    // middle line is margin + two spaces).
    let ws = "\"\"\"\n    a\n      \n    b\n    \"\"\"";
    assert_eq!(str_text(ws), "a\n  \nb");
    assert!(diag_codes(ws).is_empty());
}

#[test]
fn triple_quoted_interpolation_and_literal_quotes() {
    use TokenKind::{Eof, Ident, InterpEnd, InterpStart, StrEnd, StrStart, StrText};
    let interp = "\"\"\"\n    Hello {name}\n    \"\"\"";
    assert!(diag_codes(interp).is_empty());
    assert_eq!(
        kinds(interp),
        vec![
            StrStart,
            StrText,
            InterpStart,
            Ident,
            InterpEnd,
            StrEnd,
            Eof
        ]
    );
    // A mid-line """ is literal content, not the closing delimiter.
    let lit = "\"\"\"\n    a\"\"\"b\n    \"\"\"";
    assert_eq!(str_text(lit), "a\"\"\"b");
    assert!(diag_codes(lit).is_empty());
}

#[test]
fn triple_quoted_errors() {
    // An under-indented content line fails the margin match.
    assert_eq!(
        diag_codes("\"\"\"\n    a\n  b\n    \"\"\""),
        vec!["margin-mismatch"]
    );
    // A tab where the margin is spaces.
    assert_eq!(
        diag_codes("\"\"\"\n    a\n\tb\n    \"\"\""),
        vec!["margin-mismatch"]
    );
    // Nothing may follow the opening """ on its line.
    assert_eq!(
        diag_codes("\"\"\"x\n    a\n    \"\"\""),
        vec!["malformed-triple-quote"]
    );
    // No closing """: unterminated.
    assert_eq!(diag_codes("\"\"\"\n    a\n"), vec!["unterminated-string"]);
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
