//! Unit tests for the machine over real Doodle source, driven through the front
//! end. Split from `machine.rs` (the production code) so that file stays within
//! the hygiene length limit; these use the crate-internal stepping API to observe
//! the result register directly (which the integration tests in `tests/` cannot).

use super::*;

/// Builds an instance from Doodle source through the real front end, asserting
/// the program loads clean (no lex/parse/resolve diagnostics).
fn load_source(src: &str) -> Instance {
    use crate::diag::Severity;
    let nfc = crate::source::normalize(src);
    let parsed = crate::parse::parse_program(nfc.as_ref(), ModuleId(0));
    assert!(
        !parsed
            .diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error),
        "unexpected parse error(s): {:?}",
        parsed.diagnostics
    );
    let resolved = crate::resolve::resolve(parsed.ast, parsed.root, ModuleId(0));
    assert!(
        resolved.diagnostics.is_empty(),
        "unexpected resolve diagnostic(s): {:?}",
        resolved.diagnostics
    );
    Instance::load(resolved.module)
}

/// Advances until a value lands in the register or the machine halts.
fn step_to_first_value(inst: &mut Instance) {
    let mut steps = 0;
    while inst.result().is_none() && !inst.is_halted() {
        inst.step().expect("unexpected raise");
        steps += 1;
        assert!(steps < 1000, "machine failed to produce a value");
    }
}

/// Advances until the register holds `Int(want)`, failing if the machine
/// halts first (or runs away).
fn step_until_int(inst: &mut Instance, want: i64) {
    let mut steps = 0;
    while inst.result().and_then(Value::as_int) != Some(want) {
        assert!(
            !inst.is_halted(),
            "halted before the register reached {want}"
        );
        inst.step().expect("unexpected raise");
        steps += 1;
        assert!(steps < 1000, "register never reached {want}");
    }
}

/// Drives to halt, returning the last value the register held before the
/// module returned Void — i.e. the final expression's value.
fn drive_capturing_last_value(inst: &mut Instance) -> Option<Value> {
    let mut last = None;
    let mut steps = 0;
    while !inst.is_halted() {
        if let Some(v) = inst.result() {
            last = Some(v);
        }
        inst.step().expect("unexpected raise");
        steps += 1;
        assert!(steps < 10000, "machine failed to halt");
    }
    last
}

#[test]
fn value_readers_match_only_their_own_variant() {
    assert_eq!(Value::Int(7).as_int(), Some(7));
    assert_eq!(Value::Float(1.5).as_int(), None);
    assert_eq!(Value::Bool(true).as_bool(), Some(true));
    assert_eq!(Value::Int(0).as_bool(), None);
    // Avoid a float `==` (clippy::float_cmp); presence is enough to catch a
    // reader matching the wrong variant.
    assert!(Value::Float(2.5).as_float().is_some());
    assert!(Value::Nil.as_float().is_none());
    assert!(Value::Nil.is_nil());
    assert!(!Value::Int(0).is_nil());
}

#[test]
fn a_fresh_instance_is_ready_and_not_halted() {
    let inst = load_source("1\n");
    assert_eq!(inst.state(), InstanceState::Ready);
    assert!(!inst.is_halted());
}

#[test]
fn evaluates_an_int_literal_into_the_register() {
    let mut inst = load_source("42\n");
    step_to_first_value(&mut inst);
    assert_eq!(inst.result().and_then(Value::as_int), Some(42));
}

#[test]
fn a_bytes_literal_allocates_on_the_heap() {
    let mut inst = load_source("b\"hi\"\n");
    step_to_first_value(&mut inst);
    assert!(matches!(inst.result(), Some(Value::Bytes(_))));
}

#[test]
fn sequencing_runs_statements_in_order() {
    // Each statement's value lands in the register in turn: `1` then `2`.
    // A skip/miscount bug (e.g. advancing the sequence index by two) would
    // halt before the register ever reaches `2`, failing the second wait —
    // catching what the Void-completion tests alone cannot.
    let mut inst = load_source("1\n2\n");
    step_until_int(&mut inst, 1);
    step_until_int(&mut inst, 2);
}

#[test]
fn a_module_halts_and_completes_void() {
    // Several literal statements: sequencing runs each, and the top-level
    // return discards the final value (a module yields Void, L§6.11).
    let mut inst = load_source("1\ntrue\nnil\n");
    let mut steps = 0;
    while !inst.is_halted() {
        inst.step().expect("unexpected raise");
        steps += 1;
        assert!(steps < 1000, "machine failed to halt");
    }
    assert!(inst.result().is_none());
}

#[test]
fn arithmetic_evaluates_through_the_machine() {
    // Precedence + associativity flow through the continuation stack.
    let mut inst = load_source("2 * 3 + 4\n");
    assert_eq!(
        drive_capturing_last_value(&mut inst).and_then(Value::as_int),
        Some(10)
    );
}

#[test]
fn integer_overflow_promotes_to_bigint_through_the_machine() {
    let mut inst = load_source("9223372036854775807 + 1\n");
    assert!(matches!(
        drive_capturing_last_value(&mut inst),
        Some(Value::BigInt(_))
    ));
}

