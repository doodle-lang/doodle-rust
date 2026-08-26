//! The provisional demo intrinsics (`print`, `each`, `read_line`) and the provisional
//! value renderer, split from `mod.rs` so the reentrant-drive *mechanism* and the demo
//! *callables* built on it live apart (and to keep each file within the hygiene length
//! limit). All are retired or superseded as the module system, prelude, and Stringable
//! protocol land (S-43, M4/M5/M9a).

use super::{BlockResult, ForeignBody, ForeignParam, Intrinsic};
use crate::heap::Heap;
use crate::machine::Value;
use crate::machine::error::{ExceptionKind, Raise};
use crate::resolve::BodyKind;
use crate::span::Span;
use num_traits::ToPrimitive;

/// The demo intrinsic `print` (E§5.2, S-43): a `to` taking one value, rendering it
/// (the provisional [`render`] stand-in for L§15 Stringable, superseded at M4/M9a),
/// and appending it plus a newline to the instance's output sink.
pub fn print() -> Intrinsic {
    Intrinsic {
        name: "print".into(),
        kind: BodyKind::Proc,
        params: vec![ForeignParam {
            name: "value".into(),
            default: None,
            is_block: false,
        }],
        body: ForeignBody::Sync(|ctx| {
            let text = render(ctx.heap(), ctx.args()[0]);
            ctx.emit(text.as_bytes());
            ctx.emit(b"\n");
            Ok(None)
        }),
    }
}

/// The demo native block-consuming intrinsic `each` (E§5.4/§7.6, MD §14): a `to` taking
/// a `List` and a trailing block, invoking the block **reentrantly** once per element.
/// A raise inside the block propagates; a `break`/`return` out of it crosses the native
/// boundary as an S-46 `NonLocalExit` (a `break` ends the `each`, a `return` unwinds
/// past it to the enclosing function). The first native higher-order primitive — the
/// shape `repeat`/`map` take, and proof a native consumer is program-observably
/// identical to a Doodle-written block-consumer for every exit kind (S-46 parity).
pub fn each() -> Intrinsic {
    Intrinsic {
        name: "each".into(),
        kind: BodyKind::Proc,
        params: vec![
            ForeignParam {
                name: "list".into(),
                default: None,
                is_block: false,
            },
            ForeignParam {
                name: "body".into(),
                default: None,
                is_block: true,
            },
        ],
        body: ForeignBody::Sync(|ctx| {
            match ctx.args()[0] {
                // Iterate a **fixed count** (the length at entry) over the live heap list —
                // which stays rooted through `each`'s `foreign_roots` entry (MD §15), so the
                // block's reentrant drive may collect. The fixed count bounds a block that
                // appends; `.get` guards a block that shrinks the list.
                Value::List(idx) => {
                    let count = ctx.heap().list(idx).items.len();
                    for i in 0..count {
                        let Some(&element) = ctx.heap().list(idx).items.get(i) else {
                            break; // the block shrank the list past here
                        };
                        if !each_continues(ctx.invoke_block(vec![element])?) {
                            break;
                        }
                    }
                }
                // Iterate a string one **extended grapheme cluster** at a time (L§4.4), each
                // a length-one string. The source string stays rooted (`foreign_roots`), so
                // its bytes and grapheme memo survive each block's reentrant drive.
                Value::Str(idx) => {
                    let count = ctx.heap().grapheme_offsets(idx).len();
                    for i in 0..count {
                        let grapheme = extract_grapheme(ctx.heap(), idx, i);
                        let value = ctx.alloc_string(grapheme);
                        if !each_continues(ctx.invoke_block(vec![value])?) {
                            break;
                        }
                    }
                }
                _ => {
                    return Err(Raise::new(
                        ExceptionKind::TypeMismatch,
                        "`each` needs a list or a string to iterate",
                        ctx.span(),
                    ));
                }
            }
            Ok(None)
        }),
    }
}

/// Whether `each` continues to the next element after a block invocation. A normal
/// completion (fall-off or `continue`) continues; a `break`/`return` across the native
/// boundary (an S-46 `NonLocalExit`, resumed at the apply site) or a parked fault
/// (`Halted`, surfaced by `step`) stops the iteration — the callback must not drive on.
fn each_continues(result: BlockResult) -> bool {
    matches!(result, BlockResult::Completed)
}

/// The `i`-th extended grapheme cluster of the string at `idx`, as an owned NFC string (a
/// slice at cluster boundaries of an NFC string is itself NFC). `i` must be in range.
fn extract_grapheme(heap: &Heap, idx: crate::machine::StrIdx, i: usize) -> Box<str> {
    let offsets = heap.grapheme_offsets(idx);
    let start = offsets[i] as usize;
    let end = offsets
        .get(i + 1)
        .map_or(heap.string(idx).utf8.len(), |&e| e as usize);
    heap.string(idx).utf8[start..end].into()
}

