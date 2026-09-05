//! Golden-string tests pinning the `transcript v1` serialization byte-for-byte (M7.5d) — the format
//! every surface (native / wasm / C) must emit identically for the drift check to be meaningful.

use super::*;
use crate::model::{Mode, ScriptResponse, ScriptValue};

fn pos(line: u32, column: u32) -> Pos {
    Pos {
        module: "main".to_string(),
        line,
        column,
    }
}

#[test]
fn serializes_a_run_transcript_with_capability_io() {
    let t = Transcript {
        mode: Mode::Run,
        events: vec![
            Event::Req {
                capability: "read_line".to_string(),
                pos: pos(9, 1),
            },
            Event::Res(ScriptResponse::Value(ScriptValue::Str("hi".to_string()))),
            Event::Out(b"hi\n".to_vec()),
            Event::Outcome(Terminal::Completed),
        ],
    };
    assert_eq!(
        t.serialize(),
        "transcript v1\nmode: run\nreq: read_line @ main:9:1\nres: \"hi\"\nout: hi\\x0a\noutcome: completed\n"
    );
}

#[test]
fn a_raise_records_the_kind_not_the_message() {
    let t = Transcript {
        mode: Mode::Run,
        events: vec![Event::Outcome(Terminal::Raised {
            kind: "type-mismatch".to_string(),
            pos: pos(6, 1),
        })],
    };
    assert_eq!(
        t.serialize(),
        "transcript v1\nmode: run\noutcome: raised type-mismatch @ main:6:1\n"
    );
}

#[test]
fn a_fault_records_only_the_kind() {
    let t = Transcript {
        mode: Mode::Run,
        events: vec![Event::Outcome(Terminal::Faulted {
            kind: "nested-suspend".to_string(),
        })],
    };
    assert_eq!(
        t.serialize(),
        "transcript v1\nmode: run\noutcome: faulted nested-suspend\n"
    );
}

#[test]
fn a_scripted_raise_resolution_echoes_the_message() {
    // `res: raise "…"` echoes the fixture's scripted input, not an engine message (the "never
    // message text" rule is about *asserted* error text).
    let t = Transcript {
        mode: Mode::Run,
        events: vec![Event::Res(ScriptResponse::Raise("eof".to_string()))],
    };
    assert_eq!(
        t.serialize(),
        "transcript v1\nmode: run\nres: raise \"eof\"\n"
    );
}

#[test]
fn serializes_a_drive_transcript_with_a_stack() {
    let t = Transcript {
        mode: Mode::Drive,
        events: vec![
            Event::Step("continue".to_string()),
            Event::Stop(Stop::Paused {
                reason: "breakpoint".to_string(),
                pos: pos(14, 1),
            }),
            Event::Stack(vec![
                StackElem {
                    name: Some("f".to_string()),
                    line: 3,
                    tail: 0,
                },
                StackElem {
                    name: None,
                    line: 10,
                    tail: 2,
                },
            ]),
        ],
    };
    assert_eq!(
        t.serialize(),
        "transcript v1\nmode: drive\nstep: continue\nstop: paused breakpoint @ main:14:1\nstack: f@3, 10×2\n"
    );
}

#[test]
fn escapes_control_bytes_and_backslash_keeping_utf8_verbatim() {
    // Tab/backslash/newline escape as \xNN; a multibyte UTF-8 char stays verbatim (file stays UTF-8).
    assert_eq!(escape_bytes("a\tb\\c\né".as_bytes()), "a\\x09b\\x5cc\\x0aé");
}

#[test]
fn quotes_a_string_escaping_the_quote() {
    assert_eq!(quote_string("say \"hi\""), "\"say \\x22hi\\x22\"");
}