#[test]
fn comparison_and_boolean_ops_evaluate_through_the_machine() {
    for (src, expected) in [
        ("3 < 5\n", true),
        ("5 <= 5\n", true),
        ("2 != 2\n", false),
        ("1 == 1.0\n", true),
        ("true and false\n", false),
        ("false or true\n", true),
        ("not false\n", true),
    ] {
        let mut inst = load_source(src);
        let got = drive_capturing_last_value(&mut inst);
        assert!(
            matches!(got, Some(Value::Bool(b)) if b == expected),
            "{src:?} should evaluate to {expected}, got {got:?}"
        );
    }
}

#[test]
fn module_bindings_read_and_write_through_the_machine() {
    for (src, want) in [
        ("let x = 1\nx + 1\n", 2),        // read a `let`
        ("let x = 1\nx = x + 1\nx\n", 2), // reassign a `let`
        ("const c = 5\nc * 2\n", 10),     // read a `const`
    ] {
        let mut inst = load_source(src);
        assert_eq!(
            drive_capturing_last_value(&mut inst).and_then(Value::as_int),
            Some(want),
            "{src:?}"
        );
    }
}

#[test]
fn control_flow_evaluates_through_the_machine() {
    for (src, want) in [
        ("let x = if 3 > 2 then 10 else 20 end\nx\n", 10), // if-expr, true arm
        ("let x = if 2 > 3 then 10 else 20 end\nx\n", 20), // if-expr, else
        (
            "let s = if 5 < 0 then 1 else if 5 > 0 then 2 else 3 end\ns\n",
            2,
        ), // else-if
        ("let x = 0\nif 3 > 2 then x = 5 end\nx\n", 5),    // statement if mutates a global
        ("let n = 0\nwhile n < 3 do n = n + 1 end\nn\n", 3), // while counter
        // A construct-body local (`step`, a frame slot) summed across the loop.
        (
            "let t = 0\nlet i = 0\nwhile i < 3 do\n\
             let step = i + 1\nt = t + step\ni = i + 1\nend\nt\n",
            6,
        ),
    ] {
        let mut inst = load_source(src);
        assert_eq!(
            drive_capturing_last_value(&mut inst).and_then(Value::as_int),
            Some(want),
            "{src:?}"
        );
    }
}

#[test]
fn a_loop_keeps_running_until_something_stops_it() {
    // `break`/`return`/limits arrive later (M2a.6/M2a.9); the reloop mechanism
    // itself just cycles the body forever.
    let mut inst = load_source("loop do 1 end\n");
    for _ in 0..500 {
        assert!(!inst.is_halted());
        inst.step().expect("unexpected raise");
    }
    assert!(!inst.is_halted());
}

#[test]
fn calls_evaluate_through_the_machine() {
    for (src, want) in [
        ("fn add(a, b) a + b end\nadd(2, 3)\n", 5), // a plain function call
        // A call tree: nested calls as arguments, evaluated before the outer.
        (
            "fn double(x) x * 2 end\nfn add(a, b) a + b end\nadd(double(3), double(4))\n",
            14,
        ),
        ("fn sub(a, b) a - b end\nsub(b: 1, a: 10)\n", 9), // keyword args, out of order
        ("fn sub(a, b) a - b end\nsub(10, b: 3)\n", 7),    // positional + keyword
        ("fn f(x, y = 10) x + y end\nf(5)\n", 15),         // default supplies an omitted param
        ("fn f(x, y = 10) x + y end\nf(5, 1)\n", 6),       // an explicit arg overrides the default
        ("fn f(x, y = 10) x + y end\nf(x: 1, y: 2)\n", 3), // all by keyword
        // Non-tail recursion: `fib` calls itself twice per frame (a call tree).
        (
            "fn fib(n) if n < 2 then n else fib(n - 1) + fib(n - 2) end end\nfib(10)\n",
            55,
        ),
        ("let g = fn(x) x + 1 end\ng(41)\n", 42), // an anonymous function value, then called
        // A procedure mutates a module global across calls; its result is Void.
        ("let c = 0\nto bump() c = c + 1 end\nbump()\nbump()\nc\n", 2),
    ] {
        let mut inst = load_source(src);
        assert_eq!(
            drive_capturing_last_value(&mut inst).and_then(Value::as_int),
            Some(want),
            "{src:?}"
        );
    }
}

#[test]
fn the_is_operator_classifies_values() {
    for (src, expected) in [
        ("5 is Int\n", true),
        ("5 is Number\n", true),
        ("5 is Float\n", false),
        ("5 is Bool\n", false),
        ("3.0 is Float\n", true),
        ("3.0 is Number\n", true),
        ("true is Bool\n", true),
        ("nil is Nil\n", true),
        ("b\"hi\" is Bytes\n", true),
        ("let f = fn(x) x end\nf is Procedure\n", true),
        // Integer beyond i64 is a promoted BigInt, still an `Int` (MD §3).
        ("9223372036854775807 + 1 is Int\n", true),
    ] {
        let mut inst = load_source(src);
        let got = drive_capturing_last_value(&mut inst);
        assert!(
            matches!(got, Some(Value::Bool(b)) if b == expected),
            "{src:?} should be {expected}, got {got:?}"
        );
    }
}

