//! Unit tests for the intrinsic foreign-function mechanism (registry, synchronous
//! dispatch, `print`), driven through the real front end. Split from `mod.rs` so that
//! file stays within the hygiene length limit.

use super::*;
use crate::drive::{Directive, Outcome, run};
use crate::machine::Instance;
use crate::span::ModuleId;
use std::sync::Arc;

/// An `fn` intrinsic returning `42`, for testing value-yielding foreign calls.
fn answer() -> Intrinsic {
    Intrinsic {
        name: "answer".into(),
        kind: BodyKind::Func,
        params: Vec::new(),
        body: ForeignBody::Sync(|_ctx| Ok(Some(Value::Int(42)))),
    }
}

/// A block parameter for the misbehaving-consumer intrinsics below.
fn block_param() -> ForeignParam {
    ForeignParam {
        name: "body".into(),
        default: None,
        is_block: true,
    }
}

/// A **misbehaving** native block-consumer that drives its block **again after a
/// non-local exit** — the block `break`s (an S-46 `NonLocalExit` parks the unwind),
/// then this callback invokes it a second time instead of returning promptly. That
/// violates the E§7.6 host contract (rider 3: no new drives after a `NonLocalExit`);
/// the engine must fault, not run the block a second time.
fn redrives_after_a_non_local_exit() -> Intrinsic {
    Intrinsic {
        name: "misbehave".into(),
        kind: BodyKind::Proc,
        params: vec![block_param()],
        body: ForeignBody::Sync(|ctx| {
            let _ = ctx.invoke_block(vec![Value::Int(0)])?; // block `break`s → NonLocalExit
            let _ = ctx.invoke_block(vec![Value::Int(0)])?; // VIOLATION: drive again
            Ok(None)
        }),
    }
}

/// A **misbehaving** native block-consumer that **returns a value after a non-local
/// exit** — the block `break`s (an S-46 `NonLocalExit` parks the unwind), then this
/// callback returns a result instead of returning promptly with none. That violates
/// the E§7.6 host contract; the engine must fault rather than let the value stomp the
/// parked exit's target.
fn returns_a_value_after_a_non_local_exit() -> Intrinsic {
    Intrinsic {
        name: "misbehave".into(),
        kind: BodyKind::Proc,
        params: vec![block_param()],
        body: ForeignBody::Sync(|ctx| {
            let _ = ctx.invoke_block(vec![Value::Int(0)])?; // block `break`s → NonLocalExit
            Ok(Some(Value::Int(42))) // VIOLATION: a result after the exit
        }),
    }
}

/// A **well-behaved** native block-consumer that is a **procedure** (yields no value):
/// it invokes its block once and returns none. For the S-46 parity check — a foreign
/// function that yields no value is a `to` for the valued-`break` rule (S-10).
fn proc_consumer() -> Intrinsic {
    Intrinsic {
        name: "consume".into(),
        kind: BodyKind::Proc,
        params: vec![block_param()],
        body: ForeignBody::Sync(|ctx| {
            let _ = ctx.invoke_block(vec![Value::Int(0)])?;
            Ok(None)
        }),
    }
}

/// Loads `src` (which must load clean) with `registry`, driving it to completion
/// and returning the instance so the caller can read its output/outcome.
fn run_with(src: &str, registry: Registry) -> (Instance, Outcome) {
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
        "resolve diagnostic(s): {:?}",
        resolved.diagnostics
    );
    let mut inst = Instance::load_with_intrinsics(resolved.module, registry);
    let outcome = run(&mut inst, Directive::RunToCompletion);
    (inst, outcome)
}

fn registry_with(intrinsics: Vec<Intrinsic>) -> Registry {
    let mut r = Registry::new();
    for i in intrinsics {
        r.register(i).unwrap();
    }
    r
}

#[test]
fn register_rejects_a_duplicate_name() {
    let mut r = Registry::new();
    r.register(print()).unwrap();
    assert_eq!(
        r.register(print()),
        Err(HostError::DuplicateIntrinsic("print".into()))
    );
}

#[test]
fn register_rejects_a_builtin_type_value_name() {
    let mut r = Registry::new();
    let shadow_int = Intrinsic {
        name: "Int".into(),
        kind: BodyKind::Func,
        params: Vec::new(),
        body: ForeignBody::Sync(|_| Ok(Some(Value::Nil))),
    };
    assert_eq!(
        r.register(shadow_int),
        Err(HostError::CollidesWithBuiltin("Int".into()))
    );
}

