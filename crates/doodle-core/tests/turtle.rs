//! M3.2: the Doodle turtle library driven through the engine with **scripted
//! capabilities**. The library (`doodle/turtle.doodle`) is prepended to each small
//! program as one module (the M3 prepend; the real module system is M5), then driven to
//! completion — every `draw_line`/`set_turtle`/`clear_canvas` call suspends as a `to`
//! capability, and the harness records it (as the JS canvas host will) and resolves it
//! with `nil`. The tests assert the recorded primitive sequence: geometry (positions,
//! headings), color parsing (named / hex-int / channels), the pen-up = alpha-0 rule, and
//! that `repeat` runs its block per iteration. No canvas — rendering is M3.6.

use doodle_core::drive::{CapabilityId, Directive, Outcome, Resolution, resolve, run};
use doodle_core::machine::{
    Handle, Instance, Kind, Registry, clear_canvas_intrinsic, cos_intrinsic, draw_line_intrinsic,
    print_intrinsic, set_turtle_intrinsic, sin_intrinsic,
};
use doodle_core::parse::parse_program;
use doodle_core::resolve::resolve as resolve_module;
use doodle_core::source::normalize;
use doodle_core::span::ModuleId;

/// The turtle library source, prepended to each test program (the M3 one-module prepend).
const TURTLE: &str = include_str!("../../../doodle/turtle.doodle");

/// Registration order fixes the capability ids (S-43): `print` 0, `sin` 1, `cos` 2 are
/// synchronous (they never suspend); `draw_line` 3, `set_turtle` 4, `clear_canvas` 5 are
/// the suspending platform primitives the harness records.
fn turtle_registry() -> Registry {
    let mut r = Registry::new();
    r.register(print_intrinsic()).unwrap();
    r.register(sin_intrinsic()).unwrap();
    r.register(cos_intrinsic()).unwrap();
    r.register(draw_line_intrinsic()).unwrap();
    r.register(set_turtle_intrinsic()).unwrap();
    r.register(clear_canvas_intrinsic()).unwrap();
    r
}

/// A recorded numeric/boolean argument of a primitive call (the demo values are all
/// numbers or the visibility flag).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Arg {
    Int(i64),
    Float(f64),
    Bool(bool),
}

/// One recorded platform-primitive call: the primitive's name and its bound arguments.
#[derive(Debug, Clone)]
struct Cmd {
    name: &'static str,
    args: Vec<Arg>,
}

impl Cmd {
    /// A numeric argument as `f64` (Int widens), for a position/channel check.
    fn num(&self, i: usize) -> f64 {
        match self.args[i] {
            Arg::Int(n) => n as f64,
            Arg::Float(x) => x,
            Arg::Bool(_) => panic!("arg {i} of `{}` is a bool, not a number", self.name),
        }
    }

    /// An integer argument (a color channel or heading), asserting it is an `Int`.
    fn int(&self, i: usize) -> i64 {
        match self.args[i] {
            Arg::Int(n) => n,
            other => panic!("arg {i} of `{}` is {other:?}, not an Int", self.name),
        }
    }

    /// A boolean argument (the visibility flag).
    fn boolean(&self, i: usize) -> bool {
        match self.args[i] {
            Arg::Bool(b) => b,
            other => panic!("arg {i} of `{}` is {other:?}, not a Bool", self.name),
        }
    }
}

/// The name of a suspending platform primitive by its capability id (registration order).
fn primitive_name(id: CapabilityId) -> &'static str {
    match id.0 {
        3 => "draw_line",
        4 => "set_turtle",
        5 => "clear_canvas",
        other => panic!("capability id {other} is not a drawing primitive"),
    }
}

/// Reads a capability-argument handle into an [`Arg`] (the demo primitives pass only
/// numbers and the visibility bool).
fn read_arg(inst: &Instance, handle: Handle) -> Arg {
    match inst.kind_of(handle).unwrap() {
        Kind::Int => Arg::Int(inst.as_int(handle).unwrap()),
        Kind::Float => Arg::Float(inst.as_float(handle).unwrap()),
        Kind::Bool => Arg::Bool(inst.as_bool(handle).unwrap()),
        other => panic!("unexpected primitive-argument kind {other:?}"),
    }
}

