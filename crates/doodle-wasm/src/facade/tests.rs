//! Native tests for the wasm facade core ([`Session`]) — driven through plain Rust, no
//! JS. They cover the front-end pipeline, the drive/resolve loop, outcome tagging, the
//! handle boundary, output, and position, so M3.4 is verified without a Node harness
//! (Decision #1). The JS-glue smoke test is `doodle-web`, M3.5.

use super::*;

#[test]
fn demo_runs_print_to_completion() {
    // The demo config (print-only, no prelude) matches the native conformance runner.
    let mut session = Session::demo("print(1 + 2)\n").unwrap();
    assert_eq!(
        session.drive(Directive::RunToCompletion, None),
        DriveOutcome::Completed
    );
    assert_eq!(session.output(), b"3\n");
    assert_eq!(session.prelude_bytes(), 0);
}

#[test]
fn a_parse_error_fails_to_load_with_diagnostics() {
    let Err(err) = Session::demo("print(\n") else {
        panic!("expected a load error");
    };
    assert!(!err.message.is_empty(), "a load error carries diagnostics");
}

#[test]
fn division_by_zero_surfaces_a_tagged_raise() {
    let mut session = Session::demo("1 / 0\n").unwrap();
    let outcome = session.drive(Directive::RunToCompletion, None);
    let DriveOutcome::Raised {
        kind,
        message,
        span,
    } = outcome
    else {
        panic!("expected Raised, got {outcome:?}");
    };
    assert_eq!(kind, "division-by-zero");
    assert!(!message.is_empty());
    assert!(span.is_some(), "the raise carries its site span");
}

#[test]
fn a_turtle_forward_suspends_in_draw_line_and_resolves_to_completion() {
    // draw_line is capability id 3 in the turtle registry (print 0, sin 1, cos 2,
    // draw_line 3, set_turtle 4, clear_canvas 5). `forward(10)` at heading 0 draws
    // (0,0)->(0,10) opaque black: args [0.0, 0.0, 0.0, 10.0, 0, 0, 0, 255].
    let mut session = Session::turtle("forward(10)\n").unwrap();
    assert!(
        session.prelude_bytes() > 0,
        "the turtle library was prepended"
    );

    let outcome = session.drive(Directive::RunToCompletion, None);
    let DriveOutcome::Suspended { capability, args } = outcome else {
        panic!("expected Suspended, got {outcome:?}");
    };
    assert_eq!(capability, 3, "draw_line's registration id");
    assert_eq!(args.len(), 8);
    assert!((session.as_float(args[3]).unwrap() - 10.0).abs() < 1e-9);
    assert_eq!(session.as_int(args[7]).unwrap(), 255);
    // A suspended instance has an executing position (the draw_line call site).
    assert!(session.current_position().is_some());

    // The host owns the request handles (S-17): release them, then resolve with nil (a
    // `to` capability yields Void) — the module completes.
    for &h in &args {
        session.release(h).unwrap();
    }
    let nil = session.make_nil();
    assert_eq!(session.resolve(nil, false, None), DriveOutcome::Completed);
}

#[test]
fn current_user_position_tracks_the_user_line_not_the_library() {
    // `forward(10)` (the user's program) calls `draw_line` inside the prepended turtle
    // library, so at the suspend the TOP-frame position is in the prelude, while
    // `current_user_position` reports the user's `forward(10)` call site — the basis for a
    // live line highlight of the user's program (M3.7).
    let mut session = Session::turtle("forward(10)\n").unwrap();
    let prelude = session.prelude_bytes();
    let DriveOutcome::Suspended { args, .. } = session.drive(Directive::RunToCompletion, None)
    else {
        panic!("expected Suspended");
    };
    let top = session.current_position().expect("a top-frame position");
    let user = session
        .current_user_position()
        .expect("a user-program position");
    assert!(
        top.start < prelude,
        "the top frame (draw_line) is inside the library ({} < {prelude})",
        top.start
    );
    assert!(
        user.start >= prelude,
        "the user position is in the program ({} >= {prelude})",
        user.start
    );
    for &h in &args {
        session.release(h).unwrap();
    }
}

