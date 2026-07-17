//! Resolver tests (M1.10a): name resolution rendered compactly so slot
//! assignment, static links, and free/module classification are visible.
//!
//! Reference notation: `L{slot}` = local slot, `B{hops}.{slot}` = block static
//! link, `M` = module name (free), `?` = unresolved (a deferred cross-`fn`
//! capture, M1.10c).

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

#[test]
fn module_level_lets_are_globals_referenced_by_module_name() {
    // A module-level `let`/`const`/`to`/`fn` binds a module cell (a global); a
    // reference to it is a module name (resolved to the cell at load, AD5).
    let r = resolved("let x = 1\nlet y = x");
    assert_eq!(globals(&r), vec!["x:Let", "y:Let"]);
    assert_eq!(refs(&r), vec!["x:M"]); // the `x` in `let y = x`
    assert_eq!(name_refs(&r), vec!["x"]);
    assert!(r.deferred_captures.is_empty());
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
    assert!(r.deferred_captures.is_empty()); // a block is not a capture
}

#[test]
fn cross_fn_reference_is_deferred_as_a_capture() {
    // A nested `fn` referencing an enclosing fn's local crosses an `fn` boundary
    // — a closure capture, deferred to M1.10c (unresolved for now).
    let r = resolved("fn outer()\n  let x = 1\n  fn inner()\n    x\n  end\nend");
    // The `x` reference inside `inner` is unresolved (deferred).
    assert_eq!(refs(&r), vec!["x:?"]);
    assert_eq!(r.deferred_captures.len(), 1);
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
    // Different scopes = shadowing, not duplicate (the warning is M1.11).
    assert!(diags(&resolved("let x = 1\nto f()\n  let x = 2\nend")).is_empty());
    // A `let` colliding with a param in the same fn scope IS a duplicate.
    assert_eq!(
        diags(&resolved("to f(x)\n  let x = 2\nend")),
        vec!["duplicate-declaration"]
    );
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
    // `return expr` PRODUCES (the fn doesn't fall off) — even `return p()` for a
    // `to` p: that's an S-6 return-operand error, not falls-off-end. Not flagged
    // by S-5 (S-6's consuming-site check owns it).
    assert!(diags(&resolved("to p() end\nfn f()\n  return p()\nend")).is_empty());
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
