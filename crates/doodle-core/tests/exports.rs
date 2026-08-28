//! `exports` enforcement tests (M5.3a, L§11.1): a module's public surface. A member not in
//! a module's `exports` list is private — invisible to other modules through `m.member`,
//! `import m.member`, and `import m.*`. A private access raises `not-exported`; a truly-absent
//! one raises `no-such-member` (the module container's access-miss kind, never a record's
//! `no-such-field`); an `exports` naming an undeclared name is the static `undeclared-export`.

use doodle_core::diag::Severity;
use doodle_core::drive::{Directive, ImportResolution, Outcome, resolve_import, run};
use doodle_core::machine::{Instance, Registry, print_intrinsic, read_line_intrinsic};
use doodle_core::parse::parse_program;
use doodle_core::resolve::resolve as resolve_module;
use doodle_core::source::normalize;
use doodle_core::span::ModuleId;

/// A `lib` module exporting only `hello` (a `to`); `secret` is declared but private.
const LIB: &str = "\
to hello()
    print(\"hi\")
end
to secret()
    print(\"shh\")
end
exports hello
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
        "{:?}",
        resolved.diagnostics
    );
    let mut registry = Registry::new();
    registry.register(print_intrinsic()).unwrap();
    registry.register(read_line_intrinsic()).unwrap();
    Instance::load_with_intrinsics(resolved.module, registry)
}

/// Drives `main` as a bundling host that resolves `import lib` to [`LIB`], to a terminal
/// outcome.
fn run_bundled(main: &str) -> (Instance, Outcome) {
    let mut inst = instance(main);
    let mut outcome = run(&mut inst, Directive::RunToCompletion);
    while let Outcome::SuspendedImport(req) = &outcome {
        let res = if req.path == ["lib"] {
            ImportResolution::Source {
                text: LIB.to_string(),
                canonical_id: "lib".to_string(),
            }
        } else {
            ImportResolution::NotFound
        };
        outcome = resolve_import(&mut inst, res);
    }
    (inst, outcome)
}

/// The `(slug, message)` of a bundled run that raises.
fn raised(main: &str) -> (String, String) {
    let (inst, outcome) = run_bundled(main);
    let Outcome::Raised(value, _) = outcome else {
        panic!("expected a raise, got {outcome:?}");
    };
    let (kind, message) = inst.describe_raised(value);
    (kind.to_string(), message.to_string())
}

#[test]
fn an_exported_member_is_reachable() {
    let (inst, outcome) = run_bundled("import lib\nlib.hello()\n");
    assert!(matches!(outcome, Outcome::Completed(_)), "{outcome:?}");
    assert_eq!(inst.output(), b"hi\n");
}

#[test]
fn a_private_member_is_not_exported() {
    let (kind, message) = raised("import lib\nlib.secret()\n");
    assert_eq!(kind, "not-exported");
    assert!(message.contains("secret"), "{message}");
    assert!(message.contains("lib"), "{message}");
    assert!(message.contains("exports"), "points at the fix: {message}");
}

#[test]
fn an_absent_member_is_no_such_member() {
    let (kind, message) = raised("import lib\nlib.nope()\n");
    assert_eq!(kind, "no-such-member");
    assert!(message.contains("nope"), "{message}");
}

#[test]
fn a_selective_import_of_a_private_member_is_not_exported() {
    let (kind, _message) = raised("import lib.secret\n");
    assert_eq!(kind, "not-exported");
}

#[test]
fn a_wildcard_import_omits_private_members_and_names_them_on_use() {
    // `import lib.*` brings in only `hello`; touching `secret` is the helpful `not-exported`
    // (naming `lib`), not a bare `name-not-defined`.
    let (kind, message) = raised("import lib.*\nhello()\nsecret()\n");
    assert_eq!(kind, "not-exported");
    assert!(message.contains("lib"), "{message}");
}

#[test]
fn without_an_exports_statement_every_member_is_public() {
    // A module with no `exports` keeps today's behavior: all definitions are public.
    let mut inst = instance("import open\nopen.a()\nopen.b()\n");
    let mut outcome = run(&mut inst, Directive::RunToCompletion);
    while let Outcome::SuspendedImport(req) = &outcome {
        let res = if req.path == ["open"] {
            ImportResolution::Source {
                text: "to a()\n    print(\"a\")\nend\nto b()\n    print(\"b\")\nend\n".to_string(),
                canonical_id: "open".to_string(),
            }
        } else {
            ImportResolution::NotFound
        };
        outcome = resolve_import(&mut inst, res);
    }
    assert!(matches!(outcome, Outcome::Completed(_)), "{outcome:?}");
    assert_eq!(inst.output(), b"a\nb\n");
}

#[test]
fn a_file_level_module_block_wraps_the_file() {
    // `module App … end` wrapping the whole file is unwrapped: its body is the file's top
    // level, so it runs exactly as if the block weren't there (the name is documentation).
    let mut inst = instance("module App\n    print(\"wrapped\")\nend\n");
    let outcome = run(&mut inst, Directive::RunToCompletion);
    assert!(matches!(outcome, Outcome::Completed(_)), "{outcome:?}");
    assert_eq!(inst.output(), b"wrapped\n");
}

fn resolve_slugs(src: &str) -> Vec<String> {
    let nfc = normalize(src);
    let parsed = parse_program(nfc.as_ref(), ModuleId(0));
    let resolved = resolve_module(parsed.ast, parsed.root, ModuleId(0));
    resolved
        .diagnostics
        .iter()
        .map(|d| d.code.slug().to_string())
        .collect()
}

#[test]
fn a_module_block_that_does_not_wrap_the_file_is_a_static_error() {
    // A `module` block alongside other top-level statements is a nested sub-namespace,
    // deferred past v0.1 (D-M5-5).
    let slugs = resolve_slugs("let x = 1\nmodule Inner\n    let y = 2\nend\n");
    assert!(slugs.iter().any(|s| s == "nested-module"), "{slugs:?}");
}

#[test]
fn a_module_nested_inside_a_wrapper_is_a_static_error() {
    // Even inside a file-wrapping module, a second (nested) `module` block is unsupported.
    let slugs = resolve_slugs("module Outer\n    module Inner\n        let y = 2\n    end\nend\n");
    assert!(slugs.iter().any(|s| s == "nested-module"), "{slugs:?}");
}

#[test]
fn exporting_an_undeclared_name_is_a_static_error() {
    let src = "to a()\n    print(\"a\")\nend\nexports a, ghost\n";
    let nfc = normalize(src);
    let parsed = parse_program(nfc.as_ref(), ModuleId(0));
    let resolved = resolve_module(parsed.ast, parsed.root, ModuleId(0));
    let diags: Vec<(&str, &str)> = resolved
        .diagnostics
        .iter()
        .map(|d| (d.code.slug(), d.message.as_str()))
        .collect();
    assert!(
        diags
            .iter()
            .any(|(slug, msg)| *slug == "undeclared-export" && msg.contains("ghost")),
        "expected undeclared-export naming `ghost`: {diags:?}"
    );
}