#[test]
fn a_callable_is_one_canonical_value_across_reads() {
    // A plain `to`/`fn` interns one `CalObj` (MD §8): every read of the name
    // yields the same slab index (callable equality is identity, L§4.9).
    let mut inst = load_source("fn f(x) x end\nf\nf\n");
    let mut seen = Vec::new();
    while !inst.is_halted() {
        if let Some(Value::Callable(c)) = inst.result() {
            seen.push(c.0);
        }
        inst.step().expect("unexpected raise");
    }
    assert!(seen.len() >= 2, "expected to observe the callable twice");
    assert!(
        seen.iter().all(|&c| c == seen[0]),
        "each read of `f` must be the same canonical callable: {seen:?}"
    );
}

// Doodle indentation is not significant, so these multi-statement sources use
// `\`-continued string literals (the escaped newline + leading whitespace are
// stripped) to stay within the line-length limit.
#[test]
fn blocks_pass_invoke_and_read_enclosing_locals() {
    for (src, want) in [
        // Pass a block, invoke it, use its yielded value.
        (
            "fn call_it(do body)\n  body()\nend\ncall_it() do 42 end\n",
            42,
        ),
        // A block invoked with an argument, bound to the block's own parameter.
        (
            "fn twice_sum(do body)\nbody(1) + body(2)\nend\n\
             twice_sum() do (n) n * 10 end\n",
            30,
        ),
        // A block reads an enclosing fn local through a static link (§7): `x` lives
        // in `outer`'s frame, read from inside the block invoked within `give`.
        (
            "fn give(do body)\nbody()\nend\n\
             fn outer()\nlet x = 10\ngive() do x end\nend\nouter()\n",
            10,
        ),
        // A block mutates an enclosing local through the static link (§8.5).
        (
            "to run(do body)\nbody()\nend\n\
             fn outer()\nlet n = 0\nrun() do n = n + 5 end\nn\nend\nouter()\n",
            5,
        ),
        // The callee invokes the block several times (an iterating consumer),
        // accumulating into an enclosing local across invocations.
        (
            "to each3(do body)\nbody()\nbody()\nbody()\nend\n\
             fn sum()\nlet t = 0\neach3() do t = t + 1 end\nt\nend\nsum()\n",
            3,
        ),
    ] {
        let mut inst = load_source(src);
        assert_eq!(
            drive_capturing_last_value(&mut inst).and_then(Value::as_int),
            Some(want),
            "{src:?}"
        );
    }
}

/// Drives to halt, returning `(max frame-stack depth, max top-frame tail_count)`.
fn drive_tracking(inst: &mut Instance) -> (usize, u64) {
    let mut max_depth = 0;
    let mut max_tail = 0;
    let mut steps = 0u64;
    while !inst.is_halted() {
        max_depth = max_depth.max(inst.frame_depth());
        if let Some(tc) = inst.top_frame_tail_count() {
            max_tail = max_tail.max(tc);
        }
        inst.step().expect("unexpected raise");
        steps += 1;
        assert!(steps < 50_000_000, "machine failed to halt");
    }
    (max_depth, max_tail)
}

#[test]
fn a_tail_recursive_loop_runs_in_constant_memory() {
    // `count` tail-calls itself (kind-matched fn→fn), so its frame is REUSED each
    // iteration: the stack stays a constant depth and the frame absorbs every
    // iteration into its tail_count (exit criterion 1, MD §11).
    const N: u64 = 100_000;
    let src = format!("fn count(n)\nif n == 0 then 0 else count(n - 1) end\nend\ncount({N})\n");
    let mut inst = load_source(&src);
    let (max_depth, max_tail) = drive_tracking(&mut inst);
    // The module frame plus the one reused `count` frame — never deeper.
    assert!(max_depth <= 2, "tail loop grew the stack to {max_depth}");
    assert_eq!(
        max_tail, N,
        "the tail counter should read the iteration count"
    );
}

#[test]
fn a_kind_matched_tail_call_reuses_a_procedure_frame_too() {
    // Procedures have tail positions too (S-55): a `to` tail-calling a `to` reuses.
    // `walk` recurses via a tail call to itself; the frame is reused, so the stack
    // stays bounded and the tail counter reads the iteration count.
    const N: u64 = 50_000;
    let src = format!("to walk(n)\nif n == 0 then nil else walk(n - 1) end\nend\nwalk({N})\n");
    let mut inst = load_source(&src);
    let (max_depth, max_tail) = drive_tracking(&mut inst);
    assert!(max_depth <= 2, "tail loop grew the stack to {max_depth}");
    assert_eq!(max_tail, N);
}