/// Loads the turtle library prepended to `program`, asserting it loads clean.
fn load_turtle(program: &str) -> Instance {
    use doodle_core::diag::Severity;
    let src = format!("{TURTLE}\n{program}\n");
    let nfc = normalize(&src);
    let parsed = parse_program(nfc.as_ref(), ModuleId(0));
    assert!(
        parsed
            .diagnostics
            .iter()
            .all(|d| d.severity != Severity::Error),
        "parse error(s): {:?}",
        parsed.diagnostics
    );
    let resolved = resolve_module(parsed.ast, parsed.root, ModuleId(0));
    assert!(
        resolved.diagnostics.is_empty(),
        "resolve diagnostic(s): {:?}",
        resolved.diagnostics
    );
    Instance::load_with_intrinsics(resolved.module, turtle_registry())
}

/// Drives `program` (with the turtle library prepended) to completion, recording every
/// platform-primitive call in order and resolving each with `nil` (a `to` capability
/// yields Void). Panics on any non-drawing outcome so a raise/fault surfaces loudly.
fn run_turtle(program: &str) -> Vec<Cmd> {
    let mut inst = load_turtle(program);
    let mut cmds = Vec::new();
    let mut outcome = run(&mut inst, Directive::RunToCompletion);
    loop {
        match outcome {
            Outcome::Suspended(request) => {
                let name = primitive_name(request.capability);
                let args = request.args.iter().map(|&h| read_arg(&inst, h)).collect();
                for &h in &request.args {
                    inst.release(h).unwrap();
                }
                cmds.push(Cmd { name, args });
                let nil = inst.make_nil();
                outcome = resolve(&mut inst, Resolution::Value(nil));
            }
            Outcome::Completed(_) => break,
            other => panic!("unexpected outcome: {other:?}"),
        }
    }
    cmds
}

/// Position/coordinate tolerance: trig on integer-degree headings lands a hair off exact
/// axis values (e.g. `cos(90°)` is ~6e-17, not 0), so positions are compared with a
/// generous epsilon while channels/headings/flags are exact.
const EPS: f64 = 1e-9;

fn assert_draw_line(cmd: &Cmd, x0: f64, y0: f64, x1: f64, y1: f64, rgba: [i64; 4]) {
    assert_eq!(cmd.name, "draw_line", "expected a draw_line, got {cmd:?}");
    for (i, want) in [x0, y0, x1, y1].into_iter().enumerate() {
        assert!(
            (cmd.num(i) - want).abs() < EPS,
            "draw_line arg {i}: got {}, want {want}",
            cmd.num(i)
        );
    }
    assert_eq!(
        [cmd.int(4), cmd.int(5), cmd.int(6), cmd.int(7)],
        rgba,
        "draw_line rgba"
    );
}

fn assert_set_turtle(cmd: &Cmd, x: f64, y: f64, heading: i64, visible: bool) {
    assert_eq!(cmd.name, "set_turtle", "expected a set_turtle, got {cmd:?}");
    assert!(
        (cmd.num(0) - x).abs() < EPS,
        "set_turtle x: got {}",
        cmd.num(0)
    );
    assert!(
        (cmd.num(1) - y).abs() < EPS,
        "set_turtle y: got {}",
        cmd.num(1)
    );
    assert_eq!(cmd.int(2), heading, "set_turtle heading");
    assert_eq!(cmd.boolean(3), visible, "set_turtle visible");
}

