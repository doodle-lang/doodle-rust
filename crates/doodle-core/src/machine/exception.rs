//! The built-in `Error` exception value (L§12.1, E§9; S-58). An engine raise carries a
//! kind + message; when it enters the unwind channel it **materializes** the value
//! record `Error(kind, message, details)` a `rescue` binds. A program can construct the
//! very same value, so who-raised lives in the trace, not the value.

use crate::diag::Diagnostic;
use crate::heap::Heap;
use crate::machine::{RecIdx, StrIdx, TypeIdx, TypeKind, Value};
use crate::span::{ModuleId, Span};

/// A structured detail for an `Error`'s `details` dict (E§9, S-58). At materialization
/// (`make_error`) each becomes an ordinary Doodle value, so one `details` record is
/// inspectable from Doodle code **and** from the host without a second schema: `Str`→a
/// string, `Int`→an integer, `Bool`→a boolean, `Value`→that value verbatim (e.g. a
/// `key-not-found` key), `List`→a list, `Dict`→a small nested dict (e.g. one diagnostic
/// entry in a `module-load-error`). Type names in details are **display strings** spelled
/// as the type values (Int/String/List/a record's name/Callable, S-37) — hosts render and
/// localize *by name*, never by parsing the message (rubric pin (a)).
#[derive(Clone, Debug)]
pub(crate) enum DetailVal {
    /// A string (a name, a slug, a type name, a path segment).
    Str(Box<str>),
    /// A small integer (an index, a length, a count).
    Int(i64),
    /// A boolean flag (e.g. `undefined-ordering`'s `nan`).
    Bool(bool),
    /// A Doodle value verbatim (e.g. a `key-not-found` key, which may be any value).
    Value(Value),
    /// An ordered list of details (e.g. `type-mismatch`'s accepted `expected` types, a
    /// wildcard ambiguity's two `modules`, a `circular-import` `cycle`).
    List(Vec<DetailVal>),
    /// A small nested dict (e.g. one diagnostic in `module-load-error`'s `diagnostics`).
    Dict(Vec<(&'static str, DetailVal)>),
}

impl DetailVal {
    /// A string detail from anything string-like.
    pub(crate) fn str(s: impl Into<Box<str>>) -> Self {
        DetailVal::Str(s.into())
    }

    /// A list-of-strings detail (e.g. `type-mismatch`'s `expected`, an ambiguity's modules).
    pub(crate) fn strs(items: impl IntoIterator<Item = String>) -> Self {
        DetailVal::List(
            items
                .into_iter()
                .map(|s| DetailVal::Str(s.into_boxed_str()))
                .collect(),
        )
    }
}

/// Builds an `Error(kind, message, details)` record value (L§12.1, E§9). `details` is the
/// kind's structured data (S-58, rubric App A) — a Doodle dict a host renders/localizes
/// from without ever parsing the message. Strings are stored NFC (L§4.4); engine slugs and
/// messages are already normalized, so this is a no-op normalization that upholds the
/// heap-string invariant.
pub(crate) fn make_error(
    heap: &mut Heap,
    error_type: TypeIdx,
    kind_slug: &str,
    message: &str,
    details: &[(&'static str, DetailVal)],
) -> Value {
    let kind = Value::Str(alloc_str(heap, kind_slug));
    let message = Value::Str(alloc_str(heap, message));
    let details = build_details(heap, details);
    Value::Record(heap.alloc_record(error_type, Box::new([kind, message, details])))
}

/// Builds the `details` dict (L§4.7 insertion-ordered) from the raise's structured entries.
/// Keys are the schema's fixed strings; values are ordinary Doodle values. String keys are
/// always hashable, so the native insert never raises here (the span is unused).
fn build_details(heap: &mut Heap, entries: &[(&'static str, DetailVal)]) -> Value {
    let idx = heap.alloc_dict();
    for (key, val) in entries {
        let k = Value::Str(alloc_str(heap, key));
        let v = detail_to_value(heap, val);
        let _ = super::dict::insert(heap, idx, k, v, Span::DUMMY);
    }
    Value::Dict(idx)
}

/// Materializes one [`DetailVal`] into a Doodle value.
fn detail_to_value(heap: &mut Heap, val: &DetailVal) -> Value {
    match val {
        DetailVal::Str(s) => Value::Str(alloc_str(heap, s)),
        DetailVal::Int(i) => Value::Int(*i),
        DetailVal::Bool(b) => Value::Bool(*b),
        DetailVal::Value(v) => *v,
        DetailVal::List(items) => {
            let vals: Vec<Value> = items.iter().map(|d| detail_to_value(heap, d)).collect();
            Value::List(heap.alloc_list(vals))
        }
        DetailVal::Dict(entries) => build_details(heap, entries),
    }
}

/// A value's **type name** as a display string, spelled as the type values are
/// (`Int`/`String`/`List`/a record's own name/`Callable`/…, S-37) — the form `details`
/// carries for a type. A callable is named by its umbrella `Callable`: the
/// `Procedure`/`Function` split (S-37) needs the resolver/registry the raise helpers do
/// not carry, and a callable is essentially never the offending operand of a type error.
pub(crate) fn value_type_name(value: Value, heap: &Heap) -> String {
    match value {
        Value::Nil => "Nil".to_string(),
        Value::Bool(_) => "Bool".to_string(),
        Value::Int(_) | Value::BigInt(_) => "Int".to_string(),
        Value::Float(_) => "Float".to_string(),
        Value::Str(_) => "String".to_string(),
        Value::Bytes(_) => "Bytes".to_string(),
        Value::List(_) => "List".to_string(),
        Value::Dict(_) => "Dict".to_string(),
        Value::Record(r) => match &heap.type_value(heap.record(r).type_idx).kind {
            TypeKind::Record(rt) => rt.name.to_string(),
            _ => "Record".to_string(),
        },
        Value::Callable(_) => "Callable".to_string(),
        Value::Module(_) => "Module".to_string(),
        Value::Type(_) => "Type".to_string(),
        Value::Foreign(_) => "Foreign".to_string(),
    }
}

/// The leading `callee` detail shared by the four argument-binding kinds (S-58): the
/// callable's name when known (a record type, a protocol member), omitted for a plain
/// `fn` or block call whose name isn't available at the binding site.
fn callee_detail(callee: Option<&str>) -> Vec<(&'static str, DetailVal)> {
    callee.map_or_else(Vec::new, |c| vec![("callee", DetailVal::str(c))])
}

/// The `missing-argument` / `duplicate-argument` `details` (S-58 `{callee, parameter}`):
/// the callable (when known) and the unbound-or-repeated parameter.
pub(crate) fn parameter_details(
    callee: Option<&str>,
    parameter: &str,
) -> Vec<(&'static str, DetailVal)> {
    let mut details = callee_detail(callee);
    details.push(("parameter", DetailVal::str(parameter)));
    details
}

/// The `unknown-keyword` `details` (S-58 `{callee, keyword, parameters}`): the bad keyword
/// and the callee's valid parameter names (for a host's "did you mean?").
pub(crate) fn unknown_keyword_details(
    callee: Option<&str>,
    keyword: &str,
    parameters: &[Box<str>],
) -> Vec<(&'static str, DetailVal)> {
    let mut details = callee_detail(callee);
    details.push(("keyword", DetailVal::str(keyword)));
    details.push((
        "parameters",
        DetailVal::strs(parameters.iter().map(|p| p.to_string())),
    ));
    details
}

/// The `too-many-arguments` `details` (S-58 `{callee, expected, got}`): the callee's
/// parameter count and how many positionals it was given.
pub(crate) fn too_many_arguments_details(
    callee: Option<&str>,
    expected: usize,
    got: usize,
) -> Vec<(&'static str, DetailVal)> {
    let mut details = callee_detail(callee);
    details.push(("expected", DetailVal::Int(expected as i64)));
    details.push(("got", DetailVal::Int(got as i64)));
    details
}

/// The `type-mismatch` `details` (S-58 `{operator, expected, got}`): the operation, the
/// accepted type(s) (a list of display type names — an operator often accepts several), and
/// the offending value's runtime type name (a display string, S-37). One shape for every
/// `type-mismatch` raise so a host renders "expected a Number, got a String" without parsing.
pub(crate) fn type_mismatch_details(
    operator: &str,
    expected: &[&str],
    got: Value,
    heap: &Heap,
) -> Vec<(&'static str, DetailVal)> {
    vec![
        ("operator", DetailVal::str(operator)),
        (
            "expected",
            DetailVal::strs(expected.iter().map(|s| (*s).to_string())),
        ),
        ("got", DetailVal::str(value_type_name(got, heap))),
    ]
}

/// The `undefined-ordering` `details` (S-58 `{operator, left, right, nan?}`): the comparison
/// operator, the two operands' runtime type names, and — only in the NaN case (a `Float` vs
/// `Float` where one is NaN, S-28) — a `nan: true` flag, so a host branches the "not a real
/// number" explanation from the "these two kinds don't order" one without parsing the message.
pub(crate) fn ordering_details(
    operator: &str,
    left: Value,
    right: Value,
    nan: bool,
    heap: &Heap,
) -> Vec<(&'static str, DetailVal)> {
    let mut details = vec![
        ("operator", DetailVal::str(operator)),
        ("left", DetailVal::str(value_type_name(left, heap))),
        ("right", DetailVal::str(value_type_name(right, heap))),
    ];
    if nan {
        details.push(("nan", DetailVal::Bool(true)));
    }
    details
}

/// The `details` for a module-reference miss (`module-not-found` `{path, importer}`, S-58):
/// the requested dotted path (as segments) and the importing module's id.
pub(crate) fn module_ref_details(
    path: &[Box<str>],
    importer: ModuleId,
) -> Vec<(&'static str, DetailVal)> {
    vec![
        ("path", DetailVal::strs(path.iter().map(|s| s.to_string()))),
        ("importer", DetailVal::Int(i64::from(importer.0))),
    ]
}

/// The `details` for a `module-load-error` (`{path, canonical_id, diagnostics}`, S-58): the
/// requested path, the host's canonical id, and the imported module's front-end diagnostics
/// (the same list the load-diagnostics record holds, S-63) as structured entries — so an IDE
/// renders an imported module's errors exactly as it renders the main program's.
pub(crate) fn module_load_details(
    path: &[Box<str>],
    canonical_id: &str,
    diags: &[Diagnostic],
) -> Vec<(&'static str, DetailVal)> {
    vec![
        ("path", DetailVal::strs(path.iter().map(|s| s.to_string()))),
        ("canonical_id", DetailVal::str(canonical_id)),
        (
            "diagnostics",
            DetailVal::List(diags.iter().map(diagnostic_detail).collect()),
        ),
    ]
}

/// One front-end diagnostic as a structured detail (the S-63 schema, core subset):
/// `severity` (`"error"`/`"warning"`), `code` (the stable slug), `message`, and `span`
/// (`{start, end}` byte offsets) when present. Secondary structure (notes, the suggestion)
/// is deferred — the IDE gets the primary diagnostic and can locate + render it.
fn diagnostic_detail(d: &Diagnostic) -> DetailVal {
    use crate::diag::Severity;
    let mut entry: Vec<(&'static str, DetailVal)> = vec![
        (
            "severity",
            DetailVal::str(match d.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
            }),
        ),
        ("code", DetailVal::str(d.code.slug())),
        ("message", DetailVal::str(d.message.as_str())),
    ];
    if let Some(span) = d.span {
        entry.push((
            "span",
            DetailVal::Dict(vec![
                ("start", DetailVal::Int(i64::from(span.start))),
                ("end", DetailVal::Int(i64::from(span.end))),
            ]),
        ));
    }
    DetailVal::Dict(entry)
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