#[test]
fn closures_capture_shared_mutable_bindings() {
    for (src, want) in [
        // make_counter (S-11): `inc` captures `n` and mutates it across calls; the
        // binding outlives `make_counter`'s frame.
        (
            "fn make_counter()\nlet n = 0\nfn inc()\nn = n + 1\nn\nend\ninc\nend\n\
             let c = make_counter()\nc()\nc()\nc()\n",
            3,
        ),
        // Two counters are INDEPENDENT: `a`'s count (1 then 2) is unaffected by a
        // `b()` call in between — a shared cell would make the last `a()` yield 3.
        (
            "fn make_counter()\nlet n = 0\nfn inc()\nn = n + 1\nn\nend\ninc\nend\n\
             let a = make_counter()\nlet b = make_counter()\na()\nb()\na()\n",
            2,
        ),
        // A captured PARAMETER: `add` closes over `base`.
        (
            "fn adder(base)\nfn add(x)\nbase + x\nend\nadd\nend\nlet a5 = adder(5)\na5(3)\n",
            8,
        ),
        // Two closures sharing one binding see each other's writes: `bump` (a `to`,
        // it only mutates) writes `x`, `peek` (a `fn`) reads it.
        (
            "fn pair()\nlet x = 0\nto bump()\nx = x + 1\nend\nfn peek()\nx\nend\n\
             bump()\nbump()\npeek()\nend\npair()\n",
            2,
        ),
        // A captured DEFAULTED parameter: the default fills the cell (bind_default).
        (
            "fn adder(base = 4)\nfn add(x)\nbase + x\nend\nadd\nend\nlet a = adder()\na(10)\n",
            14,
        ),
        // Nested closures: `inner` captures `x`, threaded through `mid` — one cell.
        (
            "fn outer()\nlet x = 7\nfn mid()\nfn inner()\nx\nend\ninner()\nend\n\
             mid()\nend\nouter()\n",
            7,
        ),
        // A closure created INSIDE a `do … end` block, capturing an outer fn local
        // through the defining chain (a `BlockOuter`-sourced capture, hops > 1).
        (
            "fn outer()\nlet x = 100\nlet result = 0\nto run(do body)\nbody()\nend\n\
             run() do\nfn get()\nx\nend\nresult = get()\nend\nresult\nend\nouter()\n",
            100,
        ),
    ] {
        let mut inst = load_source(src);
        assert_eq!(
            drive_capturing_last_value(&mut inst).and_then(Value::as_int),
            Some(want),
            "{src:?}"
        );
    }
}

#[test]
fn a_self_recursive_nested_helper_works() {
    // A locally-declared recursive `fn`/`to` references its own name — a capture of
    // its own binding (letrec). The closure must capture the SAME cell the binding
    // fills, or the recursive self-call reads an uninitialized cell (a spurious
    // UsedBeforeDefined). This is a common idiom (a recursive helper inside a fn).
    for (src, want) in [
        // Recursive `fn` helper: fact(5) = 120.
        (
            "fn outer()\nfn fact(n)\nif n < 2 then 1 else n * fact(n - 1) end\nend\n\
             fact(5)\nend\nouter()\n",
            120,
        ),
        // Recursive `to` helper mutating a captured counter: step(3) sets c = 3.
        (
            "fn outer()\nlet c = 0\nto step(n)\nif n > 0 then\nc = c + 1\nstep(n - 1)\nend\nend\n\
             step(3)\nc\nend\nouter()\n",
            3,
        ),
    ] {
        let mut inst = load_source(src);
        assert_eq!(
            drive_capturing_last_value(&mut inst).and_then(Value::as_int),
            Some(want),
            "{src:?}"
        );
    }
}

#[test]
fn loop_created_closures_capture_fresh_bindings() {
    // Exit criterion 5 / loop-fresh (L§5.4): each iteration's `let k` is a distinct
    // binding, so `grab` closures from different iterations capture DIFFERENT cells.
    // iter 0 captures k=0, iter 1 captures k=1 → 0 + 1*10 = 10. A shared cell would
    // make both read the final k (1) → 11.
    let src = "let first = nil\nlet second = nil\nlet i = 0\n\
               while i < 2 do\nlet k = i\nfn grab()\nk\nend\n\
               if i == 0 then first = grab else second = grab end\ni = i + 1\nend\n\
               first() + second() * 10\n";
    let mut inst = load_source(src);
    assert_eq!(
        drive_capturing_last_value(&mut inst).and_then(Value::as_int),
        Some(10)
    );
}

#[test]
fn a_loop_closure_shares_the_outer_counter_but_freshens_the_inner_let() {
    // The distinction that makes closures subtle: `i` (declared OUTSIDE the loop) is
    // one binding mutated across iterations, so both closures see its final value
    // (2); `k` (declared INSIDE) is loop-fresh, so each closure sees its own
    // iteration's value (10, 11). `g0` = 2 + 10 = 12, `g1` = 2 + 11 = 13.
    let src = "let f0 = nil\nlet f1 = nil\nlet i = 0\n\
               while i < 2 do\nlet k = i + 10\nfn g()\ni + k\nend\n\
               if i == 0 then f0 = g else f1 = g end\ni = i + 1\nend\n\
               f0() + f1() * 100\n";
    let mut inst = load_source(src);
    assert_eq!(
        drive_capturing_last_value(&mut inst).and_then(Value::as_int),
        Some(1312),
    );
}