#[test]
fn a_square_traces_the_expected_primitive_sequence() {
    // `repeat` (Doodle) runs its block four times; each `forward` suspends in `draw_line`
    // *inside the block* and resumes — the demo never touches the S-15 native case. The
    // path closes: (0,0)→(0,100)→(100,100)→(100,0)→(0,0), red, headings 90/180/270/0.
    let cmds = run_turtle("pencolor(\"red\")\nrepeat(4) do\nforward(100)\nright(90)\nend");
    assert_eq!(cmds.len(), 8, "4 forwards + 4 turns: {cmds:#?}");
    let red = [255, 0, 0, 255];
    assert_draw_line(&cmds[0], 0.0, 0.0, 0.0, 100.0, red);
    assert_set_turtle(&cmds[1], 0.0, 100.0, 90, true);
    assert_draw_line(&cmds[2], 0.0, 100.0, 100.0, 100.0, red);
    assert_set_turtle(&cmds[3], 100.0, 100.0, 180, true);
    assert_draw_line(&cmds[4], 100.0, 100.0, 100.0, 0.0, red);
    assert_set_turtle(&cmds[5], 100.0, 0.0, 270, true);
    assert_draw_line(&cmds[6], 100.0, 0.0, 0.0, 0.0, red);
    assert_set_turtle(&cmds[7], 0.0, 0.0, 0, true);
}

#[test]
fn the_square_is_deterministic() {
    // The whole primitive sequence (positions included) is bit-identical run-to-run,
    // guarding within-process nondeterminism (a default-hasher/address/GC-timing leak on
    // the turtle's float path) that replay depends on (E§11). Cross-*target* trig identity
    // is pinned separately by the exact `sin`/`cos` golden in the intrinsic unit tests.
    let a = run_turtle("pencolor(\"red\")\nrepeat(4) do\nforward(100)\nright(90)\nend");
    let b = run_turtle("pencolor(\"red\")\nrepeat(4) do\nforward(100)\nright(90)\nend");
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(x.name, y.name);
        assert_eq!(x.args, y.args, "primitive args diverged between runs");
    }
}

#[test]
fn back_moves_opposite_the_heading() {
    // `back(30)` at heading 0 draws to (0, -30) — `forward(-dist)`, still a trail.
    let cmds = run_turtle("back(30)");
    assert_eq!(cmds.len(), 1);
    assert_draw_line(&cmds[0], 0.0, 0.0, 0.0, -30.0, [0, 0, 0, 255]);
}

#[test]
fn pen_up_glides_with_alpha_zero() {
    // Pen up is "draw with alpha 0", not a separate host mode: the move still emits a
    // `draw_line`, but its alpha channel is 0.
    let cmds = run_turtle("penup()\nforward(50)");
    assert_eq!(cmds.len(), 1);
    assert_draw_line(&cmds[0], 0.0, 0.0, 0.0, 50.0, [0, 0, 0, 0]);
}

#[test]
fn pencolor_parses_named_hex_and_channels() {
    // Each `pencolor` form sets the RGBA a following `forward` draws with.
    let named = run_turtle("pencolor(\"blue\")\nforward(1)");
    assert_eq!(
        [
            named[0].int(4),
            named[0].int(5),
            named[0].int(6),
            named[0].int(7)
        ],
        [0, 0, 255, 255]
    );

    let hex = run_turtle("pencolor(0xff8800)\nforward(1)");
    assert_eq!(
        [hex[0].int(4), hex[0].int(5), hex[0].int(6), hex[0].int(7)],
        [255, 136, 0, 255]
    );

    let rgb = run_turtle("pencolor(10, 20, 30)\nforward(1)");
    assert_eq!(
        [rgb[0].int(4), rgb[0].int(5), rgb[0].int(6), rgb[0].int(7)],
        [10, 20, 30, 255]
    );

    let rgba = run_turtle("pencolor(10, 20, 30, 40)\nforward(1)");
    assert_eq!(
        [
            rgba[0].int(4),
            rgba[0].int(5),
            rgba[0].int(6),
            rgba[0].int(7)
        ],
        [10, 20, 30, 40]
    );
}