#[test]
fn print_renders_its_argument_and_appends_a_newline() {
    let (inst, outcome) = run_with("print(1 + 2)\n", registry_with(vec![print()]));
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"3\n");
}

#[test]
fn print_renders_each_demo_scalar_kind() {
    let (inst, _) = run_with(
        "print(nil)\nprint(true)\nprint(-7)\nprint(1.5)\nprint(\"hi\")\n",
        registry_with(vec![print()]),
    );
    assert_eq!(inst.output(), b"nil\ntrue\n-7\n1.5\nhi\n");
}

#[test]
fn an_fn_intrinsic_yields_a_value_the_call_consumes() {
    let (inst, _) = run_with("print(answer())\n", registry_with(vec![print(), answer()]));
    assert_eq!(inst.output(), b"42\n");
}

#[test]
fn a_user_declaration_shadows_an_intrinsic() {
    // The program declares its own `print` (a no-op `to`), so the intrinsic never
    // runs — a user global is found first in the namespace scan (S-43 order).
    let (inst, outcome) = run_with(
        "to print(x)\nx\nend\nprint(\"hi\")\n",
        registry_with(vec![print()]),
    );
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"", "the intrinsic print was shadowed");
}

#[test]
fn a_missing_argument_raises() {
    let (_, outcome) = run_with("print()\n", registry_with(vec![print()]));
    assert!(matches!(outcome, Outcome::Raised(..)), "{outcome:?}");
}

#[test]
fn a_block_passed_to_a_block_less_intrinsic_raises() {
    // Parity with a source callable that takes no block (block.rs): passing a
    // `do … end` to `print` raises rather than silently dropping the block.
    let (_, outcome) = run_with("print(1) do\n1\nend\n", registry_with(vec![print()]));
    assert!(matches!(outcome, Outcome::Raised(..)), "{outcome:?}");
}

#[test]
fn a_to_intrinsic_result_used_as_a_value_raises() {
    // `print` is a `to` (Void); consuming its result in an expression raises at
    // the consuming site (the runtime Void backstop — the resolver's static
    // voidcheck only knows current-module `to`s, not intrinsics).
    let (_, outcome) = run_with("print(1) + 1\n", registry_with(vec![print()]));
    assert!(matches!(outcome, Outcome::Raised(..)), "{outcome:?}");
}

#[test]
fn a_valued_break_into_a_native_procedure_consumer_raises_no_value_destination() {
    // S-46 parity for the S-10 to-consumer half: a foreign function that yields no value
    // is a `to` for this purpose, so a `break 5` targeting a native Proc consumer has no
    // value destination and raises `no-value-destination` — the same as a Doodle `to`
    // consumer (conformance L7.10 `s10-001`).
    let (inst, outcome) = run_with(
        "consume() do (x)\nbreak 5\nend\n",
        registry_with(vec![proc_consumer()]),
    );
    let Outcome::Raised(value, _) = outcome else {
        panic!("expected Raised(NoValueDestination), got {outcome:?}");
    };
    let (slug, _) = inst.describe_raised(value);
    assert_eq!(
        slug,
        crate::machine::ExceptionKind::NoValueDestination.slug()
    );
}

/// A test intrinsic that **requests cancellation** when called — for exercising a cancel
/// that arrives while a native consumer's reentrant drive is running its block.
fn cancel_now() -> Intrinsic {
    Intrinsic {
        name: "cancel_now".into(),
        kind: BodyKind::Proc,
        params: Vec::new(),
        body: ForeignBody::Sync(|ctx| {
            ctx.request_cancel();
            Ok(None)
        }),
    }
}

#[test]
fn cancelling_inside_a_native_consumers_reentrant_drive_tears_it_down() {
    use crate::drive::EngineFault;
    use crate::machine::InstanceState;
    // The block calls `cancel_now()` (arming cancellation), then loops forever. The
    // loop's safe point — reached *inside* `each`'s reentrant drive — observes the flag,
    // and the cancel unwind must cross the native boundary: `invoke_block` relays the
    // parked `Cancel` as a `NonLocalExit`, `resume_native_boundary` declines to consume
    // it (it is not a `NativeBreak`), and the enclosing drive tears the whole stack down
    // to `Faulted(Cancelled)`. This exercises the S-46 cancel-across-the-boundary path
    // (the milestone risk-peak) that a cancel-before-`run` never reaches.
    let (inst, outcome) = run_with(
        "each([1]) do (x)\ncancel_now()\nloop do\n1\nend\nend\n",
        registry_with(vec![each(), cancel_now()]),
    );
    assert!(
        matches!(outcome, Outcome::Faulted(EngineFault::Cancelled)),
        "{outcome:?}"
    );
    assert_eq!(inst.state(), InstanceState::Faulted);
}

