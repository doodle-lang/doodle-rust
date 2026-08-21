//! The **platform primitives** (engine spec E§13) the M3 turtle library draws through:
//! `draw_line`, `set_turtle`, and `clear_canvas`. They are registered like the S-43 provisional
//! natives (via [`Registry::register`](super::Registry::register)), but unlike the demo
//! intrinsics they carry **no engine-side drawing logic** — each is a **suspending
//! capability** ([`ForeignBody::Capability`]) the *host* fulfils by drawing on its
//! surface and then resolving with `nil`. This keeps the engine a turtle-agnostic
//! executor: all turtle geometry, state, and color live in the Doodle turtle library
//! (§Decisions #6/#7), and the host is a "dumb drawing surface" reached only through
//! these three requests.
//!
//! **All three are `to` capabilities** (they yield Void). `draw_line` is the one the
//! host *animates* — it paces the drive over animation frames and resolves on
//! completion (E§5.3, M3.5/M3.6); `set_turtle` and `clear_canvas` are instant, so the
//! host applies them and resolves immediately (no frame wait). Making them capabilities too
//! (rather than a separate sync effect channel) keeps one uniform, ordered request
//! stream and needs no new engine machinery — a `to` capability already resolves to
//! Void (E§7.5). Their animated/instant distinction is entirely the host's concern.
//!
//! Coordinates and the RGBA channels are plain numbers the library computes; the engine
//! neither interprets nor validates them. All are **provisional for M3**: the real
//! native-module/`use` surface that hosts these lands at M5 (E§13).

use super::{ForeignBody, ForeignParam, Intrinsic};
use crate::resolve::BodyKind;

/// Builds a required (no-default, non-block) parameter named `name`.
fn param(name: &str) -> ForeignParam {
    ForeignParam {
        name: name.into(),
        default: None,
        is_block: false,
    }
}

/// A `to` capability with the given name and required parameters (E§5.3/§13): it
/// suspends, and the host resolves it with `nil` (Void) after drawing.
fn drawing_capability(name: &str, params: &[&str]) -> Intrinsic {
    Intrinsic {
        name: name.into(),
        kind: BodyKind::Proc,
        params: params.iter().map(|n| param(n)).collect(),
        body: ForeignBody::Capability,
    }
}

/// The platform primitive `draw_line(x0, y0, x1, y1, r, g, b, a)` (E§13): draw a line
/// from `(x0, y0)` to `(x1, y1)` in color `(r, g, b, a)`. A **suspending capability**
/// the host *animates* (gliding the turtle along the segment) and resolves on
/// completion (E§5.3) — the pen-trail primitive `forward`/`back` draw through. An alpha
/// of `0` is a pen-up glide (the turtle moves but leaves no mark), so `penup` is simply
/// "draw with `a = 0`" rather than a separate host mode.
pub fn draw_line() -> Intrinsic {
    drawing_capability("draw_line", &["x0", "y0", "x1", "y1", "r", "g", "b", "a"])
}

/// The platform primitive `set_turtle(x, y, heading, visible)` (E§13): place the turtle
/// marker at `(x, y)` pointing at `heading` degrees, shown when `visible` is true. An
/// **instant** capability (the host applies it and resolves immediately) that poses the
/// marker without drawing — used by the turns, teleports, and `showturtle`/`hideturtle`.
/// Distinct from `draw_line` so a program can move/point/show the marker independently
/// of leaving a trail.
pub fn set_turtle() -> Intrinsic {
    drawing_capability("set_turtle", &["x", "y", "heading", "visible"])
}

/// The platform primitive `clear_canvas()` (E§13): erase the drawing surface. An
/// **instant** capability (applied and resolved immediately). Named `clear_canvas`, not
/// `clear`, so it does not collide with the turtle library's public `clear` command in
/// the M3 single-module prepend — the command wipes the canvas *and* homes the turtle,
/// calling this primitive. (The real module system namespaces the primitives at M5.)
pub fn clear_canvas() -> Intrinsic {
    drawing_capability("clear_canvas", &[])
}