#[test]
fn the_handle_boundary_round_trips_scalars() {
    let mut session = Session::demo("let a = 1\n").unwrap();
    let i = session.make_int(-7);
    assert_eq!(session.as_int(i).unwrap(), -7);
    assert_eq!(session.kind_of(i).unwrap(), Kind::Int);

    let f = session.make_float(2.5);
    assert_eq!(session.as_float(f).unwrap(), 2.5);

    let b = session.make_bool(true);
    assert!(session.as_bool(b).unwrap());

    let n = session.make_nil();
    assert!(session.is_nil(n).unwrap());

    let s = session.make_string(b"hi").unwrap();
    assert_eq!(session.string_bytes(s).unwrap(), b"hi");
    assert_eq!(session.kind_of(s).unwrap(), Kind::String);

    // A released handle is stale — a boundary error, never a wrong value.
    session.release(i).unwrap();
    assert!(matches!(session.as_int(i), Err(ValueError::Stale)));
}

#[test]
fn the_int_boundary_carries_a_bignum_capability_argument() {
    // A Doodle integer is arbitrary-precision (L§4.2), so a capability can receive one
    // that overflows i64. `pencolor(r, g, b)` stores its channels and `forward` passes
    // them to `draw_line`; a huge `r` therefore arrives as a bignum capability argument.
    // The fixed-width reader refuses it, but the arbitrary-precision reader carries it in
    // full — the host embedding decodes through the latter so the value never wedges the
    // decode (the failure mode the M3.9 review found in the pump).
    let huge = "1000000000000000000000000000000"; // 10^30, past i64
    let mut session = Session::turtle(&format!("pencolor({huge}, 0, 0)\nforward(1)\n")).unwrap();
    let DriveOutcome::Suspended { capability, args } =
        session.drive(Directive::RunToCompletion, None)
    else {
        panic!("expected a draw_line suspend");
    };
    assert_eq!(capability, 3, "draw_line");
    let r = args[4]; // [x0, y0, x1, y1, r, g, b, a]
    assert_eq!(session.kind_of(r).unwrap(), Kind::Int);
    assert_eq!(session.as_int(r), Err(ValueError::IntOutOfRange));
    assert_eq!(session.as_int_decimal(r).unwrap(), huge);
    for &h in &args {
        session.release(h).unwrap();
    }
}

#[test]
fn make_int_decimal_round_trips_any_magnitude() {
    let mut session = Session::demo("let a = 1\n").unwrap();
    // Beyond i64: interns as a bignum and reads back in full.
    let big = "-340282366920938463463374607431768211456"; // -(2^128)
    let h = session.make_int_decimal(big).unwrap();
    assert_eq!(session.kind_of(h).unwrap(), Kind::Int);
    assert_eq!(session.as_int_decimal(h).unwrap(), big);
    // Within i64: still a machine word the fixed-width reader accepts.
    let small = session.make_int_decimal("255").unwrap();
    assert_eq!(session.as_int(small).unwrap(), 255);
    // Malformed text is a boundary error, not a panic.
    assert_eq!(
        session.make_int_decimal("twelve"),
        Err(ValueError::MalformedInt)
    );
}

#[test]
fn a_stepped_and_a_fast_demo_reach_the_same_output() {
    // Determinism through the facade (E§7.7): fuel-slicing does not change the result.
    let mut fast = Session::demo("print(6 * 7)\n").unwrap();
    assert_eq!(
        fast.drive(Directive::RunToCompletion, None),
        DriveOutcome::Completed
    );

    let mut sliced = Session::demo("print(6 * 7)\n").unwrap();
    let mut outcome = sliced.drive(Directive::RunToCompletion, Some(1));
    for _ in 0..10_000 {
        if outcome != DriveOutcome::Paused("slice-end") {
            break;
        }
        outcome = sliced.drive(Directive::RunToCompletion, Some(1));
    }
    assert_eq!(outcome, DriveOutcome::Completed);
    assert_eq!(fast.output(), sliced.output());
    assert_eq!(sliced.output(), b"42\n");
}

// --- the M6.9 debug + inspection surface (E§8) through the facade ---

