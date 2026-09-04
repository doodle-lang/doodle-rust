//! The CLI's **drawing text sink** (D-M7-18). The `doodle` CLI is a real drawing host (M3.2): it
//! registers the platform primitives `draw_line`/`set_turtle`/`clear_canvas` (E§13) and, since a
//! terminal cannot show pixels, renders them to **text** — never silently discarding them.
//!
//! - Default: a quiet, honest end-of-run [`summary`](DrawSink::summary) — e.g. `drew 5 line
//!   segments …` — so a kid knows it *did* draw and where to see it properly (the browser).
//! - `--draw-log`: one deterministic line per command as it happens (`line (0,0) -> (100,0)
//!   #000000ff`). The primitives' arguments are plain numbers, so the log is replay-stable (E§11).
//!
//! The sink is turtle-agnostic: the turtle library's `forward`/`right` bottom out in these same
//! primitives, so the log and summary see identical commands whether a program draws through the
//! library or calls the primitives directly.

use doodle_core::machine::{Handle, Instance, Kind};
use std::io::Write;

/// Accumulates the drawing commands a run issues, for the end-of-run summary and (optionally) a
/// per-command log. Owned by the drive loop and fed as each drawing capability resolves.
pub struct DrawSink {
    /// Emit one line per command as it happens (`--draw-log`).
    draw_log: bool,
    /// `draw_line` calls — the trail segments the summary counts.
    segments: u64,
    /// `set_turtle` calls — marker poses that leave no trail.
    poses: u64,
    /// `clear_canvas` calls.
    clears: u64,
    /// The bounding box of every segment endpoint seen so far, for an honest summary.
    bounds: Option<Bounds>,
}

/// The min/max extent of the drawn segment endpoints.
struct Bounds {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl DrawSink {
    /// A fresh sink; `draw_log` enables the per-command log.
    pub fn new(draw_log: bool) -> Self {
        DrawSink {
            draw_log,
            segments: 0,
            poses: 0,
            clears: 0,
            bounds: None,
        }
    }

    /// Records a `draw_line(x0, y0, x1, y1, r, g, b, a)` (E§13): counts the segment, grows the
    /// bounding box, and logs `line (x0,y0) -> (x1,y1) #rrggbbaa` when `--draw-log` is on.
    pub fn draw_line(&mut self, inst: &Instance, args: &[Handle]) {
        let n = |i: usize| args.get(i).map_or(0.0, |&h| read_number(inst, h));
        let (x0, y0, x1, y1) = (n(0), n(1), n(2), n(3));
        self.segments += 1;
        self.grow(x0, y0);
        self.grow(x1, y1);
        if self.draw_log {
            let color = hex_color(n(4), n(5), n(6), n(7));
            emit(&format!(
                "line ({},{}) -> ({},{}) {color}",
                num(x0),
                num(y0),
                num(x1),
                num(y1)
            ));
        }
    }

    /// Records a `set_turtle(x, y, heading, visible)` (E§13): a marker pose (no trail). Logs
    /// `turtle (x,y) heading=H shown|hidden` under `--draw-log`.
    pub fn set_turtle(&mut self, inst: &Instance, args: &[Handle]) {
        self.poses += 1;
        if self.draw_log {
            let n = |i: usize| args.get(i).map_or(0.0, |&h| read_number(inst, h));
            let shown = args.get(3).is_some_and(|&h| read_number(inst, h) != 0.0);
            emit(&format!(
                "turtle ({},{}) heading={} {}",
                num(n(0)),
                num(n(1)),
                num(n(2)),
                if shown { "shown" } else { "hidden" }
            ));
        }
    }

    /// Records a `clear_canvas()` (E§13). Logs `clear` under `--draw-log`.
    pub fn clear_canvas(&mut self) {
        self.clears += 1;
        if self.draw_log {
            emit("clear");
        }
    }

    /// The never-silent end-of-run summary (D-M7-18), or `None` if the run drew nothing. Reports
    /// the segment count and extent, and points to the browser where the picture actually renders.
    pub fn summary(&self) -> Option<String> {
        if self.segments == 0 && self.poses == 0 && self.clears == 0 {
            return None;
        }
        let tail = "; open this program in the browser playground to see the drawing.";
        if self.segments == 0 {
            let commands = self.poses + self.clears;
            let plural = if commands == 1 { "" } else { "s" };
            return Some(format!(
                "ran {commands} turtle command{plural} but drew no lines{tail}"
            ));
        }
        let plural = if self.segments == 1 { "" } else { "s" };
        let extent = self.bounds.as_ref().map_or(String::new(), |b| {
            format!(
                " spanning ({},{}) to ({},{})",
                num(b.min_x),
                num(b.min_y),
                num(b.max_x),
                num(b.max_y)
            )
        });
        Some(format!(
            "drew {} line segment{plural}{extent}{tail}",
            self.segments
        ))
    }

    /// Extends the bounding box to include the point `(x, y)`.
    fn grow(&mut self, x: f64, y: f64) {
        match &mut self.bounds {
            None => {
                self.bounds = Some(Bounds {
                    min_x: x,
                    min_y: y,
                    max_x: x,
                    max_y: y,
                });
            }
            Some(b) => {
                b.min_x = b.min_x.min(x);
                b.min_y = b.min_y.min(y);
                b.max_x = b.max_x.max(x);
                b.max_y = b.max_y.max(y);
            }
        }
    }
}

/// Reads a numeric capability argument as `f64`, accepting the `Int`/`Float`/`Bool` a drawing
/// primitive is passed (a bool, `set_turtle`'s `visible`, reads as 1.0/0.0). Any other kind, or a
/// read error, reads as `0.0`; the primitives are only ever called with numbers/booleans by the
/// library, so this is defensive, not a real path.
fn read_number(inst: &Instance, handle: Handle) -> f64 {
    match inst.kind_of(handle) {
        Ok(Kind::Int) => inst.as_int(handle).map_or(0.0, |n| n as f64),
        Ok(Kind::Float) => inst.as_float(handle).unwrap_or(0.0),
        Ok(Kind::Bool) => inst
            .as_bool(handle)
            .map_or(0.0, |b| if b { 1.0 } else { 0.0 }),
        _ => 0.0,
    }
}

/// Formats a coordinate: a whole number prints without a decimal point (`100`), anything else keeps
/// its shortest round-trip form (`95.10565…`). Deterministic, so the log replays stably.
fn num(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// Formats an RGBA color as `#rrggbbaa`, each channel rounded and clamped to `0..=255`.
fn hex_color(r: f64, g: f64, b: f64, a: f64) -> String {
    let byte = |v: f64| v.round().clamp(0.0, 255.0) as u8;
    format!(
        "#{:02x}{:02x}{:02x}{:02x}",
        byte(r),
        byte(g),
        byte(b),
        byte(a)
    )
}

/// Writes one `--draw-log` line to stdout and flushes, so it interleaves with streamed `print`
/// output in execution order (both flush immediately at their drive-loop step).
fn emit(line: &str) {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(line.as_bytes());
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
}
