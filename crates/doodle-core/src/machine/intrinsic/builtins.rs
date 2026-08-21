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
            let Value::List(idx) = ctx.args()[0] else {
                return Err(Raise::new(
                    ExceptionKind::TypeMismatch,
                    "`each` needs a list to iterate",
                    ctx.span(),
                ));
            };
            // Iterate a **fixed count** (the length at entry) over the live heap list —
            // which stays rooted through `each`'s `foreign_roots` entry (MD §15), so the
            // block's reentrant drive may collect. The fixed count bounds a block that
            // appends; `.get` guards a block that shrinks the list.
            let count = ctx.heap().list(idx).items.len();
            for i in 0..count {
                let Some(&element) = ctx.heap().list(idx).items.get(i) else {
                    break; // the block shrank the list past here
                };
                match ctx.invoke_block(vec![element])? {
                    // The block completed (or `continue`d): go to the next element.
                    BlockResult::Completed => {}
                    // A `break`/`return` crossed the native boundary (S-46): stop
                    // iterating and return promptly; the parked exit is resumed at the
                    // apply site (a `break` completes this `each`, a `return`/outer break
                    // unwinds past it). The callback must not drive further here.
                    BlockResult::NonLocalExit => break,
                    // A nested fault was parked (a limit inside the block, or S-15): stop;
                    // `step` surfaces it after this call returns.
                    BlockResult::Halted => break,
                }
            }
            Ok(None)
        }),
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
