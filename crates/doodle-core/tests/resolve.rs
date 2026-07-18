//! Resolver tests (M1.10a): name resolution rendered compactly so slot
//! assignment, static links, and free/module classification are visible.
//!
//! Reference notation: `L{slot}` = local slot, `B{hops}.{slot}` = block static
//! link, `M` = module name (free), `?` = unresolved. A cross-`fn` reference is a
//! closure **capture** (representation B): it resolves to `L`/`B` naming a capture
//! slot (see [`layout`] for the cell-boxing and capture wiring).

use doodle_core::ast::{Node, NodeId};
use doodle_core::parse::parse_program;
use doodle_core::resolve::{ExitTarget, Resolution, Resolved, resolve};
use doodle_core::source::normalize;
use doodle_core::span::ModuleId;

const M: ModuleId = ModuleId(0);

fn resolved(src: &str) -> Resolved {
    let nfc = normalize(src);
    let p = parse_program(nfc.as_ref(), M);
    assert!(
        p.diagnostics.is_empty(),
        "test source should parse cleanly: {:?}",
        p.diagnostics
    );
    resolve(p.ast, p.root, M)
}

/// Each module-level declaration as `name:Kind`, in order.
fn globals(r: &Resolved) -> Vec<String> {
    r.module
        .globals
        .iter()
        .map(|g| format!("{}:{:?}", g.name, g.kind))
        .collect()
}

/// Each `Ident` reference as `name:resolution`, in node order.
fn refs(r: &Resolved) -> Vec<String> {
    let m = &r.module;
    (0..m.ast.len())
        .filter_map(|i| {
            let id = NodeId(i as u32);
            match m.ast.node(id) {
                Node::Ident(name) => Some(format!("{name}:{}", fmt_res(&m.resolutions[i]))),
                _ => None,
            }
        })
        .collect()
}

fn fmt_res(res: &Option<Resolution>) -> String {
    match res {
        None => "?".to_string(),
        Some(Resolution::LocalSlot(s)) => format!("L{s}"),
        Some(Resolution::BlockOuter { hops, slot }) => format!("B{hops}.{slot}"),
        Some(Resolution::ModuleName(_)) => "M".to_string(),
    }
}

/// The free-name reference names, in order.
fn name_refs(r: &Resolved) -> Vec<String> {
    r.module
        .name_refs
        .iter()
        .map(|n| n.name.to_string())
        .collect()
}

/// Each `return`/`break`/`continue` as `kw:target`, in node order (`?` = no
/// target = a misplaced exit with a diagnostic).
fn exits(r: &Resolved) -> Vec<String> {
    let m = &r.module;
    (0..m.ast.len())
        .filter_map(|i| {
            let kw = match m.ast.node(NodeId(i as u32)) {
                Node::Return(_) => "return",
                Node::Break(_) => "break",
                Node::Continue(_) => "continue",
                _ => return None,
            };
            let t = match m.exit_targets[i] {
                None => "?",
                Some(ExitTarget::HomeCallable) => "home",
                Some(ExitTarget::ThisLoop(_)) => "loop",
                Some(ExitTarget::ThisBlock) => "block",
                Some(ExitTarget::ConsumerCall) => "consumer",
            };
            Some(format!("{kw}:{t}"))
        })
        .collect()
}

fn diags(r: &Resolved) -> Vec<&'static str> {
    r.diagnostics.iter().map(|d| d.code.slug()).collect()
}

/// Each `Call` as `callee:mark` in node order — `T` if marked a **tail** call
/// (L§8.7), `-` if not. The callee is its `Ident` name when simple, else `<expr>`.
fn tails(r: &Resolved) -> Vec<String> {
    let m = &r.module;
    (0..m.ast.len())
        .filter_map(|i| {
            let Node::Call { callee, .. } = m.ast.node(NodeId(i as u32)) else {
                return None;
            };
            let name = match m.ast.node(*callee) {
                Node::Ident(n) => n.to_string(),
                _ => "<expr>".to_string(),
            };
            Some(format!(
                "{name}:{}",
                if m.tail_calls[i] { "T" } else { "-" }
            ))
        })
        .collect()
}

/// Every callable's frame layout, in table order: `Kind(slots)[caps]`. A slot is
/// prefixed `*` when cell-boxed; a capture is `slot<-hops.slot` (target slot ←
/// static-link source from the creating frame). Makes capture wiring visible.
fn layout(r: &Resolved) -> Vec<String> {
    r.module
        .callables
        .iter()
        .map(|c| {
            let slots: Vec<String> = c
                .slot_names
                .iter()
                .zip(&c.cell_boxed)
                .map(|(n, &b)| if b { format!("*{n}") } else { n.to_string() })
                .collect();
            let caps: Vec<String> = c
                .captures
                .iter()
                .map(|cap| format!("{}<-{}.{}", cap.slot, cap.from.hops, cap.from.slot))
                .collect();
            let caps = if caps.is_empty() {
                String::new()
            } else {
                format!("[{}]", caps.join(","))
            };
            format!("{:?}({}){caps}", c.kind, slots.join(","))
        })
        .collect()
}

