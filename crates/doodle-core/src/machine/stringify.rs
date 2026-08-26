//! The value → `String` seam (L§15 hook 1): the `Stringable` `to_string` dispatcher
//! that string interpolation (L§6.7) and the demo `print` both render through.
//!
//! **Native placeholder (M4.9).** The language *guarantees the call* — interpolation
//! invokes this dispatcher **directly**, a hidden binding rather than the lexical name
//! `to_string`, so user shadowing of `to_string` cannot change interpolation semantics
//! (L§15 hook 1; S-37). The standard library *supplies the behavior*: real per-type
//! `Stringable` implementations arrive with protocol dispatch (M5) and stdlib defaults
//! (M9a), replacing this function with no change to the interpolation/`print` call
//! sites. Until then this renders the built-in types natively.
//!
//! Scalars (`Int`/`BigInt`/`Float`/`Bool`/`Nil`/`String`) render to their final forms
//! and are covered by M4 acceptance. Compound values (list/dict/record/bytes/callable/
//! …) get a provisional angle-bracket **placeholder** whose exact text is *not* pinned
//! at M4 — the stdlib fixes it at M9a. Rendering is **deterministic** (E§11): integers
//! and bignums render exactly, floats use a fixed shortest-round-trip format, and no
//! address or iteration order leaks in.

use crate::heap::Heap;
use crate::machine::Value;

/// Renders `value` to its `String` form via the placeholder `Stringable` dispatcher
/// (see the module note). Crate-visible so interpolation (`eval`), `print`, and the
/// host-raised-value rendering can share the one seam.
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

/// Deterministic float rendering (E§11 fixed float formatting). Every machine-produced
/// float is finite (S-56); an integer-valued float still shows a `.0` so it is not
/// mistaken for an integer.
fn render_float(x: f64) -> String {
    if x == x.trunc() && x.is_finite() {
        format!("{x:.1}")
    } else {
        // Rust's `{}` for f64 is the shortest round-tripping decimal — deterministic.
        format!("{x}")
    }
}