/// The provisional demo intrinsic `length` (L§4.4/§4.6/§4.7/§4.5, §15): a `fn` taking one
/// container and yielding its length as an `Int` — a `String` counts **extended grapheme
/// clusters** (L§4.4, O(n)), a `List`/`Dict` its elements, `Bytes` its bytes. Superseded by
/// the standard library's `length` (M9a); a non-container raises `TypeMismatch`.
pub fn length() -> Intrinsic {
    Intrinsic {
        name: "length".into(),
        kind: BodyKind::Func,
        params: vec![ForeignParam {
            name: "value".into(),
            default: None,
            is_block: false,
        }],
        body: ForeignBody::Sync(|ctx| {
            let n = match ctx.args()[0] {
                Value::Str(s) => ctx.heap().grapheme_offsets(s).len(),
                Value::List(l) => ctx.heap().list(l).items.len(),
                Value::Dict(d) => ctx.heap().dict(d).entries.len(),
                Value::Bytes(b) => ctx.heap().byte_string(b).bytes.len(),
                _ => {
                    return Err(Raise::new(
                        ExceptionKind::TypeMismatch,
                        "`length` needs a string, list, dict, or bytes",
                        ctx.span(),
                    ));
                }
            };
            Ok(Some(Value::Int(n as i64)))
        }),
    }
}

/// The provisional demo intrinsic `encode` (L§4.4/§15 "the byte view"): a `fn` mapping a
/// `String` to its NFC UTF-8 `Bytes`. **Cannot fail** — a `String` is always valid NFC
/// UTF-8 by construction (§4.4). Superseded by the standard library (M9a); a non-string
/// raises `TypeMismatch`.
pub fn encode() -> Intrinsic {
    Intrinsic {
        name: "encode".into(),
        kind: BodyKind::Func,
        params: vec![value_param()],
        body: ForeignBody::Sync(|ctx| {
            let Value::Str(s) = ctx.args()[0] else {
                return Err(Raise::new(
                    ExceptionKind::TypeMismatch,
                    "`encode` needs a string",
                    ctx.span(),
                ));
            };
            let bytes: Box<[u8]> = ctx.heap().string(s).utf8.as_bytes().into();
            Ok(Some(ctx.alloc_bytes(bytes)))
        }),
    }
}

/// The provisional demo intrinsic `decode` (L§4.4/§4.5/§15 "the byte view"): a `fn`
/// mapping `Bytes` to a `String`, **validating UTF-8** (raising `invalid-utf8` on
/// malformed input, naming the byte offset of the first bad sequence — S-58) and
/// **normalizing to NFC**. The round-trip law holds: `decode(encode(s)) == s`. A lossy
/// (replacement-character) decode is a separate later stdlib function, never this one.
pub fn decode() -> Intrinsic {
    Intrinsic {
        name: "decode".into(),
        kind: BodyKind::Func,
        params: vec![value_param()],
        body: ForeignBody::Sync(|ctx| {
            let Value::Bytes(b) = ctx.args()[0] else {
                return Err(Raise::new(
                    ExceptionKind::TypeMismatch,
                    "`decode` needs bytes",
                    ctx.span(),
                ));
            };
            let text = match std::str::from_utf8(&ctx.heap().byte_string(b).bytes) {
                Ok(text) => text,
                Err(e) => {
                    return Err(Raise::new(
                        ExceptionKind::InvalidUtf8,
                        format!(
                            "these bytes aren't valid UTF-8 text (problem at byte {})",
                            e.valid_up_to()
                        ),
                        ctx.span(),
                    ));
                }
            };
            let nfc = crate::unicode::nfc(text).into_owned().into_boxed_str();
            Ok(Some(ctx.alloc_string(nfc)))
        }),
    }
}

/// The single required `value` parameter shared by the container/conversion intrinsics.
fn value_param() -> ForeignParam {
    ForeignParam {
        name: "value".into(),
        default: None,
        is_block: false,
    }
}

/// The demo suspending capability `read_line` (E§5.3, §7.5): a `fn` taking no arguments
/// that **suspends** — the host supplies the line via `resolve(Value)` (or fails it via
/// `resolve(Raise)`). The canonical scripted capability for the M2b drive tests.
pub fn read_line() -> Intrinsic {
    Intrinsic {
        name: "read_line".into(),
        kind: BodyKind::Func,
        params: Vec::new(),
        body: ForeignBody::Capability,
    }
}