#[test]
fn a_block_param_invoked_from_a_nested_block_composes() {
    for (src, want) in [
        // `relay` receives `body` and invokes it from INSIDE another helper's block
        // (`run() do body() end`) — a block-composition pattern. `body` reaches
        // `relay`'s block parameter via the defining chain (a BlockOuter callee).
        (
            "to run(do b)\nb()\nend\nto relay(do body)\nrun() do body() end\nend\n\
             fn f()\nlet t = 0\nrelay() do t = t + 9 end\nt\nend\nf()\n",
            9,
        ),
        // A `return` in the composed block exits the WRITING function (`f`), punching
        // through `run`, the wrapper block, and `relay`.
        (
            "to run(do b)\nb()\nend\nto relay(do body)\nrun() do body() end\nend\n\
             fn f()\nrelay() do return 7 end\n99\nend\nf()\n",
            7,
        ),
        // A `break` in the composed block exits the call that RECEIVED the block
        // (`relay(…)`), not the inner helper's call — so `relay`'s `marker = 1`
        // (after `run()`) never runs and the module `marker` stays 0. If `break`
        // wrongly targeted `run`'s call, `relay` would continue and yield 1.
        (
            "let marker = 0\nto run(do b)\nb()\nb()\nend\n\
             to relay(do body)\nrun() do body() end\nmarker = 1\nend\n\
             fn f()\nrelay() do break end\nmarker\nend\nf()\n",
            0,
        ),
    ] {
        let mut inst = load_source(src);
        assert_eq!(
            drive_capturing_last_value(&mut inst).and_then(Value::as_int),
            Some(want),
            "{src:?}"
        );
    }
}

#[test]
fn non_local_exits_reach_the_right_target() {
    for (src, want) in [
        // A plain `return`, and a conditional early `return` in a function.
        ("fn f() return 42 end\nf()\n", 42),
        ("fn f(x)\nif x > 0 then return 1 end\n2\nend\nf(5)\n", 1),
        ("fn f(x)\nif x > 0 then return 1 end\n2\nend\nf(0 - 5)\n", 2),
        // `return` inside a block exits the WRITING function, not the consumer
        // (punch-through): `f` yields 7; the `99` after `run()` never runs.
        (
            "to run(do body)\nbody()\nend\nfn f()\nrun() do return 7 end\n99\nend\nf()\n",
            7,
        ),
        // `return` punches through TWO nested blocks/consumers to the home fn.
        (
            "to outer(do body)\nbody()\nend\nto inner(do body)\nbody()\nend\n\
             fn f()\nouter() do inner() do return 5 end end\n99\nend\nf()\n",
            5,
        ),
        // `break` exits the block-consuming call: the iterating callee stops early.
        (
            "to loop3(do body)\nbody()\nbody()\nbody()\nend\n\
             fn f()\nlet hits = 0\nloop3() do\nhits = hits + 1\n\
             if hits == 2 then break end\nend\nhits\nend\nf()\n",
            2,
        ),
        // A valued `break` becomes the (function) consuming call's result (§8.5):
        // `search` yields the break value 7, not its fall-off value 999.
        (
            "fn search(do body)\nbody()\n999\nend\n\
             fn f()\nsearch() do break 7 end\nend\nf()\n",
            7,
        ),
        // `continue` ends the block invocation; the callee invokes it again. The
        // skipped invocation adds nothing, so only two of three add 10.
        (
            "to each3(do body)\nbody()\nbody()\nbody()\nend\n\
             fn f()\nlet sum = 0\nlet calls = 0\neach3() do\ncalls = calls + 1\n\
             if calls == 2 then continue end\nsum = sum + 10\nend\nsum\nend\nf()\n",
            20,
        ),
        // A valued `continue` is the block's yield to the callee (a mapping use).
        (
            "fn map_sum(do body)\nbody(1) + body(2)\nend\n\
             fn f()\nmap_sum() do (n) continue n * 100 end\nend\nf()\n",
            300,
        ),
        // Loop `break`/`continue` (same frame): sum 1,2, skip 3, sum 4,5, break at 6.
        (
            "fn f()\nlet i = 0\nlet sum = 0\nwhile i < 10 do\ni = i + 1\n\
             if i == 3 then continue end\nif i == 6 then break end\n\
             sum = sum + i\nend\nsum\nend\nf()\n",
            12,
        ),
        // A `loop` (endless without a break) exited by a `break`.
        (
            "fn f()\nlet n = 0\nloop do\nn = n + 1\nif n == 4 then break end\nend\nn\nend\nf()\n",
            4,
        ),
    ] {
        let mut inst = load_source(src);
        assert_eq!(
            drive_capturing_last_value(&mut inst).and_then(Value::as_int),
            Some(want),
            "{src:?}"
        );
    }
}

// --- M2a.10: garbage collection (precise non-moving mark-sweep, MD §15) ---

use crate::drive::{EngineFault, LimitKind, Limits};

/// Builds an instance from Doodle source under `limits` (for GC-trigger tests that
/// need a small step budget to stop an intentional infinite loop).
fn load_source_with_limits(src: &str, limits: Limits) -> Instance {
    use crate::diag::Severity;
    let nfc = crate::source::normalize(src);
    let parsed = crate::parse::parse_program(nfc.as_ref(), ModuleId(0));
    assert!(
        !parsed
            .diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error),
        "unexpected parse error(s): {:?}",
        parsed.diagnostics
    );
    let resolved = crate::resolve::resolve(parsed.ast, parsed.root, ModuleId(0));
    assert!(
        resolved.diagnostics.is_empty(),
        "unexpected resolve diagnostic(s)"
    );
    Instance::load_with_limits(resolved.module, limits)
}