#[test]
fn entry_module_is_the_default_canonical_id() {
    let session = Session::demo("print(1)\n").unwrap();
    assert_eq!(session.entry_module(), "main");
}

#[test]
fn a_breakpoint_pauses_continue_and_exposes_locals_and_inspection() {
    // A record + list are function locals bound before line 5, where a breakpoint stops
    // `Continue`; the stack walk then shows the `describe` frame over the module top level,
    // lists its locals by name, a lazy `frame_local` mints one value, and the structural
    // inspection reads a record's fields and a list's elements without driving. (Module-level
    // `let`s are globals, not frame locals, so the locals live inside a `fn`.)
    let source = "record Point with x, y end\n\
        fn describe()\n\
        \x20 let p = Point(x: 1, y: 2)\n\
        \x20 let xs = [10, 20]\n\
        \x20 print(1)\n\
        \x20 p\n\
        end\n\
        describe()\n";
    let mut session = Session::demo(source).unwrap();
    let bp = session.set_breakpoint("main", 5);
    assert!(
        session
            .breakpoints()
            .iter()
            .any(|b| b.id.0 == bp && b.resolved),
        "the breakpoint resolves against the loaded entry module"
    );

    assert_eq!(
        session.drive(Directive::Continue, None),
        DriveOutcome::Paused("breakpoint")
    );

    let generation = session.pause_generation();
    let frames = session.stack_walk();
    assert_eq!(
        frames.len(),
        2,
        "the `describe` frame over the module top level"
    );
    let callable = frames[0]
        .callable
        .as_ref()
        .expect("frame 0 is the `describe` callable");
    assert_eq!(callable.name.as_deref(), Some("describe"));
    assert_eq!(callable.is_function, Some(true));
    assert!(
        frames[1].callable.is_none(),
        "the outer module top level is not a callable value"
    );
    let p_slot = frames[0]
        .locals
        .iter()
        .position(|n| n == "p")
        .expect("local `p` is in scope at the breakpoint");
    let xs_slot = frames[0]
        .locals
        .iter()
        .position(|n| n == "xs")
        .expect("local `xs` is in scope");

    // Lazy value: mint exactly the one binding, then inspect it purely (no drive).
    let p = session
        .frame_local(generation, 0, p_slot)
        .unwrap()
        .expect("`p` is bound");
    assert_eq!(session.record_type_name(p).unwrap(), "Point");
    assert_eq!(session.record_length(p).unwrap(), 2);
    assert_eq!(session.record_field_name(p, 0).unwrap(), "x");
    let x = session.record_field(p, 0).unwrap();
    assert_eq!(session.as_int(x).unwrap(), 1);
    session.release(x).unwrap();
    session.release(p).unwrap();

    let xs = session
        .frame_local(generation, 0, xs_slot)
        .unwrap()
        .expect("`xs` is bound");
    assert_eq!(session.list_length(xs).unwrap(), 2);
    let first = session.list_get(xs, 0).unwrap();
    assert_eq!(session.as_int(first).unwrap(), 10);
    session.release(first).unwrap();
    session.release(xs).unwrap();

    // The generation is invalidated by the next drive, so a stale frame read is a clean error.
    assert_eq!(
        session.drive(Directive::Continue, None),
        DriveOutcome::Completed
    );
    assert!(matches!(
        session.frame_local(generation, 0, p_slot),
        Err(super::StaleGeneration)
    ));
}

#[test]
fn step_stops_at_the_next_safe_point() {
    let mut session = Session::demo("print(1)\nprint(2)\n").unwrap();
    assert_eq!(
        session.drive(Directive::Step, None),
        DriveOutcome::Paused("step")
    );
}

#[test]
fn current_result_reads_the_value_at_a_fine_stop() {
    // In subexpression mode a step stops at each non-leaf completion; at the `2 + 3` stop the
    // result register holds 5 — the watch-it-run value (S-62).
    let mut session = Session::demo("let x = 2 + 3\nprint(x)\n").unwrap();
    session.set_observation_mode(true);
    for _ in 0..50 {
        match session.drive(Directive::Step, None) {
            DriveOutcome::Paused(_) => {
                if session.completed_position().is_some()
                    && let Some(handle) = session.current_result()
                {
                    let value = session.as_int(handle).ok();
                    session.release(handle).unwrap();
                    if value == Some(5) {
                        return;
                    }
                }
            }
            other => panic!("never reached the `2 + 3` fine stop: {other:?}"),
        }
    }
    panic!("never observed the fine-stop value 5");
}

