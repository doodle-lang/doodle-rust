//! The built-in `Error` exception value (L§12.1, E§9; S-58). An engine raise carries a
//! kind + message; when it enters the unwind channel it **materializes** the value
//! record `Error(kind, message, details)` a `rescue` binds. A program can construct the
//! very same value, so who-raised lives in the trace, not the value.

use crate::heap::Heap;
use crate::machine::{RecIdx, StrIdx, TypeIdx, Value};

/// Builds an `Error(kind, message, details)` record value (L§12.1). `details` starts as
/// an empty dict — the three-field shape is frozen (records have no defaults, L§9), and
/// per-kind structured data is filled in later with the message-rubric work. Strings are
/// stored NFC (L§4.4); engine slugs and messages are already normalized, so this is a
/// no-op normalization that upholds the heap-string invariant.
pub(crate) fn make_error(
    heap: &mut Heap,
    error_type: TypeIdx,
    kind_slug: &str,
    message: &str,
) -> Value {
    let kind = Value::Str(alloc_str(heap, kind_slug));
    let message = Value::Str(alloc_str(heap, message));
    let details = Value::Dict(heap.alloc_dict());
    Value::Record(heap.alloc_record(error_type, Box::new([kind, message, details])))
}

/// Describes a raised value for a drive boundary (E§9): its `(kind_slug, message)`. An
/// `Error` record reports its own `kind`/`message` fields — a program-raised `Error` is
/// indistinguishable from an engine one (L§12.1). Any other raised value reports the
/// generic kind `"raised"` with a best-effort message (a `String` value renders as its
/// contents; richer rendering awaits the `Stringable` dispatcher, M4.9).
pub(crate) fn describe(heap: &Heap, error_type: TypeIdx, value: Value) -> (String, String) {
    if let Value::Record(r) = value
        && heap.record(r).type_idx == error_type
    {
        return (field_str(heap, r, 0), field_str(heap, r, 1));
    }
    let message = match value {
        Value::Str(s) => heap.string(s).utf8.to_string(),
        _ => "(a raised value)".to_string(),
    };
    ("raised".to_string(), message)
}

/// Reads a string field of the `Error` record, or `""` if it is not a string — `Error`'s
/// fields are strings by construction, but the type is forgeable (L§12.1), so a program
/// could build one with a non-string `kind`/`message`; degrade gracefully rather than panic.
fn field_str(heap: &Heap, r: RecIdx, pos: usize) -> String {
    match heap.record(r).fields[pos] {
        Value::Str(s) => heap.string(s).utf8.to_string(),
        _ => String::new(),
    }
}

fn alloc_str(heap: &mut Heap, s: &str) -> StrIdx {
    heap.alloc_string(crate::unicode::nfc(s).into_owned().into_boxed_str())
}
