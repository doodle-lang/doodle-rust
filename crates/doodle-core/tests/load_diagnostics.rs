//! The instance **load-diagnostics record** (engine spec E§3.2/§8, S-63) and the
//! **D-M5-6 prelude-shadowing warning** (L§5.1) that feeds it. A successful load may still
//! produce warnings — a module-level declaration that hides a prelude name (a built-in type,
//! `Error`, a well-known protocol, or a host intrinsic like `print`) shadows it — and every
//! imported module's front-end diagnostics (errors included) accumulate in one instance-scoped,
//! deterministically ordered, pull-read record. These drive through the public API: `load*`,
//! `Instance::load_diagnostics`, and the `bundle_run` host loop for imports.

use doodle_core::diag::{DiagnosticCode, Severity};
use doodle_core::drive::{Directive, ImportResolution, Outcome, resolve_import, run};
use doodle_core::machine::{Instance, Registry, print_intrinsic, read_line_intrinsic};
use doodle_core::parse::parse_program;
use doodle_core::resolve::resolve as resolve_module;
use doodle_core::source::normalize;
use doodle_core::span::ModuleId;

/// Loads `main` as the entry module with the `print` (0) and `read_line` (1) intrinsics
/// (flat prelude names, not native modules), asserting only that it has **no static
/// errors** — a warning (prelude shadowing) is exactly what these tests exercise, and it
/// surfaces in the load-diagnostics record at load, not in the resolver's output.
fn instance(main: &str) -> Instance {
    let nfc = normalize(main);
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
        resolved
            .diagnostics
            .iter()
            .all(|d| d.severity != Severity::Error),
        "resolve error(s): {:?}",
        resolved.diagnostics
    );
    let mut registry = Registry::new();
    registry.register(print_intrinsic()).unwrap();
    registry.register(read_line_intrinsic()).unwrap();
    Instance::load_with_intrinsics(resolved.module, registry)
}

/// Drives `inst` to a terminal outcome as a bundling host, resolving each import against
/// `modules` (dotted path → source, the path doubling as the canonical id) or `NotFound`.
fn bundle_run(inst: &mut Instance, modules: &[(&str, &str)]) -> Outcome {
    let mut outcome = run(inst, Directive::RunToCompletion);
    loop {
        match &outcome {
            Outcome::SuspendedImport(req) => {
                let path = req.path.join(".");
                outcome = match modules.iter().find(|(p, _)| *p == path) {
                    Some((_, src)) => resolve_import(
                        inst,
                        ImportResolution::Source {
                            text: (*src).to_string(),
                            canonical_id: path,
                        },
                    ),
                    None => resolve_import(inst, ImportResolution::NotFound),
                };
            }
            _ => return outcome,
        }
    }
}

#[test]
fn a_main_module_declaration_hiding_a_prelude_name_warns_at_load() {
    // `print` is a prelude intrinsic; declaring it as a module global hides the built-in.
    let inst = instance("let print = 5\n");
    let diags = inst.load_diagnostics(0);
    assert_eq!(diags.len(), 1, "one shadowing warning: {diags:?}");
    let d = &diags[0];
    assert_eq!(d.code, DiagnosticCode::Shadowing);
    assert_eq!(d.severity, Severity::Warning);
    assert_eq!(d.module, Some(ModuleId(0)));
    assert!(d.span.is_some(), "the warning points at the decl span");
    assert!(
        d.message.contains("print") && d.message.contains("prelude"),
        "message names the hidden built-in: {}",
        d.message
    );
}

#[test]
fn a_non_colliding_declaration_is_silent() {
    let inst = instance("let score = 5\nto greet() print(\"hi\") end\n");
    assert!(
        inst.load_diagnostics(0).is_empty(),
        "no prelude name is shadowed: {:?}",
        inst.load_diagnostics(0)
    );
}

#[test]
fn a_type_name_and_the_error_type_are_prelude_names_too() {
    // `Int` (a built-in type value) and `Error` (the built-in error type) are prelude
    // exports, so shadowing either warns — the record is ordered by span (nondecreasing).
    let inst = instance("let Int = 1\nlet Error = 2\n");
    let diags = inst.load_diagnostics(0);
    assert_eq!(diags.len(), 2, "two shadowing warnings: {diags:?}");
    assert!(diags[0].message.contains("Int"));
    assert!(diags[1].message.contains("Error"));
    let (s0, s1) = (diags[0].span.unwrap(), diags[1].span.unwrap());
    assert!(s0.start < s1.start, "ordered by span start");
}

#[test]
fn an_imported_modules_warning_appears_after_its_load_executes() {
    let mut inst = instance("import helper\n");
    // Before the drive, the importer has run nothing: the record is empty (the entry
    // module `import helper` shadows no prelude name).
    assert!(inst.load_diagnostics(0).is_empty());
    let outcome = bundle_run(&mut inst, &[("helper", "let print = 5\n")]);
    assert!(
        matches!(outcome, Outcome::Completed(_)),
        "clean import completes: {outcome:?}"
    );
    // helper is the first imported module (main = 0, prelude = 1) → ModuleId(2).
    let diags = inst.load_diagnostics(0);
    assert_eq!(diags.len(), 1, "helper's shadowing warning: {diags:?}");
    assert_eq!(diags[0].code, DiagnosticCode::Shadowing);
    assert_eq!(diags[0].module, Some(ModuleId(2)));
    assert!(diags[0].message.contains("print"));
}

#[test]
fn a_failed_imports_diagnostics_are_present_in_the_record() {
    let mut inst = instance("import broken\n");
    // A syntactically broken imported module: the load fails (module-load-error raised),
    // but its diagnostics are still in the display record (errors included, S-63).
    let outcome = bundle_run(&mut inst, &[("broken", "let = 5\n")]);
    assert!(
        matches!(outcome, Outcome::Raised(_, _)),
        "a broken import raises module-load-error: {outcome:?}"
    );
    let diags = inst.load_diagnostics(0);
    assert!(
        diags
            .iter()
            .any(|d| d.severity == Severity::Error && d.module == Some(ModuleId(2))),
        "the broken import's error is in the record: {diags:?}"
    );
}

#[test]
fn the_since_cursor_returns_only_the_tail() {
    let inst = instance("let Int = 1\nlet print = 2\n");
    assert_eq!(inst.load_diagnostics(0).len(), 2);
    assert_eq!(
        inst.load_diagnostics(1).len(),
        1,
        "the cursor drops the prefix"
    );
    // An out-of-range cursor is clamped, not a panic.
    assert!(inst.load_diagnostics(99).is_empty());
}

#[test]
fn the_record_is_replay_stable_across_two_runs() {
    let src = "let print = 1\nlet Int = 2\n";
    let modules: &[(&str, &str)] = &[("m", "let Error = 1\nlet read_line = 2\n")];
    let mut a = instance("import m\n");
    let _ = bundle_run(&mut a, modules);
    let mut b = instance("import m\n");
    let _ = bundle_run(&mut b, modules);
    assert_eq!(
        a.load_diagnostics(0),
        b.load_diagnostics(0),
        "the load-diagnostics record is a pure function of sources + prelude exports"
    );
    // And a same-shape entry-module record is identical across two constructions.
    let c = instance(src);
    let d = instance(src);
    assert_eq!(c.load_diagnostics(0), d.load_diagnostics(0));
}