#[test]
fn module_level_lets_are_globals_referenced_by_module_name() {
    // A module-level `let`/`const`/`to`/`fn` binds a module cell (a global); a
    // reference to it is a module name (resolved to the cell at load, AD5).
    let r = resolved("let x = 1\nlet y = x");
    assert_eq!(globals(&r), vec!["x:Let", "y:Let"]);
    assert_eq!(refs(&r), vec!["x:M"]); // the `x` in `let y = x`
    assert_eq!(name_refs(&r), vec!["x"]);
}

#[test]
fn module_const_keeps_its_kind() {
    // A module-level `const` is a non-assignable global — its kind must survive
    // (the M1.10b const-reassignment check and the M2a cell kind read it).
    let r = resolved("const c = 1");
    assert_eq!(globals(&r), vec!["c:Const"]);
}

#[test]
fn function_params_and_body_locals_take_slots() {
    let r = resolved("fn double(n)\n  n\nend");
    assert_eq!(globals(&r), vec!["double:Fn"]);
    // `n` in the body resolves to the param's slot 0.
    assert_eq!(refs(&r), vec!["n:L0"]);
    // The callable table has the module top level + the function (one slot: `n`).
    let f = r
        .module
        .callables
        .iter()
        .find(|c| matches!(c.kind, doodle_core::resolve::BodyKind::Func))
        .expect("the fn");
    assert_eq!(f.slot_count, 1);
    assert_eq!(&*f.slot_names, &["n".into()]);
    assert_eq!(f.params.len(), 1);
    assert_eq!(f.params[0].slot, 0);
}

#[test]
fn param_default_resolves_in_enclosing_scope_not_a_sibling_param() {
    // L§8.2: a default evaluates in the declaration's (enclosing) lexical scope,
    // so `x` in `y`'s default is NOT the earlier param `x` (slot 0) — with no
    // outer `x`, it is a free module name. (params: x→0, y→1.)
    let r = resolved("fn f(x, y = x)\n  y\nend");
    assert_eq!(refs(&r), vec!["x:M", "y:L1"]);
    assert_eq!(name_refs(&r), vec!["x"]);
}

#[test]
fn body_local_shadows_and_takes_next_slot() {
    // `to` at module level is a global; its body locals take slots 0,1; a
    // reference resolves to the nearest local.
    let r = resolved("to f(a)\n  let b = a\n  b\nend");
    assert_eq!(globals(&r), vec!["f:Proc"]);
    // `a` (in `let b = a`) -> param slot 0; `b` (last stmt) -> local slot 1.
    assert_eq!(refs(&r), vec!["a:L0", "b:L1"]);
    assert!(name_refs(&r).is_empty());
}

#[test]
fn construct_body_locals_share_the_enclosing_frame() {
    // A `let` inside an `if` at module level is scoped to that body (L§5.4), so it
    // is a module-top-level frame SLOT, not a global.
    let r = resolved("if true then\n  let a = 1\n  a\nend");
    assert!(globals(&r).is_empty()); // `a` is a slot, not a global
    assert_eq!(refs(&r), vec!["a:L0"]);
}

#[test]
fn block_argument_outer_reference_is_a_static_link() {
    // `xs` (the fn param) referenced from inside the `do … end` block is a static
    // link one hop up; the block param `y` is a local slot in the block frame.
    let r = resolved("fn f(xs)\n  each(xs) do (y)\n    show(xs, y)\n  end\nend");
    // In node order: `each`(M), `xs`(arg, L0 in fn), `show`(M), `xs`(B1.0), `y`(L0).
    assert_eq!(
        refs(&r),
        vec!["each:M", "xs:L0", "show:M", "xs:B1.0", "y:L0"]
    );
    // A block reaches an enclosing fn local by static link, not capture — the fn
    // frame has no capture slots and `xs` is not cell-boxed.
    assert_eq!(layout(&r), vec!["Block(y)", "Func(xs)", "ModuleTopLevel()"]);
}

#[test]
fn cross_fn_reference_resolves_as_a_capture() {
    // A nested `fn` referencing an enclosing fn's local crosses an `fn` boundary —
    // a closure capture (representation B): the enclosing slot is cell-boxed, the
    // closure gets a trailing capture slot filled from the parent's slot (hops 0).
    let r = resolved("fn outer()\n  let x = 1\n  fn inner()\n    x\n  end\nend");
    // `x` inside `inner` resolves to inner's capture slot 0.
    assert_eq!(refs(&r), vec!["x:L0"]);
    assert_eq!(
        layout(&r),
        vec![
            "Func(*x)[0<-0.0]", // inner: capture slot 0 for x, from the parent's slot 0
            "Func(*x,inner)",   // outer: x is cell-boxed (captured); `inner` is not
            "ModuleTopLevel()",
        ]
    );
}

