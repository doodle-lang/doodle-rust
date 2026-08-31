//! Unit tests for the machine over real Doodle source, driven through the front
//! end. Split from `machine.rs` (the production code) so that file stays within
//! the hygiene length limit; these use the crate-internal stepping API to observe
//! the result register directly (which the integration tests in `tests/` cannot).

use super::*;
use crate::span::ModuleId;

impl Machine {
    /// A bare machine for unit-testing pieces that take `&mut Machine` without a full
    /// [`Instance`] — the arith R8 size guard (`admit_bignum`) needs one to charge the
    /// budget and park a fault. Default (generous) limits, an empty frame stack, and a
    /// placeholder `error_type`: it never materializes an `Error`. (Lives here, in the
    /// length-exempt test module, rather than in `machine.rs`.)
    pub(crate) fn for_test() -> Self {
        let limits = Limits::default();
        Machine {
            frames: Vec::new(),
            reg: None,
            frame_serial: 0,
            unwind: None,
            ring: ring::RingBuffer::new(),
            fuel: FusedCounter::new(&limits),
            gc_threshold: limits::GC_MIN_BYTES,
            handles: HandleTable::new(),
            intrinsics: intrinsic::Registry::new(),
            output: Vec::new(),
            pending: None,
            load: modload::ModuleLoad::new(),
            protocols: super::protocol::Registry::default(),
            // No modules in this bare machine; the prelude id is a placeholder never resolved.
            prelude: crate::span::ModuleId(0),
            module_root_cells: Vec::new(),
            directive: Directive::RunToCompletion,
            pending_fault: None,
            foreign_roots: Vec::new(),
            dyn_stack: Vec::new(),
            handling: Vec::new(),
            error_type: TypeIdx(0),
            reentry_depth: 0,
            gc_every_safe_point: false,
            cancel: Arc::new(AtomicBool::new(false)),
            limits,
            load_diagnostics: Vec::new(),
        }
    }
}

/// Crate-internal, test-only accessors on [`Instance`] (they live here, in the
/// length-exempt test module, rather than in `machine.rs`). Only reachable from the
/// crate's own `#[cfg(test)]` code — an external integration test links the non-test
/// build, so these are invisible there (which is why the GC-stress hooks below can only
/// be driven from unit tests).
impl Instance {
    /// The value a handle names, generation-checked (E§4.2). The public typed readers
    /// (`as_int`, `kind_of`, …) build on this; this crate-internal form is what the M2a
    /// handle tests read with.
    pub(crate) fn resolve(&self, handle: Handle) -> Result<Value, HandleError> {
        self.machine.handles.resolve(handle)
    }

    /// The top frame's tail-iteration counter (E§8.3), or `None` when halted.
    pub(crate) fn top_frame_tail_count(&self) -> Option<u64> {
        self.machine.frames.last().map(|f| f.tail_count)
    }

    /// Forces a collection now (machine-design §15), independent of the trigger
    /// threshold — for tests that drive GC at chosen points to prove reachable state
    /// survives and garbage is reclaimed.
    pub(crate) fn force_collect(&mut self) {
        // Every loaded module's namespace cells are permanent roots on the machine
        // (`module_root_cells`), so a collection roots all modules' globals regardless of
        // which module is executing (AD5).
        gc::collect(&mut self.heap, &self.machine);
    }

    /// Makes every safe point collect (machine-design §15) — including those inside a
    /// reentrant nested drive, which the between-`step` `force_collect` idiom cannot
    /// reach — so a GC-stress test can collect at a transiently-rooted window.
    pub(crate) fn collect_at_every_safe_point(&mut self) {
        self.machine.gc_every_safe_point = true;
    }

    /// The number of live heap objects across all slabs (for GC tests).
    pub(crate) fn live_object_count(&self) -> u32 {
        self.heap.live_objects()
    }

    /// The described `kind` of the exception each `failed` module retains (S-8) — for
    /// asserting a failed load retained the right value (the re-raise itself is latent
    /// until a reload path exists, M9b).
    pub(crate) fn failed_module_error_kinds(&self) -> Vec<String> {
        self.machine
            .load
            .failed_values()
            .map(|v| exception::describe(&self.heap, self.machine.error_type, v).0)
            .collect()
    }
}

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

/// Interns a fresh host handle to the value bound to module global `name` (a driven
/// program's declaration) — how M6.1's inspection tests obtain a value handle.
fn global_handle(inst: &mut Instance, name: &str) -> Handle {
    let cell = control::find_cell(&inst.modules[0].namespace, name).expect("a bound global");
    let value = inst.heap.cell(cell).value.expect("an initialized global");
    inst.intern(value)
}

