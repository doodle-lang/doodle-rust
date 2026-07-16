//! String and bytes literal assembly (L§3.6.3/§3.6.4/§3.6.5). A string literal
//! arrives as the structured token stream `StrStart (StrText | interpolation)*
//! StrEnd`; this module stitches the decoded text runs and parsed `{ … }`
//! interpolations into a [`Node::StrLit`], and decodes a `b"…"` literal to its
//! byte sequence. The escape/`{{`/margin decoding itself lives in
//! [`super::decode`]; this is the parser-side assembly, parallel to
//! [`super::collection`] for `[]`/`{}` literals.

use super::decode;
use crate::ast::{Node, NodeId, StrPart};
use crate::lex::TokenKind;
use crate::span::Span;

impl super::Parser<'_> {
    /// Assembles a string literal from the structured stream `StrStart (StrText
    /// | interpolation)* StrEnd`, decoding escapes and parsing each `{ … }`.
    /// Adjacent decoded text (including triple-quoted `\n`-join chunks) merges
    /// into one `Text` part.
    pub(super) fn string_lit(&mut self, start_span: Span) -> NodeId {
        let source = self.source;
        // A chunk-final `\` is only a real error in a triple-quoted string (a
        // line-final backslash, S-49 × S-3). In a single-line string it can only
        // arise from `\` before the terminating newline, which the lexer already
        // reports as unterminated-string — that diagnostic takes precedence.
        let is_triple = start_span.end - start_span.start == 3;
        self.advance(); // StrStart
        let mut parts = Vec::new();
        let mut acc = String::new();
        let mut end = start_span.end;
        loop {
            match self.peek_kind() {
                Some(TokenKind::StrText) => {
                    let sp = self.peek_span();
                    self.advance();
                    let (text, dangling) =
                        decode::decode_text(&source[sp.start as usize..sp.end as usize]);
                    if let Some(off) = dangling.filter(|_| is_triple) {
                        let at = sp.start + off as u32;
                        self.error(
                            Span::new(at, at + 1),
                            "a backslash can't end a line — write `\\\\` for a literal \
                             backslash; Doodle doesn't join lines with `\\` (each line of a \
                             multi-line string is its own line in the value)",
                        );
                    }
                    acc.push_str(&text);
                }
                Some(TokenKind::InterpStart) => {
                    flush_text(&mut parts, &mut acc);
                    self.advance();
                    // An empty interpolation was already diagnosed by the lexer;
                    // skip it rather than pile on an "expected expression".
                    if matches!(self.peek_kind(), Some(TokenKind::InterpEnd)) {
                        self.advance();
                    } else {
                        let expr = self.delimited(|p| p.expr(0));
                        if matches!(self.peek_kind(), Some(TokenKind::InterpEnd)) {
                            self.advance();
                        } else {
                            let sp = self.peek_span();
                            self.error(sp, "expected `}` to close this interpolation");
                        }
                        parts.push(StrPart::Interp(expr));
                    }
                }
                Some(TokenKind::StrEnd) => {
                    end = self.peek_span().end;
                    self.advance();
                    break;
                }
                // The lexer always balances StrStart/StrEnd (a synthetic StrEnd
                // even for an unterminated string), so this stops rather than
                // loops on an otherwise-impossible malformed stream.
                _ => break,
            }
        }
        flush_text(&mut parts, &mut acc);
        self.push(Node::StrLit(parts), Span::new(start_span.start, end))
    }

    /// Decodes a bytes literal `b"…"` to its byte sequence.
    pub(super) fn decode_bytes_literal(&mut self, span: Span) -> Node {
        let text = &self.source[span.start as usize..span.end as usize];
        let inner = text.strip_prefix("b\"").unwrap_or(text);
        let (bytes, dangling) = decode::decode_bytes(inner);
        if let Some(off) = dangling {
            let at = span.start + 2 + off as u32; // past the `b"` prefix
            self.error(
                Span::new(at, at + 1),
                "a backslash here isn't a valid escape",
            );
        }
        Node::BytesLit(bytes)
    }
}

/// Flushes accumulated decoded text into a `Text` part, if any.
fn flush_text(parts: &mut Vec<StrPart>, acc: &mut String) {
    if !acc.is_empty() {
        parts.push(StrPart::Text(std::mem::take(acc).into()));
    }
}