#[test]
fn param_default_referencing_an_enclosing_local_is_captured() {
    // A param default evaluates at call time in the closure's activation (L§8.2),
    // so a default that names an enclosing fn's local must be CAPTURED into the
    // closure — not resolved against the enclosing frame. `inner`'s default `a = x`
    // captures `x` from `outer` (a trailing capture slot after the param `a`).
    let r = resolved("fn outer()\n  let x = 1\n  fn inner(a = x)\n    a\n  end\nend");
    assert_eq!(refs(&r), vec!["x:L1", "a:L0"]); // default `x` → cap slot 1; body `a` → slot 0
    assert_eq!(
        layout(&r),
        vec![
            "Func(a,*x)[1<-0.0]", // inner: param a (slot 0), captured x (slot 1) from parent slot 0
            "Func(*x,inner)",     // outer: x is cell-boxed (captured); inner is a local
            "ModuleTopLevel()",
        ]
    );
    // A default still cannot see a sibling param (resolved before params are in
    // scope): `y = x` in `fn f(x, y = x)` is the enclosing/module `x`, not param x.
    assert_eq!(
        refs(&resolved("fn f(x, y = x)\n  y\nend")),
        vec!["x:M", "y:L1"]
    );
}

#[test]
fn capture_inside_a_block_uses_static_link_hops() {
    // A closure created INSIDE a block that captures an enclosing-fn local: the
    // source is `hops > 0` up the block's defining chain (the ratified refinement).
    // `helper` captures `x` (from `a`, 1 static-link hop up through the block) and
    // `item` (the block's own local, 0 hops).
    let src = "fn a()\n  let x = 1\n  each(xs) do (item)\n    fn helper()\n      \
               x + item\n    end\n    run(helper)\n  end\nend";
    let r = resolved(src);
    // In helper: `x` → capture slot 0, `item` → capture slot 1.
    assert_eq!(
        refs(&r),
        vec!["each:M", "xs:M", "x:L0", "item:L1", "run:M", "helper:L1"]
    );
    assert_eq!(
        layout(&r),
        vec![
            "Func(*x,*item)[0<-1.0,1<-0.0]", // x from hops 1 (up to `a`); item from hops 0 (block)
            "Block(*item,helper)",           // the block owns the cell-boxed `item`
            "Func(*x)",                      // `a` owns the cell-boxed `x`
            "ModuleTopLevel()",
        ]
    );
}

#[test]
fn deep_capture_threads_through_intervening_closures() {
    // closure-in-block-in-closure-in-block: `c` (deepest) captures `x` (from `a`).
    // `b` never names `x` but gets a pass-through capture slot so `c` can reach it.
    // Both hops are 1 (a block sits between each closure and its capture source).
    let src = "fn a()\n  let x = 1\n  each(xs) do (i)\n    fn b()\n      \
               each(ys) do (j)\n        fn c()\n          x\n        end\n        \
               run(c)\n      end\n    end\n    run(b)\n  end\nend";
    let r = resolved(src);
    assert_eq!(
        layout(&r),
        vec![
            "Func(*x)[0<-1.0]", // c: x from hops 1 up to b's capture slot
            "Block(j,c)",       // block2 (inside b)
            "Func(*x)[0<-1.0]", // b: pass-through x from hops 1 up to a's slot
            "Block(i,b)",       // block1 (inside a)
            "Func(*x)",         // a owns the cell-boxed x
            "ModuleTopLevel()",
        ]
    );
}

#[test]
fn free_names_become_module_name_refs() {
    let r = resolved("show(greeting)");
    assert_eq!(refs(&r), vec!["show:M", "greeting:M"]);
    assert_eq!(name_refs(&r), vec!["show", "greeting"]);
    assert!(globals(&r).is_empty());
}

#[test]
fn try_rescue_binds_the_error_name_as_a_slot() {
    // The caught value binds `e` as a slot in the enclosing frame; the handler's
    // reference to `e` resolves to that slot.
    let r = resolved("to f()\n  try risky() rescue e handle(e) end\nend");
    // `risky`(M), `handle`(M), `e`(L0 — the rescue binding, first slot in f).
    assert_eq!(refs(&r), vec!["risky:M", "handle:M", "e:L0"]);
}

#[test]
fn stmt_spans_cover_top_level_statements() {
    // Every statement gets a span boundary (for breakpoints/stepping).
    let r = resolved("let x = 1\nlet y = 2\nshow(x)");
    assert_eq!(r.module.stmt_spans.len(), 3);
}

#[test]
fn resolving_an_empty_module_is_clean() {
    let r = resolved("");
    assert!(globals(&r).is_empty());
    assert!(refs(&r).is_empty());
    assert_eq!(r.module.callables.len(), 1); // just the module top level
    assert!(matches!(
        r.module.callables[0].kind,
        doodle_core::resolve::BodyKind::ModuleTopLevel
    ));
}

