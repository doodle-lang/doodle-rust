//! Cross-module `with` — the M5 acceptance headliner (exit #1, L§5.5/§11.2, S-39). A Doodle
//! **turtle wrapper** declares `parameter pen_color` over a **native** turtle primitive; a user
//! module imports the wrapper and `with pen_color = …` changes the color the wrapper draws with
//! *inside* the imported `forward` — the **cell-aliasing** proof (the user's `with` rebinds the
//! very cell the wrapper reads, S-39 live alias). Also the ratified `with`-target rules: an
//! imported parameter is `with`-bindable through a **selective or wildcard** import; two
//! wildcards supplying it is `ambiguous-import`; a free target with nothing to supply it is a
//! **static** `with-target-not-parameter`, with a wildcard in scope but no supplier a **runtime**
//! miss, and an imported **non-parameter** a runtime `with-target-not-parameter`.

use doodle_core::diag::Severity;
use doodle_core::drive::{Directive, ImportResolution, Limits, Outcome, resolve_import, run};
use doodle_core::machine::{
    Instance, NativeModule, Registry, print_intrinsic, read_line_intrinsic,
};
use doodle_core::parse::parse_program;
use doodle_core::resolve::resolve as resolve_module;
use doodle_core::source::normalize;
use doodle_core::span::ModuleId;

/// The Doodle turtle **wrapper** (`turtle`): a dynamic `parameter pen_color` over the native
/// primitive `pen`, threaded into the draw inside `forward`. Also a `const SPEED` (a non-
/// parameter export, for the `with`-a-constant case).
const TURTLE: &str = "\
import turtle_native.pen
parameter pen_color = \"black\"
const SPEED = 5
to forward()
    pen(pen_color)
end
exports forward, pen_color, SPEED
";

/// A second wrapper (`other`) that also declares and exports `pen_color` — a *distinct* cell, so
/// wildcard-importing both makes a bare `with pen_color` ambiguous.
const OTHER: &str = "\
parameter pen_color = \"blue\"
exports pen_color
";

/// The native turtle module: a foreign `pen(color)` that draws (here, emits the color, via the
/// demo `print`). Registered before the first load (S-32).
fn turtle_native() -> NativeModule {
    NativeModule::new("turtle_native").function("pen", print_intrinsic())
}

/// Loads `main` with `print`/`read_line` and the native turtle module, resolving `import turtle`
/// / `import other` to their sources, and drives to a terminal outcome.
fn run_bundled(main: &str) -> (Instance, Outcome) {
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
    registry.register_module(turtle_native()).unwrap();
    let mut inst = Instance::load(resolved.module, Limits::default(), registry, "main");
    let mut outcome = run(&mut inst, Directive::RunToCompletion);
    while let Outcome::SuspendedImport(req) = &outcome {
        let res = if req.path == ["turtle"] {
            ImportResolution::Source {
                text: TURTLE.to_string(),
                canonical_id: "turtle".to_string(),
            }
        } else if req.path == ["other"] {
            ImportResolution::Source {
                text: OTHER.to_string(),
                canonical_id: "other".to_string(),
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

/// The `(slug, message)` static diagnostics of resolving `main` alone (imports are load-time, so
/// a resolve-stage error surfaces without the wrapper).
fn resolve_diags(main: &str) -> Vec<(String, String)> {
    let nfc = normalize(main);
    let parsed = parse_program(nfc.as_ref(), ModuleId(0));
    let resolved = resolve_module(parsed.ast, parsed.root, ModuleId(0));
    resolved
        .diagnostics
        .iter()
        .map(|d| (d.code.slug().to_string(), d.message.clone()))
        .collect()
}

#[test]
fn a_selective_import_with_binds_across_the_module_boundary() {
    // The acceptance: `with pen_color = "red"` in user code changes the color `forward` draws
    // with inside the imported wrapper, and the binding is restored after the block.
    let out = output(
        "import turtle.forward\n\
         import turtle.pen_color\n\
         forward()\n\
         with pen_color = \"red\" do\n\
         \x20   forward()\n\
         end\n\
         forward()\n",
    );
    assert_eq!(out, "black\nred\nblack\n");
}

#[test]
fn a_wildcard_import_with_binds_across_the_module_boundary() {
    // The idiomatic form (L§11.2): `import turtle.*` brings `forward` + `pen_color`, and the
    // wildcard-supplied `pen_color` is `with`-bindable just the same.
    let out = output(
        "import turtle.*\n\
         with pen_color = \"green\" do\n\
         \x20   forward()\n\
         end\n",
    );
    assert_eq!(out, "green\n");
}

#[test]
fn a_with_on_a_two_wildcard_parameter_is_ambiguous() {
    // `turtle` and `other` each export a distinct `pen_color`; a bare `with pen_color` is a use,
    // so two distinct wildcard bindings are `ambiguous-import` (naming both sources).
    let (kind, message) = raised(
        "import turtle.*\n\
         import other.*\n\
         with pen_color = \"red\" do\n\
         \x20   forward()\n\
         end\n",
    );
    assert_eq!(kind, "ambiguous-import");
    assert!(
        message.contains("turtle") && message.contains("other"),
        "{message}"
    );
}

#[test]
fn a_typo_with_target_and_no_wildcard_is_a_static_error() {
    // Nothing could supply `pne_color` (no wildcard, not a selective import) — the typo is
    // caught statically.
    let diags = resolve_diags(
        "import turtle.forward\n\
         with pne_color = \"red\" do\n\
         \x20   forward()\n\
         end\n",
    );
    assert!(
        diags
            .iter()
            .any(|(slug, _)| slug == "with-target-not-parameter"),
        "{diags:?}"
    );
}

#[test]
fn a_typo_with_target_under_a_wildcard_is_a_runtime_miss() {
    // A wildcard is in scope, so the resolver defers; at runtime nothing supplies `pne_color`.
    let (kind, _message) = raised(
        "import turtle.*\n\
         with pne_color = \"red\" do\n\
         \x20   forward()\n\
         end\n",
    );
    assert_eq!(kind, "name-not-defined");
}

#[test]
fn a_with_on_an_imported_constant_is_a_runtime_error() {
    // `SPEED` is an imported `const`, not a parameter — its kind is invisible to the resolver,
    // so the check falls to runtime: `with-target-not-parameter`.
    let (kind, message) = raised(
        "import turtle.forward\n\
         import turtle.SPEED\n\
         with SPEED = 10 do\n\
         \x20   forward()\n\
         end\n",
    );
    assert_eq!(kind, "with-target-not-parameter");
    assert!(
        message.contains("SPEED") && message.contains("parameter"),
        "{message}"
    );
}
