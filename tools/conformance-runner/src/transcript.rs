//! The cross-surface **transcript oracle** (M7.5d, D-M7-20): a line-oriented, byte-exact record of
//! a `mode: run` / `mode: drive` fixture's execution — output bytes, capability request identity +
//! resolution, positions, outcome, and (drive) per-step stops with the live stack. It is committed
//! beside each fixture as `<entry>.transcript` and drift-checked (regen with `--write`, default
//! check FAILs on drift — the M1.12 lang-corpus-sync house pattern). Static-family fixtures have no
//! transcript (D-M7-20 scope: run/drive ⇒ transcript, static ⇒ `expect-*` only).
//!
//! **Grammar (`transcript v1`, normative in `conformance/README.md`).** A version line, a `mode:`
//! line, then tagged event lines. Positions are `<module>:<line>:<column>` with the module rendered
//! **entry-relative** (the entry module is `main`, imports by their canonical id) so a transcript
//! never depends on the working directory (D-M7-20 rider 4). A raise records its **kind (slug)** and
//! position, **never message text** (S-58 "messages are not API"); the Error's structured `details`
//! are a **designated v1.1 addition**, deferred until M9a stabilizes structural value serialization
//! (committing the current placeholder rendering would freeze it). Encoding rules:
//! - `out:` output bytes, control bytes / `\` / DEL escaped as `\xNN` (lowercase hex), the rest of
//!   the UTF-8 verbatim (the file stays UTF-8); output between events is coalesced into one run.
//! - values (`res:`) reuse the drive-script literal syntax: `"str"` (same `\xNN` escaping), an
//!   integer, a float, `true`/`false`, `nil`, or `raise "msg"` (a scripted resolution, not an engine
//!   message — the "never message text" rule is about *asserted* error text).
//! - an unknown line prefix is a **parse error** (the loud-fixture rule), never silently ignored.

use crate::model::{Mode, ScriptResponse, ScriptValue};

/// The version tag every transcript begins with; a future incompatible grammar bumps it.
pub(crate) const VERSION_LINE: &str = "transcript v1";

/// A recorded transcript: the fixture's mode and its ordered events.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct Transcript {
    pub(crate) mode: Mode,
    pub(crate) events: Vec<Event>,
}

/// A source position in a transcript: an **entry-relative** module id and 1-based line/column.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Pos {
    pub(crate) module: String,
    pub(crate) line: u32,
    pub(crate) column: u32,
}

/// One transcript event (one serialized line). `run` fixtures use `Out`/`Req`/`Res`/`Outcome`;
/// `drive` fixtures use `Step`/`Stop`/`Stack`.
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum Event {
    /// `out:` — a coalesced run of captured output bytes.
    Out(Vec<u8>),
    /// `req:` — a capability request: its identity (name) and call-site position.
    Req { capability: String, pos: Pos },
    /// `res:` — the host's scripted resolution of the pending capability.
    Res(ScriptResponse),
    /// `outcome:` — a `run` fixture's terminal outcome.
    Outcome(Terminal),
    /// `step:` — a `drive` fixture's driving action.
    Step(String),
    /// `stop:` — the stop that action produced.
    Stop(Stop),
    /// `stack:` — the live stack at a stop (innermost first), when non-empty.
    Stack(Vec<StackElem>),
}

/// A `run` fixture's terminal outcome (`outcome:`).
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum Terminal {
    Completed,
    Raised { kind: String, pos: Pos },
    Faulted { kind: String },
}

/// A `drive` step's stop (`stop:`) — the drive-script stop vocabulary, positions module-qualified,
/// raises by **kind** not message.
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum Stop {
    Completed,
    Paused { reason: String, pos: Pos },
    Raised { kind: String, pos: Pos },
    Suspended { capability: String, pos: Pos },
    Import { path: String, pos: Pos },
    Faulted { kind: String },
}

/// One stack element (`stack:`), reusing the drive-script `name@line×tail` encoding.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct StackElem {
    pub(crate) name: Option<String>,
    pub(crate) line: u32,
    pub(crate) tail: u64,
}

impl Transcript {
    /// Serializes to the `transcript v1` line format: LF-terminated lines, a trailing newline,
    /// byte-exact.
    pub(crate) fn serialize(&self) -> String {
        let mut out = String::new();
        out.push_str(VERSION_LINE);
        out.push('\n');
        out.push_str(match self.mode {
            Mode::Drive => "mode: drive\n",
            _ => "mode: run\n",
        });
        for event in &self.events {
            out.push_str(&event.render());
            out.push('\n');
        }
        out
    }
}