/// The **provisional** trig native `sin` (E§5.2): a `fn` taking one number (an angle in
/// radians) and yielding its sine as a `Float`. The turtle library needs trig for
/// `forward`, and the standard library that will own `sin`/`cos` is M9a — so these are
/// registered like `print` until then (superseded with no program-observable change).
/// Computed with the deterministic pure-Rust `libm` (see `Cargo.toml`): the platform
/// `f64::sin` is not bit-identical across targets, which would break replay and the
/// cross-surface conformance gate (E§11).
pub fn sin() -> Intrinsic {
    Intrinsic {
        name: "sin".into(),
        kind: BodyKind::Func,
        params: vec![angle_param()],
        body: ForeignBody::Sync(|ctx| {
            let angle = as_angle(ctx.heap(), ctx.args()[0], ctx.span())?;
            finite_float(libm::sin(angle), ctx.span())
        }),
    }
}

/// The **provisional** trig native `cos` (E§5.2): the cosine companion to [`sin`], same
/// contract and deterministic `libm` backing.
pub fn cos() -> Intrinsic {
    Intrinsic {
        name: "cos".into(),
        kind: BodyKind::Func,
        params: vec![angle_param()],
        body: ForeignBody::Sync(|ctx| {
            let angle = as_angle(ctx.heap(), ctx.args()[0], ctx.span())?;
            finite_float(libm::cos(angle), ctx.span())
        }),
    }
}

/// The single required `angle` parameter shared by [`sin`] and [`cos`].
fn angle_param() -> ForeignParam {
    ForeignParam {
        name: "angle".into(),
        default: None,
        is_block: false,
    }
}

/// Coerces a trig argument (Int, Float, or BigInt) to `f64`, mirroring arithmetic
/// widening (`arith.rs`): an integer whose magnitude rounds to ±∞ raises `NonFiniteFloat`
/// (the widening is itself nonfinite, L§4.2), a `Float` passes through (the result check
/// catches a nonfinite one), and a non-number raises `TypeMismatch`.
fn as_angle(heap: &Heap, value: Value, span: Span) -> Result<f64, Raise> {
    let widened = match value {
        Value::Int(n) => n.to_f64(),
        Value::Float(x) => return Ok(x),
        Value::BigInt(idx) => heap.bigint(idx).value.to_f64(),
        _ => {
            return Err(Raise::new(
                ExceptionKind::TypeMismatch,
                "this needs a number (an angle in radians)",
                span,
            ));
        }
    };
    match widened {
        Some(x) if x.is_finite() => Ok(x),
        _ => Err(nonfinite(span)),
    }
}

/// Wraps a trig result in the finite-float invariant (S-56): `sin`/`cos` of a finite
/// angle are always finite, but a nonfinite (host-injected) `Float` angle yields `NaN`.
fn finite_float(x: f64, span: Span) -> Result<Option<Value>, Raise> {
    if x.is_finite() {
        Ok(Some(Value::Float(x)))
    } else {
        Err(nonfinite(span))
    }
}

/// The shared `NonFiniteFloat` raise for the trig natives (message parallels `arith.rs`).
fn nonfinite(span: Span) -> Raise {
    Raise::new(
        ExceptionKind::NonFiniteFloat,
        "that number got too big to be a real number",
        span,
    )
}

/// A **provisional** value renderer for `print` over the demo subset — a stand-in for
/// the L§15 Stringable dispatcher (real `to_string` protocol dispatch is M4/M9a). It
/// must be **deterministic** (E§11): integers/bignums render exactly, floats use a
/// fixed shortest-round-trip format, and no address/ordering leaks in. Compound
/// values (list/bytes/records/…) get a provisional angle-bracket tag until the real
/// dispatcher lands. Crate-visible so `resolve(Raise)` can render a host-raised value
/// into its message (`machine.rs`).
pub(crate) fn render(heap: &Heap, value: Value) -> String {
    match value {
        Value::Nil => "nil".to_string(),
        Value::Bool(true) => "true".to_string(),
        Value::Bool(false) => "false".to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(idx) => heap.bigint(idx).value.to_string(),
        Value::Float(x) => render_float(x),
        Value::Str(idx) => heap.string(idx).utf8.to_string(),
        Value::Bytes(_) => "<bytes>".to_string(),
        Value::List(_) => "<list>".to_string(),
        Value::Dict(_) => "<dict>".to_string(),
        Value::Record(_) => "<record>".to_string(),
        Value::Callable(_) => "<callable>".to_string(),
        Value::Module(_) => "<module>".to_string(),
        Value::Type(_) => "<type>".to_string(),
        Value::Foreign(_) => "<foreign>".to_string(),
    }
}

/// Deterministic float rendering for the provisional `print` (E§11 fixed float
/// formatting). Every machine-produced float is finite (S-56); an integer-valued
/// float still shows a `.0` so it is not mistaken for an integer.
fn render_float(x: f64) -> String {
    if x == x.trunc() && x.is_finite() {
        format!("{x:.1}")
    } else {
        // Rust's `{}` for f64 is the shortest round-tripping decimal — deterministic.
        format!("{x}")
    }
}
