//! Resolver tests (M1.10a): name resolution rendered compactly so slot
//! assignment, static links, and free/module classification are visible.
//!
//! Reference notation: `L{slot}` = local slot, `B{hops}.{slot}` = block static
//! link, `M` = module name (free), `?` = unresolved (a deferred cross-`fn`
//! capture, M1.10c).

use doodle_core::ast::{Node, NodeId};
use doodle_core::parse::parse_program;
use doodle_core::resolve::{Resolution, Resolved, resolve};
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