#[test]
fn return_targets_the_home_callable_and_punches_through_blocks() {
    // `return` in a fn body targets the callable; inside a block it punches
    // through to the same home callable (MD §12).
    let r = resolved("to f()\n  return\nend");
    assert_eq!(exits(&r), vec!["return:home"]);
    assert!(diags(&r).is_empty());
    let r = resolved("fn f()\n  each(xs) do (y)\n    return y\n  end\nend");
    assert_eq!(exits(&r), vec!["return:home"]);
    assert!(diags(&r).is_empty());
}

#[test]
fn return_outside_a_callable_is_misplaced() {
    let r = resolved("return");
    assert_eq!(exits(&r), vec!["return:?"]);
    assert_eq!(diags(&r), vec!["misplaced-exit"]);
    // A loop at module level is not a callable — `return` there is still misplaced.
    let r = resolved("while c do return end");
    assert_eq!(diags(&r), vec!["misplaced-exit"]);
}

#[test]
fn break_and_continue_target_the_enclosing_loop() {
    let r = resolved("while c do\n  continue\n  break\nend");
    assert_eq!(exits(&r), vec!["continue:loop", "break:loop"]);
    assert!(diags(&r).is_empty());
}

#[test]
fn break_and_continue_in_a_block_target_the_block() {
    // In a block, `break` exits the block-consuming call, `continue` ends the
    // block invocation (MD §12).
    let r = resolved("each(xs) do (y)\n  continue\n  break\nend");
    assert_eq!(exits(&r), vec!["continue:block", "break:consumer"]);
    assert!(diags(&r).is_empty());
}

#[test]
fn break_outside_a_loop_or_block_is_misplaced() {
    let r = resolved("break");
    assert_eq!(diags(&r), vec!["misplaced-exit"]);
    // A `break` inside a fn cannot escape to an outer loop (the fn is a barrier).
    let r = resolved("while c do\n  to f()\n    break\n  end\nend");
    assert_eq!(exits(&r), vec!["break:?"]);
    assert_eq!(diags(&r), vec!["misplaced-exit"]);
}

#[test]
fn break_targets_the_nearest_loop_not_an_outer_one() {
    // `while a do while b do break end end`: the break targets the INNER loop.
    let r = resolved("while a do\n  while b do\n    break\n  end\nend");
    let m = &r.module;
    // The two `while` nodes; the inner one's span is contained in the outer's.
    let whiles: Vec<NodeId> = (0..m.ast.len())
        .map(|i| NodeId(i as u32))
        .filter(|&id| matches!(m.ast.node(id), Node::While { .. }))
        .collect();
    assert_eq!(whiles.len(), 2);
    let (a, b) = (m.ast.span(whiles[0]), m.ast.span(whiles[1]));
    let inner = if a.start <= b.start && b.end <= a.end {
        whiles[1]
    } else {
        whiles[0]
    };
    let break_id = (0..m.ast.len())
        .map(|i| NodeId(i as u32))
        .find(|&id| matches!(m.ast.node(id), Node::Break(_)))
        .expect("a break");
    assert_eq!(
        m.exit_targets[break_id.0 as usize],
        Some(ExitTarget::ThisLoop(inner))
    );
}

#[test]
fn duplicate_declaration_in_a_scope() {
    assert_eq!(
        diags(&resolved("let x = 1\nlet x = 2")),
        vec!["duplicate-declaration"]
    );
    // Different scopes = shadowing (a warning), not duplicate: an inner `let x`
    // hides the module-level `x`.
    assert_eq!(
        diags(&resolved("let x = 1\nto f()\n  let x = 2\nend")),
        vec!["shadowing"]
    );
    // A `let` colliding with a param in the same fn scope IS a duplicate.
    assert_eq!(
        diags(&resolved("to f(x)\n  let x = 2\nend")),
        vec!["duplicate-declaration"]
    );
}

#[test]
fn shadowing_an_outer_binding_warns() {
    let sh = vec!["shadowing"];
    // A local hiding an enclosing local across construct scopes (`if` body).
    assert_eq!(
        diags(&resolved(
            "to f()\n  let x = 1\n  if c then\n    let x = 2\n    x\n  end\nend"
        )),
        sh
    );
    // A `rescue` name hiding an outer local.
    assert_eq!(
        diags(&resolved(
            "to f()\n  let e = 1\n  try g() rescue e h(e) end\nend"
        )),
        sh
    );
    // A **parameter** hiding a module global (the rubric's flagship case).
    assert_eq!(
        diags(&resolved("let count = 0\nto tally(count)\n  count\nend")),
        sh
    );
    // A **block parameter** hiding a module global.
    assert_eq!(diags(&resolved("let y = 1\neach(xs) do (y)\n  y\nend")), sh);
    // Module globals are whole-scope: a nested local hides a global declared
    // *later*, just as one declared earlier (consistent with assignment).
    assert_eq!(
        diags(&resolved("to f()\n  let x = 1\n  x\nend\nlet x = 9")),
        sh
    );
    // No warning when nothing is hidden (distinct name; or nothing outer).
    assert!(diags(&resolved("let x = 1\nto f()\n  let y = 2\n  y\nend")).is_empty());
    assert!(diags(&resolved("to f()\n  let x = 1\n  x\nend")).is_empty());
    // Sibling params don't shadow each other (same scope, not enclosing).
    assert!(diags(&resolved("to f(a, b)\n  a\nend")).is_empty());
}