/// Drives to halt, forcing a full collection **before every step** — maximal GC
/// pressure. Returns the last register value. If any reachable object is wrongly
/// collected, a later step reads a freed slab slot and panics (or the result
/// diverges), so agreement with the un-forced drive is the correctness signal.
fn drive_forcing_gc(inst: &mut Instance) -> Option<Value> {
    let mut last = None;
    let mut steps = 0;
    while !inst.is_halted() {
        inst.force_collect();
        if let Some(v) = inst.result() {
            last = Some(v);
        }
        inst.step().expect("unexpected raise under forced GC");
        steps += 1;
        assert!(steps < 20000, "machine failed to halt");
    }
    last
}

/// Drives until a step returns a fault, returning it (panics on completion/raise).
fn drive_to_fault(inst: &mut Instance) -> EngineFault {
    let mut steps = 0;
    loop {
        assert!(!inst.is_halted(), "completed without faulting");
        match inst.step() {
            Ok(()) => {}
            Err(Halt::Raise(r)) => panic!("unexpected raise: {r:?}"),
            Err(Halt::Fault(f)) => return f,
        }
        steps += 1;
        assert!(steps < 5_000_000, "did not fault in time");
    }
}

/// Collecting before **every** step must never change the result: GC is precise
/// (only unreachable objects are freed) and non-moving (survivors keep their slab
/// index). Each program builds live heap state — captured cells, a self-recursive
/// letrec cell, nested-closure capture chains, a running callable with no other
/// reference — that a missed root would corrupt.
#[test]
fn forced_collection_never_changes_the_result() {
    for (src, want) in [
        // A captured, mutated cell must survive across the calls that read it.
        // `bump` mutates and yields Void, so it is a `to` (a `fn` would fall off).
        (
            "fn outer()\nlet c = 0\nto bump()\nc = c + 1\nend\n\
             bump()\nbump()\nbump()\nc\nend\nouter()\n",
            3,
        ),
        // A self-recursive helper's own (letrec) cell must survive the recursion.
        (
            "fn outer()\nfn fact(n)\nif n < 2 then 1 else n * fact(n - 1) end\nend\n\
             fact(10)\nend\nouter()\n",
            3_628_800,
        ),
        // A capture reached through nested closures (hops > 1).
        (
            "fn outer()\nlet x = 10\nfn mid()\nfn inner()\nx\nend\ninner()\nend\n\
             mid()\nend\nouter()\n",
            10,
        ),
        // Blocks sharing the enclosing cell through the static link.
        (
            "to run(do body)\nbody()\nend\nfn outer()\nlet n = 0\n\
             run() do n = n + 5 end\nrun() do n = n + 5 end\nn\nend\nouter()\n",
            10,
        ),
    ] {
        let mut normal = load_source(src);
        assert_eq!(
            drive_capturing_last_value(&mut normal).and_then(Value::as_int),
            Some(want),
            "un-forced: {src:?}"
        );
        let mut forced = load_source(src);
        assert_eq!(
            drive_forcing_gc(&mut forced).and_then(Value::as_int),
            Some(want),
            "forced-GC: {src:?}"
        );
    }
}

/// A collection reclaims unreachable objects and leaves the reachable ones (the
/// built-in prelude, rooted through the namespace) untouched: after driving three
/// discarded byte-string statements, a forced collect returns the live count to its
/// pre-drive baseline.
#[test]
fn collection_reclaims_unreachable_garbage() {
    let mut inst = load_source("b\"aaaa\"\nb\"bbbb\"\nb\"cccc\"\n");
    let baseline = inst.live_object_count();
    drive_capturing_last_value(&mut inst);
    assert!(
        inst.live_object_count() > baseline,
        "the discarded byte strings should accumulate before collection"
    );
    inst.force_collect();
    assert_eq!(
        inst.live_object_count(),
        baseline,
        "collection reclaims exactly the unreachable byte strings"
    );
}

/// The automatic trigger keeps a garbage-producing loop in bounded memory: each
/// iteration allocates a fresh byte string it never keeps. The heap limit here is
/// set **below** what the loop would reach if nothing were reclaimed, yet the run
/// stops on the **step budget**, not the heap — proof the collector fired and kept
/// live memory bounded (without it the loop would `Heap`-fault first).
#[test]
fn the_gc_trigger_bounds_a_garbage_loop() {
    let mut inst = load_source_with_limits(
        "loop do\nb\"xxxxxxxx\"\nend\n",
        Limits {
            step_budget: 200_000,
            // 2 MiB: below the ~4 MiB the loop's garbage would reach un-reclaimed
            // within the step budget, but never reached because GC (floor 1 MiB)
            // keeps live memory oscillating around ~1 MiB.
            heap_bytes: 2 * 1024 * 1024,
            ..Limits::default()
        },
    );
    let fault = drive_to_fault(&mut inst);
    assert!(
        matches!(fault, EngineFault::LimitExceeded(LimitKind::StepBudget)),
        "expected StepBudget (GC kept memory under the heap limit), got {fault:?}"
    );
}