#[test]
fn an_unknown_color_name_leaves_the_color_unchanged() {
    // A typo'd/unknown name is a true no-op — every channel, including alpha, is left as
    // the last color set. (Regression: `pencolor_named` once clobbered alpha to 255 on
    // the no-match path before drawing.)
    let cmds = run_turtle("pencolor(10, 20, 30, 40)\npencolor(\"purpel\")\nforward(1)");
    assert_eq!(cmds.len(), 1);
    assert_eq!(
        [
            cmds[0].int(4),
            cmds[0].int(5),
            cmds[0].int(6),
            cmds[0].int(7)
        ],
        [10, 20, 30, 40],
        "unknown color must not touch any channel, alpha included"
    );
}

#[test]
fn hideturtle_poses_the_marker_invisible() {
    // `hideturtle` sets the visibility flag and poses the (unmoved) marker.
    let cmds = run_turtle("hideturtle()");
    assert_eq!(cmds.len(), 1);
    assert_set_turtle(&cmds[0], 0.0, 0.0, 0, false);
}

#[test]
fn left_turns_counterclockwise_using_floored_modulo() {
    // `left(90)` from heading 0 must give 270, not -90: the update is
    // `(0 - 90) % 360`, and Doodle's `%` is *floored*, so a negative dividend wraps up
    // into [0, 360). A truncated `%` would emit heading -90 — this pins the difference.
    let cmds = run_turtle("left(90)");
    assert_eq!(cmds.len(), 1);
    assert_set_turtle(&cmds[0], 0.0, 0.0, 270, true);
}

#[test]
fn pendown_restores_drawing_after_penup() {
    // `penup` glides at alpha 0; `pendown` must restore drawing at the pen's alpha.
    let cmds = run_turtle("penup()\npendown()\nforward(10)");
    assert_eq!(cmds.len(), 1);
    assert_draw_line(&cmds[0], 0.0, 0.0, 0.0, 10.0, [0, 0, 0, 255]);
}

#[test]
fn showturtle_restores_visibility() {
    // After `hideturtle`, `showturtle` poses the marker visible again.
    let cmds = run_turtle("hideturtle()\nshowturtle()");
    assert_eq!(cmds.len(), 2);
    assert_set_turtle(&cmds[0], 0.0, 0.0, 0, false);
    assert_set_turtle(&cmds[1], 0.0, 0.0, 0, true);
}

#[test]
fn home_resets_position_and_heading() {
    // After moving and turning, `home` teleports back to the origin facing up (heading 0),
    // posing the marker without drawing a trail.
    let cmds = run_turtle("forward(10)\nright(90)\nhome()");
    assert_eq!(cmds.len(), 3, "{cmds:#?}");
    assert_draw_line(&cmds[0], 0.0, 0.0, 0.0, 10.0, [0, 0, 0, 255]);
    assert_set_turtle(&cmds[1], 0.0, 10.0, 90, true);
    assert_set_turtle(&cmds[2], 0.0, 0.0, 0, true);
}

#[test]
fn clear_wipes_the_canvas_and_homes_the_turtle() {
    // After a move, `clear` wipes the surface (`clear_canvas`, no args) and sends the
    // turtle home (a `set_turtle` back to the origin facing up).
    let cmds = run_turtle("forward(10)\nright(45)\nclear()");
    // forward → draw_line; right → set_turtle; clear → clear_canvas + home's set_turtle.
    assert_eq!(cmds.len(), 4, "{cmds:#?}");
    assert_eq!(cmds[2].name, "clear_canvas");
    assert!(cmds[2].args.is_empty(), "clear_canvas takes no arguments");
    assert_set_turtle(&cmds[3], 0.0, 0.0, 0, true);
}

#[test]
fn a_forward_suspends_as_a_capability() {
    // The pen-trail primitive is a *suspending* capability: a lone `forward` leaves the
    // instance Suspended in `draw_line` (id 3) with its eight bound arguments, before any
    // resolve. This is the suspend the JS pump animates (M3.5/M3.6).
    let mut inst = load_turtle("forward(10)");
    let outcome = run(&mut inst, Directive::RunToCompletion);
    let Outcome::Suspended(request) = outcome else {
        panic!("expected Suspended, got {outcome:?}");
    };
    assert_eq!(request.capability, CapabilityId(3));
    assert_eq!(request.args.len(), 8);
}