#[test]
fn assigning_to_a_mutable_let_is_allowed() {
    assert!(diags(&resolved("let x = 1\nx = 2")).is_empty());
    // A forward module `let` (declared later) is still a binding (checked post-pass).
    assert!(diags(&resolved("x = 2\nlet x = 1")).is_empty());
    // A parameter is a mutable local.
    assert!(diags(&resolved("to f(x)\n  x = 2\nend")).is_empty());
    // Field/index assignment mutates a pointee — always allowed, never rule-2a.
    assert!(diags(&resolved("let a = thing()\na.b = 1\na[0] = 2")).is_empty());
}

#[test]
fn assigning_to_a_const_or_declaration_is_rule_2a() {
    // `const` — module and local.
    assert_eq!(
        diags(&resolved("const c = 1\nc = 2")),
        vec!["const-reassignment"]
    );
    assert_eq!(
        diags(&resolved("to f()\n  const c = 1\n  c = 2\nend")),
        vec!["const-reassignment"]
    );
    // A declaration binding is non-assignable (S-6 rule 2a).
    assert_eq!(
        diags(&resolved("to greet() end\ngreet = 2")),
        vec!["const-reassignment"]
    );
    // Rule 2a still catches a *local* declaration (S-6 ratified 2026-07-17): the
    // static-subset narrowing is about the Void *value* check, not assignability —
    // rule 2a runs in the scope walk where the binding kind is in hand.
    assert_eq!(
        diags(&resolved("to f()\n  to helper() end\n  helper = 5\nend")),
        vec!["const-reassignment"]
    );
    // A dynamic parameter is `with`-rebindable, not `=`-assignable.
    assert_eq!(
        diags(&resolved("parameter p = 1\np = 2")),
        vec!["const-reassignment"]
    );
}

#[test]
fn assigning_to_an_unbound_name_is_undeclared() {
    // Sound and complete without import resolution: a name that isn't a visible
    // `let` can only be undeclared or a read-only import — both errors (S-39).
    assert_eq!(diags(&resolved("x = 5")), vec!["undeclared-assignment"]);
    assert_eq!(
        diags(&resolved("to f()\n  y = 5\nend")),
        vec!["undeclared-assignment"]
    );
    // Reading an unknown name is fine (that's an IDE/load concern, AD5) — only
    // *assignment* to one is a static error.
    assert!(diags(&resolved("show(unknown)")).is_empty());
}

#[test]
fn assigning_to_a_selective_import_names_the_source() {
    // A selective (non-wildcard) import is lexically visible, so assigning to it
    // gets a specific "imported from …" message (imports are read-only, S-39).
    let r = resolved("import turtle.pen_color\npen_color = red");
    assert_eq!(diags(&r), vec!["undeclared-assignment"]);
    let msg = &r.diagnostics[0].message;
    assert!(
        msg.contains("pen_color") && msg.contains("imported from") && msg.contains("turtle"),
        "expected an 'imported from' message, got: {msg}"
    );
    // A wildcard-supplied name isn't nameable until load (M5), so it falls to the
    // generic message — but the verdict is still an error.
    let r2 = resolved("import turtle.*\npen_color = red");
    assert_eq!(diags(&r2), vec!["undeclared-assignment"]);
    assert!(r2.diagnostics[0].message.contains("no `let` named"));
}

#[test]
fn fn_falls_off_end_when_tail_is_value_less() {
    let dup = "function-falls-off-end";
    assert_eq!(diags(&resolved("fn f() end")), vec![dup]); // empty body
    assert_eq!(diags(&resolved("fn f()\n  let x = 1\nend")), vec![dup]); // tail `let`
    assert_eq!(
        diags(&resolved("fn f()\n  while c do g() end\nend")),
        vec![dup]
    ); // `while` yields nothing
    assert_eq!(
        diags(&resolved("fn f()\n  if c then 1 end\nend")),
        vec![dup]
    ); // `if` with no `else`
    assert_eq!(
        diags(&resolved("to p() end\nfn f()\n  p()\nend")),
        vec![dup]
    ); // tail call to a `to` (Void)
    assert_eq!(
        diags(&resolved("fn f()\n  loop do break end\nend")),
        vec![dup]
    ); // a `loop` that can exit
}