#[test]
fn sin_and_cos_yield_deterministic_floats() {
    // Exact anchors plus a fixed non-trivial angle, pinning the deterministic `libm`
    // values (the same on every target). `sin`/`cos` accept Int or Float; the result is
    // a `Float` (so `0`/`1` render with a `.0`).
    let (inst, _) = run_with(
        "print(sin(0))\nprint(cos(0))\nprint(sin(1.0))\nprint(cos(1.0))\n",
        registry_with(vec![print(), sin(), cos()]),
    );
    assert_eq!(
        inst.output(),
        b"0.0\n1.0\n0.8414709848078965\n0.5403023058681398\n"
    );
}

#[test]
fn a_proc_reassigns_a_module_level_binding() {
    // The turtle library keeps its state in module-level `let` bindings that the command
    // procedures mutate (Decision #7). Confirm a `to` can reassign a module global: two
    // `bump()` calls must leave `counter` at 2.
    let (inst, outcome) = run_with(
        "let counter = 0\nto bump()\ncounter = counter + 1\nend\nbump()\nbump()\nprint(counter)\n",
        registry_with(vec![print()]),
    );
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"2\n");
}

#[test]
fn is_string_and_equals_nil_branch_as_the_color_parser_needs() {
    // `pencolor` branches on `c is String` (named vs numeric color) and on `arg == nil`
    // (one-arg vs component form). Confirm both evaluate to the expected booleans.
    let (inst, _) = run_with(
        "print(\"red\" is String)\nprint(3 is String)\nprint(nil == nil)\nprint(7 == nil)\n",
        registry_with(vec![print()]),
    );
    assert_eq!(inst.output(), b"true\nfalse\ntrue\nfalse\n");
}

#[test]
fn a_drawing_capability_suspends_with_its_args_and_resolves_to_void() {
    // `draw_line` is a `to` **capability** (E§13): the call suspends, the host reads its
    // eight bound arguments (coordinates + RGBA, in order) and draws, then resolves with
    // `nil` — a `to` capability yields Void, so the module completes (E§7.5).
    use crate::drive::{CapabilityId, Resolution, resolve};
    use crate::machine::InstanceState;
    let (mut inst, outcome) = run_with(
        "draw_line(0, 0, 10, 20, 255, 128, 0, 255)\n",
        registry_with(vec![draw_line()]),
    );
    let Outcome::Suspended(request) = outcome else {
        panic!("expected Suspended, got {outcome:?}");
    };
    assert_eq!(request.capability, CapabilityId(0));
    let nums: Vec<i64> = request
        .args
        .iter()
        .map(|&h| inst.as_int(h).unwrap())
        .collect();
    assert_eq!(nums, vec![0, 0, 10, 20, 255, 128, 0, 255]);
    let nil = inst.make_nil();
    let done = resolve(&mut inst, Resolution::Value(nil));
    assert!(matches!(done, Outcome::Completed(None)), "{done:?}");
    assert_eq!(inst.state(), InstanceState::Completed);
}

#[test]
fn set_turtle_carries_a_pose_and_a_visibility_flag() {
    // `set_turtle(x, y, heading, visible)` (E§13): four bound arguments, the last a
    // `Bool` — the host poses/shows the marker without drawing, then resolves to Void.
    use crate::drive::{Resolution, resolve};
    let (mut inst, outcome) = run_with(
        "set_turtle(5, 7, 90, true)\n",
        registry_with(vec![set_turtle()]),
    );
    let Outcome::Suspended(request) = outcome else {
        panic!("expected Suspended, got {outcome:?}");
    };
    assert_eq!(request.args.len(), 4);
    assert_eq!(inst.as_int(request.args[0]).unwrap(), 5);
    assert_eq!(inst.as_int(request.args[2]).unwrap(), 90);
    assert!(inst.as_bool(request.args[3]).unwrap());
    let nil = inst.make_nil();
    assert!(matches!(
        resolve(&mut inst, Resolution::Value(nil)),
        Outcome::Completed(None)
    ));
}