/// A heap limit **below** the GC trigger floor (`GC_MIN_BYTES`) still reclaims: the
/// safe point collects on heap pressure (not only when the threshold is crossed),
/// so a garbage loop under a tight sandbox limit runs to the step budget instead of
/// spuriously faulting on `Heap` (MD §15: the limit trips only after a failed
/// collect).
#[test]
fn a_garbage_loop_reclaims_under_a_heap_limit_below_the_gc_floor() {
    let mut inst = load_source_with_limits(
        "loop do\nb\"xxxxxxxx\"\nend\n",
        Limits {
            step_budget: 50_000,
            heap_bytes: 256 * 1024, // far below the 1 MiB GC floor
            ..Limits::default()
        },
    );
    let fault = drive_to_fault(&mut inst);
    assert!(
        matches!(fault, EngineFault::LimitExceeded(LimitKind::StepBudget)),
        "the last-ditch collect should keep a tight-limit garbage loop alive, got {fault:?}"
    );
}

// --- M2a.11: host handles (engine spec E§4.2, machine-design §16) ---

/// A retained handle keeps its value reachable across a collection (the M2a.11
/// accept criterion): a byte string retained through a handle survives GC after the
/// register that produced it is gone, and is reclaimed once the handle is released.
#[test]
fn a_retained_handle_keeps_its_value_alive_across_collection() {
    let mut inst = load_source("b\"hello\"\n");
    let baseline = inst.live_object_count();
    // Retain a handle to the byte string once it lands in the register.
    step_to_first_value(&mut inst);
    let handle = inst
        .retain_result()
        .expect("the result is a byte string, not Void");
    // Drive to halt: the module returns Void, so only the handle now references the
    // byte string.
    while !inst.is_halted() {
        inst.step().expect("unexpected raise");
    }
    inst.force_collect();
    assert!(
        inst.live_object_count() > baseline,
        "a live handle must keep its value across collection"
    );
    assert!(
        inst.resolve(handle).is_ok(),
        "the handle still resolves after GC"
    );
    // Releasing the last reference lets the next collection reclaim it.
    inst.release(handle).expect("releasing a live handle");
    inst.force_collect();
    assert_eq!(
        inst.live_object_count(),
        baseline,
        "after release the byte string is reclaimed"
    );
}

// --- M2a.12: GC-stress determinism harness (exit criterion 4) ---

/// A determinism-comparable summary of driving a program to its terminal state.
/// Heap-valued results are compared by KIND only: GC is non-moving, so a reachable
/// value's content never changes; a collection that wrongly freed a live value would
/// instead panic on a freed slot (failing the test) rather than alter the kind.
#[derive(PartialEq, Eq, Debug)]
enum Terminal {
    Value(ValueRepr),
    Raised(ExceptionKind),
    Faulted(crate::drive::EngineFault),
}