#[test]
fn fn_does_not_fall_off_when_tail_produces_or_diverges() {
    assert!(diags(&resolved("fn f()\n  1\nend")).is_empty()); // a value
    assert!(diags(&resolved("fn f()\n  return 1\nend")).is_empty()); // return a value
    // A `raise` tail DIVERGES — it never falls off (the earlier-draft unsoundness).
    assert!(diags(&resolved("fn f()\n  raise err\nend")).is_empty());
    // A `loop` with no bound `break` is an infinite loop → diverges.
    assert!(diags(&resolved("fn f()\n  loop do g() end\nend")).is_empty());
    // Every `if` branch produces.
    assert!(diags(&resolved("fn f()\n  if c then 1 else 2 end\nend")).is_empty());
    // A call whose proc/fn nature isn't lexically known → indeterminate → runtime.
    assert!(diags(&resolved("fn f()\n  unknown()\nend")).is_empty());
    // A same-module `fn` call produces a value.
    assert!(diags(&resolved("fn g() 1 end\nfn f()\n  g()\nend")).is_empty());
    // `to` bodies are never checked (a procedure yields no value by design).
    assert!(diags(&resolved("to p()\n  let x = 1\nend")).is_empty());
    // try: a produces-or-diverges mix is fine (the ruling's example).
    assert!(diags(&resolved("fn f()\n  try h() rescue e raise end\nend")).is_empty());
    // `return expr` PRODUCES (the fn doesn't fall off), so S-5 never flags it. But
    // `return p()` for a `to` p uses Void as the return value — an S-6 consuming-
    // site error at the operand (`procedure-in-expression`), NOT falls-off-end.
    assert_eq!(
        diags(&resolved("to p() end\nfn f()\n  return p()\nend")),
        vec!["procedure-in-expression"]
    );
}

#[test]
fn procedure_call_where_a_value_is_required_is_a_consuming_site_error() {
    // A same-module `to` yields Void (S-6); using it as a value is a static error
    // at each consuming site (`procedure-in-expression`), regardless of the site.
    let pie = "procedure-in-expression";
    let p = "to p() end\n"; // a procedure in scope
    for consume in [
        "let x = p()",          // a `let` initializer
        "const x = p()",        // a `const` initializer
        "let x = 1\nx = p()",   // an assignment RHS
        "show(p())",            // a call argument
        "let x = p() + 1",      // an operator operand
        "let x = p().f",        // a `.field` base
        "let x = p()[0]",       // an `[i]` base
        "let s = \"{p()}\"",    // an interpolation
        "if p() then g() end",  // an `if` condition
        "while p() do g() end", // a `while` condition
        "let xs = [p()]",       // a list element
        "let d = {a: p()}",     // a dict value
        "p()()",                // the thing being called
    ] {
        let src = format!("{p}{consume}");
        assert_eq!(diags(&resolved(&src)), vec![pie], "for source: {src:?}");
    }
    // A parameter default and a module `parameter` default are consuming sites too.
    assert_eq!(
        diags(&resolved("to p() end\nto f(x = p())\n  x\nend")),
        vec![pie]
    );
    assert_eq!(diags(&resolved("to p() end\nparameter c = p()")), vec![pie]);
    // `raise`/`break` operands consume a value (Void is not a value, so this errors
    // regardless of how S-10 lands).
    assert_eq!(diags(&resolved("to p() end\nraise p()")), vec![pie]);
    assert_eq!(
        diags(&resolved("to p() end\nwhile c do\n  break p()\nend")),
        vec![pie]
    );
    // The producer-site blame names the procedure and points at the call.
    let r = resolved("to p() end\nlet x = p()");
    assert!(
        r.diagnostics[0].message.contains("`p` is a procedure"),
        "expected the proc named, got: {}",
        r.diagnostics[0].message
    );
}

#[test]
fn a_procedure_call_as_its_own_statement_is_fine() {
    // The one non-consuming position (§7.2): a bare expression statement discards
    // its value, so calling a `to` there is exactly how procedures are used.
    assert!(diags(&resolved("to p() end\np()")).is_empty());
    // Forward reference: `p` is declared after the use — the post-pass sees it.
    assert!(diags(&resolved("p()\nto p() end")).is_empty());
}

#[test]
fn void_propagates_through_expression_position_if_and_try() {
    // Void flows out of a value-producing `if`/`try` from the branch that produces
    // it, to the outer consumer (blaming the branch's call).
    let pie = vec!["procedure-in-expression"];
    let p = "to p() end\n";
    assert_eq!(
        diags(&resolved(&format!("{p}let x = if c then p() else 2 end"))),
        pie
    );
    assert_eq!(
        diags(&resolved(&format!("{p}let x = if c then 1 else p() end"))),
        pie
    );
    assert_eq!(
        diags(&resolved(&format!("{p}let x = try p() rescue e 2 end"))),
        pie
    );
}

#[test]
fn void_not_statically_determinable_is_deferred_to_runtime() {
    // The static subset is a *module-level* `to` callee (S-6, normative). These all
    // defer to the M2a runtime check, so the resolver stays silent.
    assert!(diags(&resolved("let x = unknown()")).is_empty()); // an unknown callee
    assert!(diags(&resolved("fn g() 1 end\nlet x = g()")).is_empty()); // an `fn` produces
    assert!(diags(&resolved("to f(g)\n  let x = g()\n  x\nend")).is_empty()); // a local param
    // A *locally*-declared `to` is indeterminate (declaration-kind is only known
    // module-level, ratified 2026-07-17) → runtime, not a static error here.
    assert!(diags(&resolved("fn f()\n  to p() end\n  let x = p()\n  x\nend")).is_empty());
    // A field/index *mutation* target is not a consuming read of a value.
    assert!(diags(&resolved("to p() end\nlet a = thing()\na.b = 1")).is_empty());
}