#[test]
fn driving_a_block_again_after_a_non_local_exit_faults() {
    // Host-contract fault (E§7.6, S-46 rider 3): a callback that re-invokes its block
    // after the block took a non-local exit is a violation → `Faulted(Internal)`, NOT a
    // `Raised` and NOT a silent second run of the block.
    use crate::drive::EngineFault;
    let (inst, outcome) = run_with(
        "misbehave() do (x)\nbreak\nend\n",
        registry_with(vec![redrives_after_a_non_local_exit()]),
    );
    assert!(
        matches!(outcome, Outcome::Faulted(EngineFault::Internal)),
        "{outcome:?}"
    );
    assert_eq!(inst.state(), crate::machine::InstanceState::Faulted);
}

#[test]
fn returning_a_value_after_a_non_local_exit_faults() {
    // Host-contract fault (E§7.6): a callback must return promptly with no result once
    // its block took a non-local exit; returning a value instead faults rather than
    // stomping the parked exit's target.
    use crate::drive::EngineFault;
    let (inst, outcome) = run_with(
        "misbehave() do (x)\nbreak\nend\n",
        registry_with(vec![returns_a_value_after_a_non_local_exit()]),
    );
    assert!(
        matches!(outcome, Outcome::Faulted(EngineFault::Internal)),
        "{outcome:?}"
    );
    assert_eq!(inst.state(), crate::machine::InstanceState::Faulted);
}

/// A foreign `to` with a **heap-backed default** (a string materialized per call, S-42) and a
/// trailing **block parameter** — the M7.0 accept shape (a default + a block, binding per
/// L§8.3). Invokes its block once with the greeting (the passed value, or the default).
fn greet_with_default() -> Intrinsic {
    Intrinsic {
        name: "greet".into(),
        kind: BodyKind::Proc,
        params: vec![
            ForeignParam {
                name: "who".into(),
                default: Some(crate::machine::ConstValue::Str("world".into())),
                is_block: false,
            },
            ForeignParam {
                name: "body".into(),
                default: None,
                is_block: true,
            },
        ],
        body: ForeignBody::Sync(|ctx| {
            let who = ctx.args()[0];
            ctx.invoke_block(vec![who])?;
            Ok(None)
        }),
    }
}

#[test]
fn a_foreign_default_and_block_bind_per_l8_3() {
    // S-42 / D-M7-8: an omitted argument binds the descriptor's default (a heap-backed string
    // materialized per call from its `ConstValue` recipe); the trailing block parameter is
    // invoked reentrantly with the bound value. First call defaults to "world"; second passes
    // "moon".
    let (inst, outcome) = run_with(
        "greet() do (who)\nprint(who)\nend\ngreet(\"moon\") do (who)\nprint(who)\nend\n",
        registry_with(vec![print(), greet_with_default()]),
    );
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"world\nmoon\n");
}

#[test]
fn a_materialized_foreign_default_survives_a_gc_during_the_call() {
    // The default is materialized per call and bound into the block's reentrant drive. Under a
    // collect at every safe point (max GC pressure), the materialized string must survive that
    // drive — the recipe holds no heap reference, and there is no safe point between
    // materialization and binding, so the value is never unrooted at a collectable instant.
    use crate::diag::Severity;
    let src = "greet() do (who)\nprint(who)\nend\n";
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
    let mut inst = Instance::load_with_intrinsics(
        resolved.module,
        registry_with(vec![print(), greet_with_default()]),
    );
    inst.collect_at_every_safe_point();
    let outcome = run(&mut inst, Directive::RunToCompletion);
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"world\n");
}

/// A **host**-registered foreign `to` (the C-ABI shape, built with [`ForeignBuilder`] and a
/// [`Host`](ForeignBody::Host) callback): a materialized default (S-42) + a trailing block,
/// binding per L§8.3. The callback reads its one bound argument as a handle and invokes the
/// block reentrantly with it — exactly the path the C trampoline drives, minus the C fn.
fn greet_host() -> Intrinsic {
    ForeignBuilder::new("greet", BodyKind::Proc)
        .default_param("who", crate::machine::ConstValue::Str("world".into()))
        .block_param("body")
        .host(Arc::new(|ctx: &mut IntrinsicCtx| {
            let who = ctx.arg_handle(0).expect("one bound argument");
            let outcome = ctx.invoke_block_handles(&[who]);
            debug_assert_eq!(outcome, BlockOutcome::Completed);
            HostReply::Value(None)
        }))
}