#[test]
fn structural_inspection_reads_records_dicts_callables_types() {
    let mut inst = load_source(
        "record Point with x, y end\n\
         let p = Point(x: 1, y: 2)\n\
         let d = {a: 10, b: 20}\n\
         fn greet(who) who end\n",
    );
    drive_capturing_last_value(&mut inst);

    // A record (E§4.4): type name, field count, field names in declaration order, values.
    let p = global_handle(&mut inst, "p");
    assert_eq!(inst.record_type_name(p).unwrap(), "Point");
    assert_eq!(inst.record_length(p).unwrap(), 2);
    assert_eq!(inst.record_field_name(p, 0).unwrap(), "x");
    assert_eq!(inst.record_field_name(p, 1).unwrap(), "y");
    let fy = inst.record_field(p, 1).unwrap();
    assert_eq!(inst.as_int(fy).unwrap(), 2);
    assert!(matches!(
        inst.record_field_name(p, 2),
        Err(ValueError::IndexOutOfBounds)
    ));

    // A dict (E§4.4, L§4.7): entries in insertion order.
    let d = global_handle(&mut inst, "d");
    assert_eq!(inst.dict_length(d).unwrap(), 2);
    let k0 = inst.dict_key(d, 0).unwrap();
    assert_eq!(inst.string_bytes(k0).unwrap(), b"a");
    let v1 = inst.dict_value(d, 1).unwrap();
    assert_eq!(inst.as_int(v1).unwrap(), 20);

    // A callable (E§8.2, D-M6-4): name, fn-vs-to kind, a source declaration position.
    let g = global_handle(&mut inst, "greet");
    assert_eq!(inst.callable_name(g).unwrap().as_deref(), Some("greet"));
    assert_eq!(inst.callable_is_function(g).unwrap(), Some(true));
    assert!(inst.callable_position(g).unwrap().is_some());
    assert!(inst.callable_docstring(g).unwrap().is_none());

    // A type value (E§4.4): its declared name.
    let point = global_handle(&mut inst, "Point");
    assert_eq!(inst.type_name(point).unwrap(), "Point");

    // A wrong-kind inspection reports, never panics.
    assert!(matches!(
        inst.dict_length(p),
        Err(ValueError::WrongKind { .. })
    ));
}