#[test]
fn if_or_try_as_a_value_requires_every_branch_to_produce() {
    // L§6.8/§6.9: an `if`/`try` used as a value must produce on every branch.
    // A missing `else` in value position (L§6.8).
    assert_eq!(
        diags(&resolved("let x = if c then 1 end")),
        vec!["if-expression-missing-else"]
    );
    // A present branch whose tail produces no value (ends in a `let`).
    assert_eq!(
        diags(&resolved("let x = if c then 1 else\n  let y = 2\nend")),
        vec!["non-producing-branch"]
    );
    // A `while` tail branch — value-less (like the fn-falls-off `while` case).
    assert_eq!(
        diags(&resolved(
            "let x = if c then 1 else\n  while d do g() end\nend"
        )),
        vec!["non-producing-branch"]
    );
    // A `try` body that produces no value (L§6.9) — no `else` concept for `try`.
    assert_eq!(
        diags(&resolved("let x = try\n  let y = 1\nrescue e\n  2\nend")),
        vec!["non-producing-branch"]
    );
    // An *empty* branch/body produces no value (the empty block is value-less, per
    // the S-5 lattice) — for `if` (either branch) and `try` (either body).
    let npb = vec!["non-producing-branch"];
    assert_eq!(diags(&resolved("let x = if c then 1 else\nend")), npb);
    assert_eq!(diags(&resolved("let x = if c then\nelse 2 end")), npb);
    assert_eq!(diags(&resolved("let x = try\nrescue e\n  2\nend")), npb);
    assert_eq!(diags(&resolved("let x = try\n  1\nrescue e\nend")), npb);
    // A *diverging* branch is fine (produces-or-diverges mixes, per the S-5 lattice):
    // `raise` in one branch, a value in the other.
    assert!(diags(&resolved("let x = if c then raise oops else 2 end")).is_empty());
    // A non-local exit (bare `return`/`break`) also diverges past the consumer, so
    // the `if` is not Void here — unlike an *fn tail*, where a bare `return` is the
    // fn's own value-less concern (S-5). See the voidcheck module note.
    assert!(
        diags(&resolved(
            "to f()\n  let x = if c then 1 else return end\n  show(x)\nend"
        ))
        .is_empty()
    );
    // Both branches produce → OK; a bare statement `if` needs no `else`.
    assert!(diags(&resolved("let x = if c then 1 else 2 end")).is_empty());
    assert!(diags(&resolved("if c then g() end")).is_empty());
}

#[test]
fn resolver_diagnostics_are_source_ordered() {
    // The deferred (module-name) assign check runs in a post-pass, but the front
    // end guarantees source-ordered diagnostics — so the `y` (line 1) error must
    // precede the inline `c` (line 4) error.
    let r = resolved("y = 5\nto f()\n  const c = 1\n  c = 2\nend");
    assert_eq!(
        diags(&r),
        vec!["undeclared-assignment", "const-reassignment"]
    );
}

#[test]
fn forward_reference_to_a_local_let_is_undeclared() {
    // Locals are visible declaration-point-onward (L§5.1), so assigning a local
    // `let` BEFORE its declaration is undeclared — unlike a module `let`, which is
    // whole-scope (mutual recursion). This asymmetry is deliberate.
    let r = resolved("to f()\n  x = 2\n  let x = 1\nend");
    assert_eq!(diags(&r), vec!["undeclared-assignment"]);
    // The module-level equivalent IS allowed (forward module `let`).
    assert!(diags(&resolved("x = 2\nlet x = 1")).is_empty());
}

// --- Tail-call marking (M1.11, L§8.7 / machine-design §11) ------------------

#[test]
fn last_statement_call_is_tail() {
    // A body's last statement (a `to` or `fn`) is a fall-through tail position:
    // after the call returns, the callable returns with no pending work.
    assert_eq!(tails(&resolved("to p()\n  q()\nend")), vec!["q:T"]);
    assert_eq!(tails(&resolved("fn f()\n  g()\nend")), vec!["g:T"]);
    // Module top level is not a callable — a call there is never tail.
    assert_eq!(tails(&resolved("g()")), vec!["g:-"]);
}

#[test]
fn calls_with_pending_work_are_not_tail() {
    // An operator operand, a call argument, and a `let`/assignment RHS all leave
    // work pending after the inner call returns.
    assert_eq!(tails(&resolved("fn f()\n  g() + 1\nend")), vec!["g:-"]);
    // Node order is post-order (a `Call` node follows its args), so the inner
    // `h()` precedes the outer `g(…)`.
    assert_eq!(
        tails(&resolved("fn f()\n  g(h())\nend")),
        vec!["h:-", "g:T"]
    );
    assert_eq!(
        tails(&resolved("to p()\n  let x = g()\n  q()\nend")),
        vec!["g:-", "q:T"]
    );
}