#[test]
fn a_host_callback_default_and_block_bind_per_l8_3() {
    // The M7 accept shape through the engine's host-callback body (`ForeignBody::Host`): an
    // omitted argument binds the descriptor's default, and the trailing block is invoked
    // reentrantly with the bound value read back as a handle. First call defaults to "world";
    // second passes "moon" — the same behavior as the `Sync` `greet_with_default`, now via
    // the handle-based host surface the C ABI uses.
    let (inst, outcome) = run_with(
        "greet() do (who)\nprint(who)\nend\ngreet(\"moon\") do (who)\nprint(who)\nend\n",
        registry_with(vec![print(), greet_host()]),
    );
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"world\nmoon\n");
}

/// A **host** `fn` that reads its two integer args through the ctx value API and returns their
/// sum as a freshly-constructed value — exercising in-callback reads (`as_int`) + construction
/// (`make_int`) via `IntrinsicCtx`, the path the C ABI drives (M7.2b).
fn add_host() -> Intrinsic {
    ForeignBuilder::new("add", BodyKind::Func)
        .param("a")
        .param("b")
        .host(Arc::new(|ctx: &mut IntrinsicCtx| {
            let (Some(ha), Some(hb)) = (ctx.arg_handle(0), ctx.arg_handle(1)) else {
                ctx.fault_host();
                return HostReply::Value(None);
            };
            let (Ok(a), Ok(b)) = (ctx.as_int(ha), ctx.as_int(hb)) else {
                ctx.fault_host();
                return HostReply::Value(None);
            };
            let sum = ctx.make_int(a + b);
            ctx.release(ha).ok();
            ctx.release(hb).ok();
            // `sum` is handed to the result (apply consumes it); the arg handles are ours to free.
            HostReply::Value(Some(sum))
        }))
}

#[test]
fn a_host_fn_reads_args_and_constructs_a_result_via_the_ctx() {
    let (inst, outcome) = run_with(
        "print(add(2, 3))\n",
        registry_with(vec![print(), add_host()]),
    );
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"5\n");
}

/// A **host** `to` that raises a string it constructs through the ctx (M7.2b): the raise path
/// (`HostReply::Raise`) carrying an in-callback value.
fn boom_host() -> Intrinsic {
    ForeignBuilder::new("boom", BodyKind::Proc).host(Arc::new(|ctx: &mut IntrinsicCtx| {
        let msg = ctx.make_string(b"boom").expect("valid UTF-8");
        HostReply::Raise(msg)
    }))
}

#[test]
fn a_host_callback_raises_a_ctx_constructed_value() {
    // `boom()` raises the string "boom" it builds through the ctx; a `try`/`rescue` around it
    // binds `e` to that exact value (a program `rescue e` binds a raised value as-is), and
    // printing it yields "boom" — proving the raise path and `make_string` inside a callback.
    let (inst, outcome) = run_with(
        "try\nboom()\nrescue e\nprint(e)\nend\n",
        registry_with(vec![print(), boom_host()]),
    );
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"boom\n");
}

#[test]
fn register_rejects_a_reserved_prelude_name() {
    // A host intrinsic may not shadow a fixed prelude name (M5.10): `Error` and the well-known
    // protocol names + `to_string` join the built-in type values as reserved — else the
    // intrinsic would bind a silently shadowed cell in the shared prelude namespace.
    let make = |name: &str| Intrinsic {
        name: name.into(),
        kind: BodyKind::Func,
        params: Vec::new(),
        body: ForeignBody::Sync(|_ctx| Ok(Some(Value::Int(0)))),
    };
    for name in ["Int", "Error", "Stringable", "Hashable", "to_string"] {
        let mut reg = Registry::new();
        assert!(
            matches!(
                reg.register(make(name)),
                Err(HostError::CollidesWithBuiltin(_))
            ),
            "`{name}` should be a reserved prelude name"
        );
    }
    assert!(Registry::new().register(make("draw")).is_ok());
}
