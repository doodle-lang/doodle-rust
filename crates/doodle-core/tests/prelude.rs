//! Prelude-as-import tests (M5.8, L§11.2, S-60): the built-in type values, `Error`, the
//! well-known protocols, and the host intrinsics live in one shared prelude module that every
//! source module implicitly wildcard-imports. Precedence is S-13 with no special tier: own
//! declarations → selective imports → wildcards (the prelude among them); a user global
//! shadows the prelude, and a user `import m.*` that re-exports a prelude name is ambiguous at
//! use (distinct bindings), disambiguated by an explicit `import prelude.name` / `import m.name`.

use doodle_core::diag::Severity;
use doodle_core::drive::{Directive, ImportResolution, Outcome, resolve_import, run};
use doodle_core::machine::{Instance, Registry, print_intrinsic, read_line_intrinsic};
use doodle_core::parse::parse_program;
use doodle_core::resolve::resolve as resolve_module;
use doodle_core::source::normalize;
use doodle_core::span::ModuleId;

/// A `shapes` module exporting only `draw` — no name collision with the prelude.
const SHAPES: &str = "\
to draw()
end
";

/// An `overlap` module that defines its own `print`, colliding with the prelude's.
const OVERLAP: &str = "\
to print(x)
end
to draw()
end
";

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
        resolved.diagnostics.is_empty(),
        "resolve diagnostic(s): {:?}",
        resolved.diagnostics
    );
    let mut registry = Registry::new();
    registry.register(print_intrinsic()).unwrap();
    registry.register(read_line_intrinsic()).unwrap();
    Instance::load_with_intrinsics(resolved.module, registry)
}

/// Drives `main`, resolving `import shapes` to [`SHAPES`], to a terminal outcome.
fn run_bundled(main: &str) -> (Instance, Outcome) {
    let mut inst = instance(main);
    let mut outcome = run(&mut inst, Directive::RunToCompletion);
    while let Outcome::SuspendedImport(req) = &outcome {
        let res = if req.path == ["shapes"] {
            ImportResolution::Source {
                text: SHAPES.to_string(),
                canonical_id: "shapes".to_string(),
            }
        } else if req.path == ["overlap"] {
            ImportResolution::Source {
                text: OVERLAP.to_string(),
                canonical_id: "overlap".to_string(),
            }
        } else {
            ImportResolution::NotFound
        };
        outcome = resolve_import(&mut inst, res);
    }
    (inst, outcome)
}

fn output(main: &str) -> String {
    let (inst, outcome) = run_bundled(main);
    assert!(matches!(outcome, Outcome::Completed(_)), "{outcome:?}");
    String::from_utf8(inst.output().to_vec()).expect("utf-8")
}

fn raised(main: &str) -> (String, String) {
    let (inst, outcome) = run_bundled(main);
    let Outcome::Raised(value, _) = outcome else {
        panic!("expected a raise, got {outcome:?}");
    };
    let (kind, message) = inst.describe_raised(value);
    (kind.to_string(), message.to_string())
}

#[test]
fn prelude_names_resolve_through_the_implicit_wildcard() {
    // `print` (a host intrinsic), the built-in type values, and `Error` all come from the
    // prelude module now, not per-module seeding.
    let out = output("print(\"{5 is Int} {[1] is List} {\"x\" is String}\")\n");
    assert_eq!(out, "true true true\n");
}

#[test]
fn a_user_global_shadows_the_prelude() {
    // `let print = 5` is an own declaration — it wins over the prelude wildcard, so calling
    // `print` calls the Int and raises not-callable (the D-M5-6 kid trap, warning deferred).
    let (kind, _message) = raised("let print = 5\nprint(\"hi\")\n");
    assert_eq!(kind, "not-callable");
}

#[test]
fn a_wildcard_reexporting_a_prelude_name_is_ambiguous() {
    // `import overlap.*` supplies its own `print`; the prelude also supplies one. Two distinct
    // bindings → ambiguous at use (S-13), naming both sources including the prelude.
    let (kind, message) = raised("import overlap.*\nprint(\"hi\")\n");
    assert_eq!(kind, "ambiguous-import");
    assert!(message.contains("prelude"), "{message}");
    assert!(message.contains("overlap"), "{message}");
}

#[test]
fn an_explicit_prelude_import_disambiguates() {
    // Selecting the prelude's `print` explicitly (own-namespace binding) beats both wildcards,
    // so the call resolves — the S-13 fix, with the prelude as an importable module.
    let out = output("import overlap.*\nimport prelude.print\nprint(\"chose prelude\")\n");
    assert_eq!(out, "chose prelude\n");
}

#[test]
fn a_noncolliding_wildcard_coexists_with_the_prelude() {
    // `draw` (from shapes) and `print` (from the prelude) resolve side by side — a user
    // wildcard that shares no name with the prelude is not ambiguous.
    let out = output("import shapes.*\nprint(\"ok\")\ndraw()\n");
    assert_eq!(out, "ok\n");
}