#[test]
fn tail_if_branches_are_tail_but_the_condition_is_not() {
    // The selected branch of a tail `if` is tail (L§8.7); the condition is not.
    assert_eq!(
        tails(&resolved("to p()\n  if cond() then a() else b() end\nend")),
        vec!["cond:-", "a:T", "b:T"]
    );
    // A non-tail `if` (a statement with code after) propagates non-tail to its
    // branches — but a `return` inside stays tail (next test).
    assert_eq!(
        tails(&resolved("to p()\n  if c then a() end\n  q()\nend")),
        vec!["a:-", "q:T"]
    );
}

#[test]
fn return_operand_is_a_tail_position_wherever_it_sits() {
    // `return expr` delivers expr as the callable's result — expr is tail even
    // where later code (which the return abandons) follows.
    assert_eq!(tails(&resolved("fn f()\n  return g()\nend")), vec!["g:T"]);
    // `return g() + 1` leaves the `+ 1` pending → not tail.
    assert_eq!(
        tails(&resolved("fn f()\n  return g() + 1\nend")),
        vec!["g:-"]
    );
    // A `return` in a non-tail branch, with a statement after it, is still tail.
    assert_eq!(
        tails(&resolved("to p()\n  if c then return g() end\n  h()\nend")),
        vec!["g:T", "h:T"]
    );
}

#[test]
fn a_call_passing_a_block_argument_is_not_tail() {
    // S-45: a `do … end` block references the caller's frame, so the frame cannot
    // be reused — the block-consuming call is never tail. The block's OWN last
    // statement is tail with respect to the block's invocation.
    assert_eq!(
        tails(&resolved("to p()\n  each(xs) do (x)\n    q()\n  end\nend")),
        vec!["q:T", "each:-"] // post-order: the block-body `q()` precedes `each(…)`
    );
}

#[test]
fn calls_inside_with_and_try_bodies_are_not_tail() {
    // A `with` body (a dynamic binding to restore) and a `try` body/handler (a
    // handler to unwind) are barriers — a call inside is never tail (L§8.7).
    assert_eq!(
        tails(&resolved(
            "to p()\n  with pen = red do\n    q()\n  end\nend"
        )),
        vec!["q:-"]
    );
    assert_eq!(
        tails(&resolved(
            "to p()\n  try risky() rescue e handle(e) end\nend"
        )),
        vec!["risky:-", "handle:-"]
    );
}

#[test]
fn a_return_through_a_with_or_try_barrier_is_not_tail() {
    // The correctness-critical direction: a `return g()` inside a `with`/`try`
    // body must NOT be marked — restoring the dynamic binding / unwinding the
    // handler is pending work after `g()` returns, so the frame can't be reused.
    // (Distinct from the fall-through cases above; a `return` is tail wherever it
    // sits *unless* a barrier intervenes.)
    assert_eq!(
        tails(&resolved(
            "fn f()\n  with pen = red do\n    return g()\n  end\nend"
        )),
        vec!["g:-"]
    );
    assert_eq!(
        tails(&resolved(
            "fn f()\n  try\n    return g()\n  rescue e\n    0\n  end\nend"
        )),
        vec!["g:-"]
    );
}

#[test]
fn a_call_in_a_loop_body_is_not_tail_but_a_return_there_is() {
    // A `loop`/`while` body repeats, so its calls are not fall-through tail; but a
    // `return` inside a loop is a same-frame exit → its operand is a tail call.
    assert_eq!(
        tails(&resolved("to p()\n  loop do\n    q()\n  end\nend")),
        vec!["q:-"]
    );
    assert_eq!(
        tails(&resolved("fn f()\n  loop do\n    return g()\n  end\nend")),
        vec!["g:T"]
    );
}

#[test]
fn a_return_inside_a_block_is_a_non_local_exit_not_a_tail_call() {
    // A `return` inside a `do … end` block targets an outer callable, not the
    // block frame — so it is a non-local exit, not this block's tail. The block's
    // fall-through last statement still IS a tail call w.r.t. its invocation.
    assert_eq!(
        tails(&resolved(
            "fn f()\n  each(xs) do (x)\n    return g()\n  end\nend"
        )),
        vec!["g:-", "each:-"]
    );
    assert_eq!(
        tails(&resolved("fn f()\n  each(xs) do (x)\n    g()\n  end\nend")),
        vec!["g:T", "each:-"]
    );
}

#[test]
fn nested_callables_are_marked_independently() {
    // Each callable body is marked on its own: the inner fn's tail call is marked
    // regardless of the outer's non-tail position for the `fn` expression itself.
    assert_eq!(
        tails(&resolved(
            "fn outer()\n  let h = fn ()\n    g()\n  end\n  h()\nend"
        )),
        vec!["g:T", "h:T"]
    );
}
