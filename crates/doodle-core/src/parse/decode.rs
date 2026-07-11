//! Value decoding for string and bytes literals (M1.6): a lexer-validated text
//! chunk (`StrText`) or bytes-literal interior → its runtime value, applying the
//! closed escape set (L§3.6.3/§3.6.5). `\xHH` is code point `U+00HH` in a string
//! and byte `0xHH` in bytes; `{{`/`}}` collapse to `{`/`}` in a string.
//!
//! The lexer already diagnosed malformed escapes, so decoding is best-effort and
//! never panics on ill-formed input. The one thing it reports is a **chunk-final
//! backslash** — a line-final `\` in a triple-quoted string, which the closed
//! set makes an error (L§3.6.3) since it is neither a valid escape nor, under
//! S-3, a line continuation.

const REPLACEMENT: char = '\u{FFFD}';

/// Decodes a string-literal text run: the offset returned is a chunk-final `\`,
/// if any (reported by the caller).
pub(super) fn decode_text(raw: &str) -> (String, Option<usize>) {
    let b = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut dangling = None;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => match escape_char(b, raw, i) {
                Some((ch, next)) => {
                    out.push(ch);
                    i = next;
                }
                None => {
                    dangling = Some(i); // a chunk-final `\`
                    i += 1;
                }
            },
            b'{' if b.get(i + 1) == Some(&b'{') => {
                out.push('{');
                i += 2;
            }
            b'}' if b.get(i + 1) == Some(&b'}') => {
                out.push('}');
                i += 2;
            }
            _ => {
                let ch = raw[i..].chars().next().unwrap_or(REPLACEMENT);
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    (out, dangling)
}

/// Decodes a bytes literal's content, given the text after the `b"` opener.
/// Stops at the closing `"` (an *unescaped* `"`, since `\"` is consumed as an
/// escape), so a closed, unterminated, or `\"`-ending literal all decode right
/// without any suffix guessing.
pub(super) fn decode_bytes(raw: &str) -> (Vec<u8>, Option<usize>) {
    let b = raw.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut dangling = None;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => break, // the closing quote
            b'\\' => match escape_byte(b, i) {
                Some((byte, next)) => {
                    out.push(byte);
                    i = next;
                }
                None => {
                    dangling = Some(i);
                    i += 1;
                }
            },
            // ASCII source (lexer-validated); braces are literal in bytes.
            _ => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    (out, dangling)
}

/// Decodes one string escape at `bs` (a `\`), or `None` if the `\` is chunk-final.
fn escape_char(b: &[u8], raw: &str, bs: usize) -> Option<(char, usize)> {
    let &c = b.get(bs + 1)?;
    Some(match c {
        b'"' => ('"', bs + 2),
        b'\\' => ('\\', bs + 2),
        b'n' => ('\n', bs + 2),
        b't' => ('\t', bs + 2),
        b'r' => ('\r', bs + 2),
        b'0' => ('\0', bs + 2),
        // Advance only past the hex digits actually present (0–2): a malformed
        // `\x` (already lexer-diagnosed) must not over-run into the next
        // character, or a later `&str` slice could land off a UTF-8 boundary.
        b'x' => (from_u32(two_hex(b, bs + 2)), bs + 2 + hex_run(b, bs + 2)),
        b'u' => {
            let (value, end) = brace_hex(b, bs + 2);
            (from_u32(value), end)
        }
        // Unknown escape — the lexer diagnosed it; keep the escaped character.
        _ => {
            let ch = raw[bs + 1..].chars().next().unwrap_or(REPLACEMENT);
            (ch, bs + 1 + ch.len_utf8())
        }
    })
}

/// Decodes one bytes escape at `bs` (a `\`), or `None` if `\` is chunk-final.
fn escape_byte(b: &[u8], bs: usize) -> Option<(u8, usize)> {
    let &c = b.get(bs + 1)?;
    Some(match c {
        b'"' => (b'"', bs + 2),
        b'\\' => (b'\\', bs + 2),
        b'n' => (b'\n', bs + 2),
        b't' => (b'\t', bs + 2),
        b'r' => (b'\r', bs + 2),
        b'0' => (0, bs + 2),
        b'x' => (
            (two_hex(b, bs + 2) & 0xFF) as u8,
            bs + 2 + hex_run(b, bs + 2),
        ),
        // `\u` is invalid in bytes (lexer-diagnosed); keep the `u`.
        _ => (c, bs + 2),
    })
}

fn two_hex(b: &[u8], i: usize) -> u32 {
    let hi = b.get(i).and_then(hexval).unwrap_or(0);
    let lo = b.get(i + 1).and_then(hexval).unwrap_or(0);
    hi * 16 + lo
}

/// The number of hex digits (0, 1, or 2) present at `i`, `i+1`.
fn hex_run(b: &[u8], i: usize) -> usize {
    let one = b.get(i).and_then(hexval).is_some();
    let two = one && b.get(i + 1).and_then(hexval).is_some();
    usize::from(one) + usize::from(two)
}

/// Reads `{ hex… }` at `i` (a `{`), returning the value and the offset past `}`.
fn brace_hex(b: &[u8], i: usize) -> (u32, usize) {
    let mut j = i + 1;
    let mut value = 0u32;
    while let Some(&c) = b.get(j) {
        if c == b'}' {
            return (value, j + 1);
        }
        value = value
            .saturating_mul(16)
            .saturating_add(hexval(&c).unwrap_or(0));
        j += 1;
    }
    (value, j)
}

fn hexval(c: &u8) -> Option<u32> {
    (*c as char).to_digit(16)
}

fn from_u32(value: u32) -> char {
    char::from_u32(value).unwrap_or(REPLACEMENT)
}