impl Event {
    /// The event's single serialized line (without the trailing newline).
    fn render(&self) -> String {
        match self {
            Event::Out(bytes) => format!("out: {}", escape_bytes(bytes)),
            Event::Req { capability, pos } => format!("req: {capability} @ {}", pos.render()),
            Event::Res(response) => format!("res: {}", render_response(response)),
            Event::Outcome(terminal) => format!("outcome: {}", terminal.render()),
            Event::Step(action) => format!("step: {action}"),
            Event::Stop(stop) => format!("stop: {}", stop.render()),
            Event::Stack(elems) => {
                let rendered: Vec<String> = elems.iter().map(StackElem::render).collect();
                format!("stack: {}", rendered.join(", "))
            }
        }
    }
}

impl Pos {
    fn render(&self) -> String {
        format!("{}:{}:{}", self.module, self.line, self.column)
    }
}

impl Terminal {
    fn render(&self) -> String {
        match self {
            Terminal::Completed => "completed".to_string(),
            Terminal::Raised { kind, pos } => format!("raised {kind} @ {}", pos.render()),
            Terminal::Faulted { kind } => format!("faulted {kind}"),
        }
    }
}

impl Stop {
    fn render(&self) -> String {
        match self {
            Stop::Completed => "completed".to_string(),
            Stop::Paused { reason, pos } => format!("paused {reason} @ {}", pos.render()),
            Stop::Raised { kind, pos } => format!("raised {kind} @ {}", pos.render()),
            Stop::Suspended { capability, pos } => {
                format!("suspended {capability} @ {}", pos.render())
            }
            Stop::Import { path, pos } => format!("import {path} @ {}", pos.render()),
            Stop::Faulted { kind } => format!("faulted {kind}"),
        }
    }
}

impl StackElem {
    fn render(&self) -> String {
        let mut s = match &self.name {
            Some(name) => format!("{name}@{}", self.line),
            None => format!("{}", self.line),
        };
        if self.tail > 0 {
            s.push_str(&format!("×{}", self.tail));
        }
        s
    }
}

/// Renders a scripted resolution (`res:`): a value literal, or `raise "<msg>"`.
fn render_response(response: &ScriptResponse) -> String {
    match response {
        ScriptResponse::Value(value) => render_value(value),
        ScriptResponse::Raise(message) => format!("raise {}", quote_string(message)),
    }
}

/// Renders a [`ScriptValue`] in the drive-script literal syntax.
fn render_value(value: &ScriptValue) -> String {
    match value {
        ScriptValue::Str(s) => quote_string(s),
        ScriptValue::Int(n) => n.to_string(),
        // A float always carries a `.` so it round-trips as a float literal (S-56 finite floats;
        // no NaN/∞ crosses a scripted resolution). No float `res:` occurs in the corpus today.
        ScriptValue::Float(f) => {
            let s = f.to_string();
            if s.contains('.') || s.contains('e') {
                s
            } else {
                format!("{s}.0")
            }
        }
        ScriptValue::Bool(b) => b.to_string(),
        ScriptValue::Nil => "nil".to_string(),
    }
}

/// A `"…"` string literal with the transcript escaping (control bytes / `\` / `"` / DEL as `\xNN`).
fn quote_string(s: &str) -> String {
    let mut out: Vec<u8> = vec![b'"'];
    for &byte in s.as_bytes() {
        push_escaped(&mut out, byte, true);
    }
    out.push(b'"');
    finish_utf8(out)
}

/// Escapes output bytes for an `out:` payload: control bytes (`< 0x20`), `\`, and DEL as `\xNN`;
/// every other byte verbatim (so valid UTF-8 stays readable and the file stays UTF-8).
fn escape_bytes(bytes: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    for &byte in bytes {
        push_escaped(&mut out, byte, false);
    }
    finish_utf8(out)
}

/// Appends one byte to `out`, `\xNN`-escaped if it is a control byte, `\`, DEL, or (when
/// `escape_quote`) `"`; else verbatim. Building a byte vector — not a `String` — is what keeps a
/// multibyte UTF-8 sequence intact (`byte as char` would remap `0x80..=0xFF` to a Latin-1 code point).
fn push_escaped(out: &mut Vec<u8>, byte: u8, escape_quote: bool) {
    if byte < 0x20 || byte == b'\\' || byte == 0x7f || (escape_quote && byte == b'"') {
        out.extend_from_slice(format!("\\x{byte:02x}").as_bytes());
    } else {
        out.push(byte);
    }
}

/// Finishes an escaped byte buffer into a `String`: the escapes are ASCII and every verbatim byte
/// preserved the input's valid UTF-8, so the buffer is valid UTF-8. Output that is not valid UTF-8
/// is a fixture/capability bug (Doodle strings and `print`/the conformance capabilities are all
/// UTF-8) — surface it loudly rather than silently lose bytes.
fn finish_utf8(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).expect("escaped transcript payload is valid UTF-8")
}

/// The emitters that run a fixture and record its [`Transcript`].
mod emit;
pub(crate) use emit::{record_drive, record_run};

#[cfg(test)]
mod tests;