#[test]
fn frame_observation_reads_a_function_frames_locals() {
    use crate::drive::{Directive, Outcome, run};
    let mut inst = load_source("fn f(a)\n  let b = a + 1\n  b\nend\nf(10)\n");
    // StepInto until paused at a point inside `f` where `b` is bound, then inspect frame 0.
    let mut guard = 0;
    loop {
        let out = run(&mut inst, Directive::StepInto);
        assert!(
            matches!(out, Outcome::Paused(_)),
            "stepping pauses: {out:?}"
        );
        let locals = inst.frame_locals(0);
        if let Some(b) = locals.iter().find(|x| x.name == "b" && x.value.is_some()) {
            assert!(
                locals.iter().any(|x| x.name == "a"),
                "the parameter `a` is in scope: {locals:?}"
            );
            assert_eq!(inst.as_int(b.value.unwrap()).unwrap(), 11);
            // The tail-elided history and dynamic bindings read without panic; `f` opened
            // no `with`, so it has no dynamic bindings.
            let _ = inst.tail_elided_history();
            assert!(inst.frame_dynamic_bindings(0).is_empty());
            return;
        }
        guard += 1;
        assert!(guard < 100, "never stepped inside `f`");
    }
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
        // The callable trio (S-37): an `fn` is a Function and a Callable, not a
        // Procedure; a `to` is a Procedure and a Callable, not a Function.
        ("let f = fn(x) x end\nf is Function\n", true),
        ("let f = fn(x) x end\nf is Callable\n", true),
        ("let f = fn(x) x end\nf is Procedure\n", false),
        ("to p() end\np is Procedure\n", true),
        ("to p() end\np is Callable\n", true),
        ("to p() end\np is Function\n", false),
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
            Ok(_) => {}
            Err(Halt::Raise(v, _)) => panic!("unexpected raise: {v:?}"),
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

// --- M4.7: string repetition resource bound (L§4.4, S-59) ---

/// A string `*` whose result would exceed the heap limit faults rather than attempting an
/// allocation that could overflow `usize` or exhaust memory — the R8 pre-size cap, uniform
/// now across string `*`, bignum `*`, and `**`. Both a bignum count (unrepresentable) and a
/// representable count over a tight configured limit fault as `LimitExceeded(Heap)`.
#[test]
fn a_string_repeat_over_the_heap_limit_faults() {
    let mut inst = load_source("let x = \"a\" * (10 ** 30)\n");
    assert!(
        matches!(
            drive_to_fault(&mut inst),
            EngineFault::LimitExceeded(LimitKind::Heap)
        ),
        "a bignum repeat count must fault, not panic"
    );
    let mut inst = load_source_with_limits(
        "let x = \"abcdefgh\" * 100000\n",
        Limits {
            heap_bytes: 4096,
            ..Limits::default()
        },
    );
    assert!(matches!(
        drive_to_fault(&mut inst),
        EngineFault::LimitExceeded(LimitKind::Heap)
    ));
}

/// R8: `**` whose result would exceed the heap limit faults `LimitExceeded(Heap)` from the
/// pre-size estimate, before the bignum is built. `2 ** 10_000_000` is a ~1.25 MB integer;
/// under a tight heap it faults without allocating.
#[test]
fn a_power_over_the_heap_limit_faults_before_computing() {
    let mut inst = load_source_with_limits(
        "let x = 2 ** 10000000\n",
        Limits {
            heap_bytes: 4096,
            ..Limits::default()
        },
    );
    assert!(matches!(
        drive_to_fault(&mut inst),
        EngineFault::LimitExceeded(LimitKind::Heap)
    ));
}

/// R8: an exponent beyond the engine's computable range (u32) is a magnitude *fault*, not a
/// catchable raise — the retired `exponent-too-large` behavior, folded into the size cap.
/// `2 ** (10 ** 20)` has a u32-overflowing exponent, so it faults `LimitExceeded(Heap)` even
/// under the default heap; a magnitude-<= 1 base (`1 ** huge`) still computes (unit-tested).
#[test]
fn a_power_with_an_exponent_beyond_u32_faults() {
    let mut inst = load_source("let e = 10 ** 20\nlet x = 2 ** e\n");
    assert!(matches!(
        drive_to_fault(&mut inst),
        EngineFault::LimitExceeded(LimitKind::Heap)
    ));
}

/// R8: a bignum `*` whose product would exceed the heap limit faults `LimitExceeded(Heap)`
/// from the pre-size estimate (`a.bits() + b.bits()`), without attempting the multiply. The
/// operand `10 ** 1_000_000` (~415 KB, ~500 KB estimated) fits the 600 KB limit; its square
/// (~830 KB estimated) does not.
#[test]
fn a_multiply_over_the_heap_limit_faults_before_computing() {
    let mut inst = load_source_with_limits(
        "let a = 10 ** 1000000\nlet x = a * a\n",
        Limits {
            heap_bytes: 600_000,
            ..Limits::default()
        },
    );
    assert!(matches!(
        drive_to_fault(&mut inst),
        EngineFault::LimitExceeded(LimitKind::Heap)
    ));
}

/// R8 pre-charge: a bignum result costs step budget proportional to its byte size, so a huge
/// magnitude faults `StepBudget` under a bounded budget even when the heap would allow it.
/// `2 ** 10_000_000` charges ~2.5 M units — far past the 1 M budget — while a one-line
/// program's own safe points spend only a handful, so the fault is unambiguously the charge.
#[test]
fn a_bignum_power_charges_the_step_budget_by_its_size() {
    let mut inst = load_source_with_limits(
        "let x = 2 ** 10000000\n",
        Limits {
            step_budget: 1_000_000,
            heap_bytes: 1 << 34,
            stack_depth: 100_000,
        },
    );
    assert!(matches!(
        drive_to_fault(&mut inst),
        EngineFault::LimitExceeded(LimitKind::StepBudget)
    ));
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
    Raised(String),
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
            Ok(_) => {}
            Err(Halt::Raise(value, _)) => return Terminal::Raised(inst.describe_raised(value).0),
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
        // Protocol dispatch (M5.5): single dispatch on the first argument's runtime type,
        // the extends chain, and protocol defaults — the registry is index-addressed and
        // scanned linearly (no hashing/address identity), so dispatch is GC-order-stable.
        "record A with n end\nrecord B with n end\n\
         protocol P\nfn v(self)\nend\nend\n\
         implement P for A\nfn v(a)\nreturn 1\nend\nend\n\
         implement P for B\nfn v(b)\nreturn 2\nend\nend\n\
         v(A(n: 0)) + v(B(n: 0))\n",
        "protocol Base\nfn base(self)\nreturn 10\nend\nend\n\
         protocol Ext extends Base\nfn ext(self)\nend\nend\n\
         record R with n end\n\
         implement Ext for R\nfn ext(self)\nreturn 5\nend\nend\n\
         base(R(n: 0)) + ext(R(n: 0))\n",
        // A dispatch that raises (protocol-not-implemented) is GC-order-stable too.
        "record F with n end\nprotocol P\nfn v(self)\nend\nend\nv(F(n: 0))\n",
        // The M5.7 DRIVEN paths must survive a collection at the driven-call window (the
        // in-flight dict / the interpolation register): a dict keyed by an `implement
        // Hashable` type (insert + lookup drive the user `hash`), and interpolation of an
        // `implement Stringable` type (drives the user `to_string`). Native-hash keys and
        // scalar interpolation take the synchronous seam and would miss these conts.
        "record Point with x, y end\n\
         implement Hashable for Point\nfn hash(self)\nreturn self.x\nend\nend\n\
         let d = {}\nd[Point(x: 1, y: 2)] = 7\nd[Point(x: 1, y: 2)]\n",
        "record P with n end\n\
         implement Stringable for P\nfn to_string(self)\nreturn \"v!\"\nend\nend\n\
         \"[{P(n: 1)}]\"\n",
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

/// The determinism gate over the **M4 feature set** (exit criterion #6, "the double-run
/// trace diff covers everything"): records (value-copy vs ref-share, place-chain mutation),
/// dicts (the fixed-key-SipHash **hashing** that most risks leaking nondeterminism — string,
/// many-int, record, and cross-kind numeric keys), strings (AD4 seam concat, repetition,
/// interpolation, grapheme indexing), exceptions (raise → unwind → rescue, the `Error`
/// record), and `with`/`parameter` (the dynamic-binding save stack). Each program folds its
/// heap result back to an exactly-compared scalar/bool (a by-kind terminal would hide a
/// content divergence), and must be bit-identical whether or not GC runs at every safe point.
#[test]
fn gc_stress_determinism_gate_over_m4_features() {
    let corpus = [
        // Records: field read, value-record copy-on-bind, ref-record sharing, place chain.
        "record P with x, y end\nlet p = P(x: 3, y: 4)\np.x + p.y\n",
        "record P with n end\nlet a = P(n: 1)\nlet b = a\nb.n = 9\na.n\n",
        "ref record R with n end\nlet a = R(n: 1)\nlet b = a\nb.n = 9\na.n\n",
        "record P with n end\nrecord Q with p end\nlet q = Q(p: P(n: 5))\nq.p.n = 8\nq.p.n\n",
        // Dicts / hashing (criterion #6): string keys, 50 int keys built in a loop, a record
        // key, and cross-kind numeric-key coherence (1 and 1.0 hash alike) — folded via lookup.
        "let d = {a: 1, b: 2, c: 3, d: 4}\nd[\"a\"] + d[\"b\"] + d[\"c\"] + d[\"d\"]\n",
        "let d = {}\nlet i = 0\nwhile i < 50 do\nd[i] = i * i\ni = i + 1\nend\nd[7] + d[49]\n",
        "record K with a, b end\nlet d = {}\nd[K(a: 1, b: 2)] = 7\nd[K(a: 1, b: 2)]\n",
        "let d = {}\nd[1] = 5\nd[1.0]\n",
        // Strings: AD4 seam composition, repetition, interpolation, grapheme index — via `==`.
        // Bound to a local (a module-leading string literal would parse as a docstring, L§8.6).
        "let r = \"cafe\" + \"\\u{301}\" == \"caf\\u{e9}\"\nr\n",
        "let r = \"ab\" * 3 == \"ababab\"\nr\n",
        "let n = 21\nlet r = \"{n * 2}\" == \"42\"\nr\n",
        "let r = \"caf\\u{e9}\"[3] == \"\\u{e9}\"\nr\n",
        // Exceptions: a caught raise (unwind heap-value root), and the `Error` record's kind.
        "let r = 0\ntry\nr = 1 / 0\nrescue e\nr = 99\nend\nr\n",
        "let ok = false\ntry\n1 + true\nrescue e\nok = e.kind == \"type-mismatch\"\nend\nok\n",
        // with/parameter: dynamic bind + restore (the dyn_stack GC root) across the block.
        "parameter p = 1\nlet during = 0\nwith p = 5 do\nduring = p\nend\np * 10 + during\n",
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

/// The determinism gate extends to the **R8 magnitude faults** (D-M4-4): the pre-op size
/// estimate is a pure function of operands and limits, so a `**` too big for the heap or the
/// step budget faults at the **same** terminal outcome under GC pressure. One case trips the
/// heap estimate (tight heap), one the step-budget pre-charge (ample heap, bounded budget).
#[test]
fn gc_stress_determinism_gate_over_r8_magnitude_faults() {
    let cases = [
        (
            "let x = 2 ** 10000000\n",
            Limits {
                heap_bytes: 4096,
                step_budget: 1 << 40,
                stack_depth: 200,
            },
            LimitKind::Heap,
        ),
        (
            "let x = 2 ** 10000000\n",
            Limits {
                heap_bytes: 1 << 34,
                step_budget: 1_000_000,
                stack_depth: 200,
            },
            LimitKind::StepBudget,
        ),
    ];
    for (src, limits, kind) in cases {
        let normal = drive_terminal(&mut load_source_with_limits(src, limits), false);
        let stressed = drive_terminal(&mut load_source_with_limits(src, limits), true);
        assert_eq!(
            normal, stressed,
            "GC-stress changed the R8 magnitude fault of {src:?}"
        );
        assert!(
            matches!(normal, Terminal::Faulted(EngineFault::LimitExceeded(k)) if k == kind),
            "expected LimitExceeded({kind:?}) for {src:?}, got {normal:?}"
        );
    }
}

/// Loads `src` with the `print` and `each` intrinsics registered, so a test can observe
/// a side-effecting call (via captured output) and drive a native block-consumer.
fn load_source_with_print(src: &str) -> Instance {
    use crate::diag::Severity;
    let nfc = crate::source::normalize(src);
    let parsed = crate::parse::parse_program(nfc.as_ref(), ModuleId(0));
    assert!(
        !parsed
            .diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error),
        "parse error(s): {:?}",
        parsed.diagnostics
    );
    let resolved = crate::resolve::resolve(parsed.ast, parsed.root, ModuleId(0));
    assert!(
        resolved.diagnostics.is_empty(),
        "{:?}",
        resolved.diagnostics
    );
    let mut registry = Registry::new();
    registry.register(print_intrinsic()).unwrap();
    registry.register(each_intrinsic()).unwrap();
    Instance::load_with_intrinsics(resolved.module, registry)
}

/// Drives a multi-module program to its terminal, resolving each `import name` from `mods`
/// (name → source; an unlisted name is `module-not-found`), optionally collecting at **every**
/// safe point (maximal GC pressure across module loading + dispatch). Returns the captured
/// output and the terminal outcome in the by-kind comparable form.
fn drive_multi(main: &str, mods: &[(&str, &str)], gc_stress: bool) -> (Vec<u8>, Terminal) {
    use crate::drive::{Directive, ImportResolution, Outcome, resolve_import, run};
    let mut inst = load_source_with_print(main);
    if gc_stress {
        inst.collect_at_every_safe_point();
    }
    let mut outcome = run(&mut inst, Directive::RunToCompletion);
    let terminal = loop {
        match outcome {
            Outcome::SuspendedImport(ref req) => {
                let name = req
                    .path
                    .iter()
                    .map(|s| s.as_ref())
                    .collect::<Vec<&str>>()
                    .join("/");
                let res = match mods.iter().find(|(n, _)| *n == name) {
                    Some((n, src)) => ImportResolution::Source {
                        text: (*src).to_string(),
                        canonical_id: (*n).to_string(),
                    },
                    None => ImportResolution::NotFound,
                };
                outcome = resolve_import(&mut inst, res);
            }
            Outcome::Completed(v) => break Terminal::Value(value_repr(v)),
            Outcome::Raised(v, _) => break Terminal::Raised(inst.describe_raised(v).0),
            Outcome::Faulted(f) => break Terminal::Faulted(f),
            other => panic!("unexpected {other:?}"),
        }
    };
    (inst.output().to_vec(), terminal)
}

/// The determinism gate over **module loading + cross-module dispatch** (M5.10b): a
/// multi-module program produces a bit-identical transcript and terminal whether or not a
/// collection is forced at every safe point. Module namespaces, the wildcard/prelude
/// resolution, the protocol registry, and the shared prelude cells are all index-addressed and
/// scanned linearly (no hashing, no address identity, load-order numbering), so nothing a
/// collection could perturb is observable. Covers exports + `m.member`, selective + wildcard
/// import, a two-wildcard `ambiguous-import` (order-stable naming), and a protocol implemented
/// in a wrapper module and dispatched cross-module through the qualified `P.member` form.
#[test]
fn gc_stress_determinism_over_module_loading_and_dispatch() {
    let cases: &[(&str, &[(&str, &str)])] = &[
        (
            "import lib\nimport lib.*\nlib.hello()\nhello()\ngoodbye()\n",
            &[(
                "lib",
                "to hello()\nprint(\"hi\")\nend\nto goodbye()\nprint(\"bye\")\nend\n",
            )],
        ),
        (
            "import a.*\nimport b.*\ndraw()\n",
            &[
                ("a", "to draw()\nprint(\"a\")\nend\n"),
                ("b", "to draw()\nprint(\"b\")\nend\n"),
            ],
        ),
        (
            "import shapes.*\nprint(Shape.area(Square(side: 3)))\n",
            &[(
                "shapes",
                "record Square with side end\n\
                 protocol Shape\nfn area(self)\nend\nend\n\
                 implement Shape for Square\nfn area(s)\nreturn s.side * s.side\nend\nend\n",
            )],
        ),
        // Cross-module `with` (S-39, M5.9): the user's `with` rebinds the wrapper's parameter
        // cell (a live alias) and the wrapper reads it — the aliased cell and the `dyn_stack`
        // saved value must stay rooted across a collection inside the block.
        (
            "import turtle.*\nwith pen_color = 5 do\nshow()\nend\n",
            &[(
                "turtle",
                "parameter pen_color = 0\nto show()\nprint(pen_color)\nend\n",
            )],
        ),
    ];
    for (main, mods) in cases {
        let normal = drive_multi(main, mods, false);
        let stressed = drive_multi(main, mods, true);
        assert_eq!(
            normal, stressed,
            "GC-stress changed the transcript/terminal of {main:?}"
        );
    }
}

#[test]
fn each_keeps_heap_valued_elements_rooted_across_collection() {
    // Collect at EVERY safe point — including those **inside** `each`'s reentrant drive,
    // where the list of strings is rooted only by `foreign_roots` (MD §15). (A
    // between-`step` `force_collect` cannot reach them: the whole `each` runs within one
    // top-level step.) If that rooting were removed, the strings would be swept mid-drive
    // and `print` would read freed memory; the output proves they survive.
    let mut inst = load_source_with_print("each([\"a\", \"b\", \"c\"]) do (x)\nprint(x)\nend\n");
    inst.collect_at_every_safe_point();
    let mut steps = 0;
    while !inst.is_halted() {
        inst.step().expect("unexpected raise under forced GC");
        steps += 1;
        assert!(steps < 100_000, "each did not halt");
    }
    assert_eq!(inst.output(), b"a\nb\nc\n");
}

#[test]
fn an_import_bound_module_cell_survives_gc() {
    // The cell `import lib` binds (`lib` -> its module value) is allocated at runtime,
    // after the load-time namespace seeding, so `bind_target` must add it to the permanent
    // GC roots (AD5). Force a collection at every safe point and read a member across it —
    // if the bound cell were swept, `lib.answer` would fault.
    use crate::drive::{Directive, ImportResolution, Outcome, resolve_import, run};
    let mut inst = load_source_with_print("import lib\nprint(lib.answer)\n");
    inst.collect_at_every_safe_point();
    let mut outcome = run(&mut inst, Directive::RunToCompletion);
    while let Outcome::SuspendedImport(_) = &outcome {
        outcome = resolve_import(
            &mut inst,
            ImportResolution::Source {
                text: "const answer = 99\n".to_string(),
                canonical_id: "lib".to_string(),
            },
        );
    }
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"99\n");
}

#[test]
fn a_failed_load_retains_its_exception_for_re_raise() {
    // S-8: a module whose load raises enters `failed` **retaining that exception value**,
    // so a reload/re-import (M9b) re-raises it unchanged. The re-raise is latent in a
    // single run (a load failure is uncatchable and terminates), but the retention must be
    // set: here `boom` divides by zero, and its `failed` state keeps the division-by-zero
    // `Error`.
    use crate::drive::{Directive, ImportResolution, Outcome, resolve_import, run};
    let mut inst = load_source_with_print("import boom\n");
    let mut outcome = run(&mut inst, Directive::RunToCompletion);
    while let Outcome::SuspendedImport(_) = &outcome {
        outcome = resolve_import(
            &mut inst,
            ImportResolution::Source {
                text: "let x = 1 / 0\n".to_string(),
                canonical_id: "boom".to_string(),
            },
        );
    }
    assert!(matches!(outcome, Outcome::Raised(..)), "{outcome:?}");
    assert_eq!(inst.failed_module_error_kinds(), vec!["division-by-zero"]);
}

#[test]
fn a_sub_module_load_keeps_the_importers_globals_rooted_under_gc() {
    // A collection firing while an imported module's top level runs must root **every**
    // loaded module's globals, not just the executing sub-module's (AD5, E§6). With
    // collection forced at every safe point, the main module's `g` must survive the load
    // of module `a` — otherwise its list would be swept while `a` is the executing module
    // and `print(g[1])` would read freed memory. Driven through the real import
    // suspend/resume path (a bare `step` loop cannot resolve an import).
    use crate::drive::{Directive, ImportResolution, Outcome, resolve_import, run};
    let mut inst = load_source_with_print("let g = [10, 20, 30]\nimport a\nprint(g[1])\n");
    inst.collect_at_every_safe_point();
    let mut outcome = run(&mut inst, Directive::RunToCompletion);
    while let Outcome::SuspendedImport(_) = &outcome {
        outcome = resolve_import(
            &mut inst,
            ImportResolution::Source {
                // `a`'s top level allocates its own heap value, so a collection genuinely
                // fires while `a` — not the main module — is the executing module.
                text: "let junk = [1, 2, 3, 4, 5]\n".to_string(),
                canonical_id: "a".to_string(),
            },
        );
    }
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"20\n");
}

#[test]
fn interpolation_renders_joins_and_ignores_a_local_to_string() {
    // `{expr}` renders through the Stringable dispatcher and splices between the literal
    // runs; a user `fn to_string` is a real, callable binding but cannot hijack
    // interpolation, which binds the dispatcher directly (L§6.7, §15 hook 1, S-37).
    let src = concat!(
        "fn to_string(x)\n",
        "  return \"HIJACKED\"\n",
        "end\n",
        "print(to_string(0))\n",
        "print(\"n = {1 + 2}!\")\n",
    );
    let mut inst = load_source_with_print(src);
    let mut steps = 0;
    while !inst.is_halted() {
        inst.step().expect("interpolation raised");
        steps += 1;
        assert!(steps < 100_000, "interpolation did not halt");
    }
    assert_eq!(inst.output(), b"HIJACKED\nn = 3!\n");
}

#[test]
fn step_out_stops_the_instant_a_fn_returns_before_a_sibling_runs() {
    use crate::drive::{Directive, Outcome, run};
    // `StepOut` from inside `f` must pause the instant `f` returns — before the sibling
    // `h()` in `f() + h()` runs — regardless of whether `f` exits by an explicit
    // `return` (a frame-popping unwind) or by falling through (a `ReturnBarrier`). The
    // two paths must report the return safe point identically, so `h` (which prints)
    // has produced no output when `StepOut` pauses.
    for src in [
        "fn f()\nreturn 1\nend\nfn h()\nprint(\"H\")\nreturn 2\nend\nlet x = f() + h()\n",
        "fn f()\n1\nend\nfn h()\nprint(\"H\")\n2\nend\nlet x = f() + h()\n",
    ] {
        let mut inst = load_source_with_print(src);
        // Step into `f` (its body runs at frame depth 2 — module frame is depth 1).
        loop {
            let outcome = run(&mut inst, Directive::StepInto);
            assert!(matches!(outcome, Outcome::Paused(_)), "src={src:?}");
            if inst.frame_depth() == 2 {
                break;
            }
        }
        let outcome = run(&mut inst, Directive::StepOut);
        assert!(
            matches!(outcome, Outcome::Paused(_)),
            "src={src:?} {outcome:?}"
        );
        assert_eq!(
            inst.output(),
            b"",
            "StepOut overshot f's return and ran the sibling h(): src={src:?}"
        );
    }
}

// --- M4.5a: WithRestore cleanup runs as the unwinder pops (machine-design §12/§13) ---
//
// The `with`/`parameter` producer is M4.6, so these drive the mechanism directly:
// they simulate a live `with` binding (a dyn-stack entry + a `WithRestore` cont) and
// verify the unwinder restores it. The two distinct cleanup paths are exercised —
// `cleanup_and_pop_frame` (shared by cancel and the `break`/`return` frame-poppers) via
// cancellation, and `raise_unwind` via a raise. The `restore` primitive is tested on its
// own. The loop/block `break`/`continue` punch-through shares the same `restore` via
// `discard_cont`; it becomes conformance-testable once `with` exists (M4.6).

/// Simulates entering `with p = new` on the top frame: allocate a parameter cell holding
/// `old`, record `(cell, old)` on the dyn stack, overwrite the cell with `new`, and push
/// the matching `WithRestore` cont. Returns the cell so a test can assert restoration.
fn simulate_with(inst: &mut Instance, old: Value, new: Value) -> CellIdx {
    let cell = inst.heap.alloc_cell(crate::heap::CellKind::Let, Some(old));
    let mark = inst.machine.dyn_stack.len() as u32;
    inst.machine.dyn_stack.push((cell, old));
    inst.heap.cell_mut(cell).value = Some(new);
    inst.machine
        .frames
        .last_mut()
        .expect("a live frame")
        .conts
        .push(super::cont::Cont::WithRestore { dyn_mark: mark });
    cell
}

fn cell_int(inst: &Instance, cell: CellIdx) -> Option<i64> {
    inst.heap.cell(cell).value.and_then(Value::as_int)
}

#[test]
fn restore_reverts_bindings_down_to_the_mark() {
    let mut inst = load_source("let x = 1\n");
    let c1 = inst
        .heap
        .alloc_cell(crate::heap::CellKind::Let, Some(Value::Int(1)));
    let c2 = inst
        .heap
        .alloc_cell(crate::heap::CellKind::Let, Some(Value::Int(2)));
    let mark = inst.machine.dyn_stack.len() as u32;
    inst.machine.dyn_stack.push((c1, Value::Int(1)));
    inst.machine.dyn_stack.push((c2, Value::Int(2)));
    inst.heap.cell_mut(c1).value = Some(Value::Int(11));
    inst.heap.cell_mut(c2).value = Some(Value::Int(22));
    super::unwind::restore(&mut inst.machine, &mut inst.heap, mark);
    assert_eq!(cell_int(&inst, c1), Some(1), "c1 restored");
    assert_eq!(cell_int(&inst, c2), Some(2), "c2 restored");
    assert_eq!(
        inst.machine.dyn_stack.len() as u32,
        mark,
        "dyn stack drained"
    );
}

#[test]
fn cancellation_runs_withrestore_as_it_unwinds() {
    let mut inst = load_source("let x = 1\n");
    let cell = simulate_with(&mut inst, Value::Int(10), Value::Int(99));
    assert_eq!(cell_int(&inst, cell), Some(99), "the with is active");
    inst.machine.unwind = Some(super::unwind::Unwind::Cancel);
    let mut fault = None;
    for _ in 0..100 {
        match inst.step() {
            Ok(_) => {}
            Err(Halt::Fault(f)) => {
                fault = Some(f);
                break;
            }
            Err(other) => panic!("unexpected halt during cancel: {other:?}"),
        }
    }
    assert!(
        matches!(fault, Some(EngineFault::Cancelled)),
        "cancel faults"
    );
    assert_eq!(
        cell_int(&inst, cell),
        Some(10),
        "cancel restored the binding"
    );
    assert!(inst.machine.dyn_stack.is_empty(), "dyn stack drained");
}

#[test]
fn a_raise_runs_withrestore_as_it_unwinds_to_the_boundary() {
    let mut inst = load_source("let x = 1\n");
    let cell = simulate_with(&mut inst, Value::Int(10), Value::Int(99));
    let raised_value = super::exception::make_error(
        &mut inst.heap,
        inst.machine.error_type,
        "type-mismatch",
        "boom",
        &[],
    );
    inst.machine.unwind = Some(super::unwind::Unwind::Raise {
        value: raised_value,
        trace: Trace::at(None),
    });
    let mut raised = None;
    for _ in 0..100 {
        match inst.step() {
            Ok(_) => {}
            Err(Halt::Raise(value, _)) => {
                raised = Some(value);
                break;
            }
            Err(other) => panic!("unexpected halt during raise: {other:?}"),
        }
    }
    let raised = raised.expect("the uncaught raise reached the boundary");
    let (kind, message) = inst.describe_raised(raised);
    assert_eq!((kind.as_str(), message.as_str()), ("type-mismatch", "boom"));
    assert_eq!(
        cell_int(&inst, cell),
        Some(10),
        "raise restored the binding"
    );
    assert!(inst.machine.dyn_stack.is_empty(), "dyn stack drained");
}

// --- M4.6: the `with`/`parameter` producer, end to end (L§5.5, machine-design §13) ---
//
// The M4.5a tests above drive the cleanup mechanism with a *simulated* binding; these
// run a real `with` and confirm the producer builds a binding the exit paths restore.
// Normal completion and the non-local exits are conformance-tested (L5.5 `with-*`);
// cancellation is host-driven, so it is exercised here.

#[test]
fn cancelling_mid_with_restores_the_binding_before_faulting() {
    // A cancel that arrives while the drive is inside a `with` body tears the stack down
    // through the `WithRestore` the producer pushed, restoring the parameter to its
    // pre-`with` value before the terminal `Faulted(Cancelled)` (accept #5).
    let mut inst = load_source("parameter p = 1\nwith p = 2 do\nloop do\n1\nend\nend\n");
    // Step until the `with` binding is live — the drive is now inside the body's loop.
    for _ in 0..100 {
        if !inst.machine.dyn_stack.is_empty() {
            break;
        }
        inst.step().expect("no halt before the with binding opens");
    }
    assert_eq!(inst.machine.dyn_stack.len(), 1, "the with is active");
    let cell = inst.machine.dyn_stack[0].0;
    assert_eq!(
        cell_int(&inst, cell),
        Some(2),
        "p reads the with-bound value"
    );
    // Press the stop button, then drive to the fault.
    inst.cancel_token().cancel();
    let mut fault = None;
    for _ in 0..1000 {
        match inst.step() {
            Ok(_) => {}
            Err(Halt::Fault(f)) => {
                fault = Some(f);
                break;
            }
            Err(other) => panic!("unexpected halt during cancel: {other:?}"),
        }
    }
    assert!(matches!(fault, Some(EngineFault::Cancelled)), "{fault:?}");
    assert_eq!(
        cell_int(&inst, cell),
        Some(1),
        "cancel restored p to its default"
    );
    assert!(inst.machine.dyn_stack.is_empty(), "dyn stack drained");
}

// --- M4.5b: exceptions-as-values at the boundary (L§12.1, E§9) ---

#[test]
fn a_user_raised_string_describes_as_the_generic_raised_kind() {
    // `raise value` with a non-`Error` value: the boundary reports kind `"raised"` and
    // the value's own rendering (a string renders as its contents).
    let mut inst = load_source("raise \"nope\"\n");
    for _ in 0..100 {
        match inst.step() {
            Ok(_) => assert!(!inst.is_halted(), "raised before halting"),
            Err(Halt::Raise(value, _)) => {
                let (kind, message) = inst.describe_raised(value);
                assert_eq!((kind.as_str(), message.as_str()), ("raised", "nope"));
                return;
            }
            Err(other) => panic!("unexpected halt: {other:?}"),
        }
    }
    panic!("the raise never reached the boundary");
}

#[test]
fn an_uncaught_engine_error_describes_with_its_kind_slug() {
    // An engine raise materializes an `Error` record; the boundary reads its own
    // `kind`/`message` fields (L§12.1).
    let mut inst = load_source("1 + true\n");
    for _ in 0..100 {
        match inst.step() {
            Ok(_) => assert!(!inst.is_halted(), "raised before halting"),
            Err(Halt::Raise(value, _)) => {
                let (kind, _message) = inst.describe_raised(value);
                assert_eq!(kind, "type-mismatch");
                return;
            }
            Err(other) => panic!("unexpected halt: {other:?}"),
        }
    }
    panic!("the raise never reached the boundary");
}

// --- M4.5c: trace capture (E§8.2/§9, L§12.1) ---

#[test]
fn a_raise_in_tail_recursion_captures_frames_and_tail_elided_history() {
    // `countdown` tail-calls itself (last expression of the last statement), so PTC reuses
    // one frame; at n == 0 it raises a type error. The trace, captured at the raise, shows
    // the live frames (one with a nonzero tail_count) and the bounded tail-elided history.
    let mut inst = load_source(
        "to countdown(n)\n\
         if n == 0 then\n\
         1 + true\n\
         else\n\
         countdown(n - 1)\n\
         end\n\
         end\n\
         countdown(5)\n",
    );
    for _ in 0..100_000 {
        match inst.step() {
            Ok(_) => assert!(!inst.is_halted(), "raised before halting"),
            Err(Halt::Raise(value, trace)) => {
                assert_eq!(inst.describe_raised(value).0, "type-mismatch");
                assert!(!trace.frames.is_empty(), "the trace has live frames");
                assert!(
                    trace.frames.iter().any(|f| f.tail_count > 0),
                    "a tail-reused frame is recorded, got {:?}",
                    trace.frames
                );
                assert!(
                    !trace.tail_elided.is_empty(),
                    "the tail-elided history is captured"
                );
                return;
            }
            Err(other) => panic!("unexpected halt: {other:?}"),
        }
    }
    panic!("the raise never reached the boundary");
}