#[derive(PartialEq, Eq, Debug)]
enum ValueRepr {
    Void,
    Int(i64),
    Bool(bool),
    Nil,
    FloatBits(u64),
    HeapKind(&'static str),
}

fn value_repr(v: Option<Value>) -> ValueRepr {
    match v {
        None => ValueRepr::Void,
        Some(Value::Int(n)) => ValueRepr::Int(n),
        Some(Value::Bool(b)) => ValueRepr::Bool(b),
        Some(Value::Nil) => ValueRepr::Nil,
        Some(Value::Float(x)) => ValueRepr::FloatBits(x.to_bits()),
        Some(Value::Bytes(_)) => ValueRepr::HeapKind("bytes"),
        Some(Value::Str(_)) => ValueRepr::HeapKind("str"),
        Some(Value::BigInt(_)) => ValueRepr::HeapKind("bigint"),
        Some(Value::Callable(_)) => ValueRepr::HeapKind("callable"),
        Some(Value::Type(_)) => ValueRepr::HeapKind("type"),
        Some(_) => ValueRepr::HeapKind("other"),
    }
}

/// Drives `inst` to a terminal state, optionally forcing a collection **before every
/// step** (maximal GC pressure). Returns the terminal outcome in comparable form.
fn drive_terminal(inst: &mut Instance, force_gc: bool) -> Terminal {
    let mut last = None;
    let mut steps = 0;
    loop {
        if inst.is_halted() {
            return Terminal::Value(value_repr(last));
        }
        if force_gc {
            inst.force_collect();
        }
        if let Some(v) = inst.result() {
            last = Some(v);
        }
        match inst.step() {
            Ok(()) => {}
            Err(Halt::Raise(raise)) => return Terminal::Raised(raise.exception.kind),
            Err(Halt::Fault(fault)) => return Terminal::Faulted(fault),
        }
        steps += 1;
        assert!(steps < 2_000_000, "program did not terminate");
    }
}

/// The determinism gate (M2a exit criterion 4): every program in the corpus produces
/// a **bit-identical terminal outcome** whether or not a collection is forced at every
/// safe point. A GC that corrupted reachable state — a missed root, a wrongly-freed
/// live object, a nondeterministic sweep — would change or crash one of the two runs.
/// The corpus spans the evaluable demo subset: arithmetic (int/float/bignum), the
/// numeric tower, comparison and booleans, control flow, calls, closures (shared
/// cells, letrec recursion, loop-fresh, nested capture), blocks and non-local exits,
/// and every raise kind reachable today.
#[test]
fn gc_stress_determinism_gate_over_the_corpus() {
    let corpus = [
        // Arithmetic + the numeric tower (int, promotion to bignum, floored, float).
        "2 * 3 + 4\n",
        "9223372036854775807 + 1\n",
        "9223372036854775807 * 2 * 2\n",
        "(-7) // 2\n",
        "7 % 3\n",
        "2.5 + 1.5 * 2.0\n",
        "2 ** 10\n",
        // Comparison, equality across kinds, strict booleans.
        "3 < 5\n",
        "1 == 1.0\n",
        "true and (false or not false)\n",
        // Control flow.
        "let x = 0\nif 3 > 2 then x = 5 else x = 9 end\nx\n",
        "let n = 0\nwhile n < 20 do n = n + 1 end\nn\n",
        // Calls, keyword args, defaults.
        "fn add(a, b) a + b end\nadd(2, b: 3)\n",
        "fn f(a, b = 10) a + b end\nf(5)\n",
        // Closures: shared mutable cell, letrec recursion, nested capture.
        "fn outer()\nlet c = 0\nto bump()\nc = c + 1\nend\nbump()\nbump()\nc\nend\nouter()\n",
        "fn outer()\nfn fact(n)\nif n < 2 then 1 else n * fact(n - 1) end\nend\n\
         fact(15)\nend\nouter()\n",
        "fn outer()\nlet x = 7\nfn mid()\nfn inner()\nx\nend\ninner()\nend\nmid()\nend\nouter()\n",
        // Loop-fresh closures capturing distinct per-iteration bindings.
        "let last = 0\nlet i = 0\nwhile i < 5 do\nlet k = i\nfn get()\nk\nend\n\
         last = get()\ni = i + 1\nend\nlast\n",
        // Blocks: passing, invocation, enclosing-local access, non-local exits.
        "to run(do b)\nb()\nend\nfn outer()\nlet n = 0\nrun() do n = n + 5 end\nn\nend\nouter()\n",
        "fn f()\nto run(do b)\nb()\nend\nrun() do return 7 end\n99\nend\nf()\n",
        // Non-local exits carrying a value: a valued `break` (block consumer), a
        // valued `continue` (mapping), and loop `break`/`continue`.
        "fn search(do body)\nbody()\n999\nend\nfn f()\nsearch() do break 7 end\nend\nf()\n",
        "fn map_sum(do body)\nbody(1) + body(2)\nend\n\
         fn f()\nmap_sum() do (n) continue n * 100 end\nend\nf()\n",
        "fn f()\nlet i = 0\nlet s = 0\nwhile i < 10 do\ni = i + 1\n\
         if i == 3 then continue end\nif i == 6 then break end\ns = s + i\nend\ns\nend\nf()\n",
        // Heap value CONTENT under GC pressure, folded back to an exactly-compared
        // scalar (a by-kind terminal would hide a content divergence): a bignum
        // product reduced mod a prime, and a bignum carried through an in-flight
        // `break` (stressing the unwind heap-value GC root) before being reduced.
        "(9223372036854775807 * 9223372036854775807) % 1000000007\n",
        "fn search(do body)\nbody()\n0\nend\n\
         fn f()\nsearch() do break 9223372036854775807 * 3 end\nend\nf() % 1000000007\n",
        // Heap-valued results also compared by kind (content coverage is above).
        "b\"hello, world\"\n",
        "9223372036854775807 * 9223372036854775807\n",
        // Raise kinds reachable from clean-loading programs (an undefined name and a
        // statically-caught fall-off are load-time diagnostics, not run-mode raises).
        "1 / 0\n",
        "1 + true\n",
        "5()\n",
        "1.0e308 + 1.0e308\n",
    ];
    for src in corpus {
        let normal = drive_terminal(&mut load_source(src), false);
        let stressed = drive_terminal(&mut load_source(src), true);
        assert_eq!(
            normal, stressed,
            "GC-stress changed the terminal outcome of {src:?}"
        );
    }
}

/// The determinism gate extends to the resource-limit faults: a program stopped by a
/// limit faults at the **same** terminal outcome under GC pressure. (Driven under a
/// small step budget so the intentional non-terminating cases stop deterministically.)
#[test]
fn gc_stress_determinism_gate_over_limit_faults() {
    let budget = Limits {
        step_budget: 5_000,
        heap_bytes: 64 * 1024,
        stack_depth: 200,
    };
    for src in [
        "loop do\nb\"xxxx\"\nend\n",      // heap or step budget, whichever first
        "fn f(n)\n1 + f(n)\nend\nf(0)\n", // non-tail recursion → stack depth
        "let i = 0\nwhile i < 100000000 do\ni = i + 1\nend\ni\n", // step budget
    ] {
        let normal = drive_terminal(&mut load_source_with_limits(src, budget), false);
        let stressed = drive_terminal(&mut load_source_with_limits(src, budget), true);
        assert_eq!(
            normal, stressed,
            "GC-stress changed the limit-fault outcome of {src:?}"
        );
        assert!(
            matches!(normal, Terminal::Faulted(_)),
            "{src:?} should fault under the small limits, got {normal:?}"
        );
    }
}