#[test]
fn module_globals_read_through_the_session_with_generation_gating() {
    // A top-level program's variables are module globals (not frame locals), reachable through
    // the frame's home module. Reads are pause-generation gated like the frame bindings.
    let mut session = Session::demo("let count = 7\nconst name = \"hi\"\nprint(count)\n").unwrap();
    session.set_breakpoint("main", 3);
    assert_eq!(
        session.drive(Directive::Continue, None),
        DriveOutcome::Paused("breakpoint")
    );

    let generation = session.pause_generation();
    let module = session
        .stack_walk()
        .last()
        .unwrap()
        .module
        .expect("a live frame has a home module");

    let globals = session.module_global_names(module);
    let shape: Vec<(&str, &str)> = globals.iter().map(|g| (g.name.as_str(), g.kind)).collect();
    assert!(shape.contains(&("count", "let")));
    assert!(shape.contains(&("name", "const")));

    let count_slot = globals.iter().find(|g| g.name == "count").unwrap().slot;
    let handle = session
        .module_global_value(generation, module, count_slot)
        .unwrap()
        .expect("`count` is bound");
    assert_eq!(session.as_int(handle).unwrap(), 7);
    session.release(handle).unwrap();

    // A drive invalidates the generation, so a later global read errors cleanly.
    assert_eq!(
        session.drive(Directive::Continue, None),
        DriveOutcome::Completed
    );
    assert!(matches!(
        session.module_global_value(generation, module, count_slot),
        Err(super::StaleGeneration)
    ));
}

#[test]
fn raise_trap_pauses_before_unwinding_then_resumes_to_the_raise() {
    let mut session = Session::demo("let x = 1\nraise \"boom\"\n").unwrap();
    session.set_raise_trapping(true);

    assert_eq!(
        session.drive(Directive::Continue, None),
        DriveOutcome::Paused("raise-trap")
    );
    let raised = session.trapped_raise().expect("the trapped raise value");
    assert_eq!(session.string_bytes(raised).unwrap(), b"boom");
    session.release(raised).unwrap();
    assert!(
        session.trapped_raise_position().is_some(),
        "the raise site span is exposed pre-unwind"
    );

    // Resuming continues the unwind to the uncaught raise (E§8.7).
    let after = session.drive(Directive::Continue, None);
    let DriveOutcome::Raised { message, .. } = after else {
        panic!("expected Raised, got {after:?}");
    };
    assert!(message.contains("boom"));
}

#[test]
fn eval_to_string_renders_a_scalar() {
    let mut session = Session::demo("print(1)\n").unwrap();
    let n = session.make_int(42);
    match session.eval_to_string(n, 100_000) {
        AuxOutcomeData::Rendered(s) => {
            assert_eq!(session.string_bytes(s).unwrap(), b"42");
            session.release(s).unwrap();
        }
        other => panic!("expected Rendered, got {other:?}"),
    }
    session.release(n).unwrap();
}

#[test]
fn prelude_bytes_locates_the_program_start_under_nfc() {
    // The program portion of the combined+normalized source begins **exactly** at
    // prelude_bytes — the invariant the program-relative position mapping
    // (module_pos − prelude_bytes) depends on. The `\n` joiner is a starter, so NFC
    // cannot compose across the prelude→program boundary; a decomposed accented char in
    // the program exercises NFC without reaching that boundary. This locks the firewall
    // against a future joiner/normalize change.
    let program = "# cafe\u{0301} comment\nforward(5)\n"; // "café" (e + combining acute)
    let session = Session::turtle(program).unwrap();
    let prelude_bytes = session.prelude_bytes() as usize;
    assert_eq!(
        &session.source()[prelude_bytes..],
        doodle_core::source::normalize(program).as_ref(),
        "the program begins exactly at prelude_bytes in the normalized module source"
    );
}
