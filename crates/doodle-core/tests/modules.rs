//! Module-loading tests (M5.1): the `import` suspension + the `{loading, loaded, failed}`
//! load-state machine (engine spec E§6, S-60; L§11.3). These drive through the public API
//! — `run`, `resolve_import`, `resolve`, `state`, `output`, `describe_raised` — acting as
//! the host module resolver, so they exercise the real suspend/resume path: an `import`
//! for an unloaded module suspends (`SuspendedImport`), the host supplies the source, the
//! module's top level drives to completion (observable, itself able to suspend), and the
//! importer resumes. Name *binding* per import form is M5.2 — an imported module here runs
//! only for its top-level effect.

use doodle_core::drive::{
    Directive, ImportResolution, Outcome, Resolution, resolve, resolve_import, run,
};
use doodle_core::machine::{
    Instance, InstanceState, Registry, print_intrinsic, read_line_intrinsic,
};
use doodle_core::parse::parse_program;
use doodle_core::resolve::resolve as resolve_module;
use doodle_core::source::normalize;
use doodle_core::span::ModuleId;

/// Loads `main` as the entry module with the `print` (0) and `read_line` (1) intrinsics,
/// asserting it compiles clean.
fn instance(main: &str) -> Instance {
    use doodle_core::diag::Severity;
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

/// Drives `inst` to a terminal outcome as a **bundling host**: resolves each import
/// against `modules` (dotted path → source; the path doubles as the canonical id), or
/// `NotFound` for an unlisted path. Panics on a capability suspend (use a bespoke loop for
/// those).
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
            Outcome::Suspended(_) => panic!("unexpected capability suspend: {outcome:?}"),
            _ => return outcome,
        }
    }
}

#[test]
fn import_runs_the_module_top_level_for_effect() {
    let mut inst = instance("import greeter\n");
    let outcome = bundle_run(&mut inst, &[("greeter", "print(\"hi from greeter\")\n")]);
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"hi from greeter\n");
    assert_eq!(inst.state(), InstanceState::Completed);
}

#[test]
fn a_module_loads_once_even_if_imported_twice() {
    // Two `import counter` statements: the module's top level runs once (L§11.3 singleton).
    let mut inst = instance("import counter\nimport counter\n");
    let outcome = bundle_run(&mut inst, &[("counter", "print(\"loading counter\")\n")]);
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"loading counter\n");
}

#[test]
fn the_importer_parks_until_the_host_resolves_the_import() {
    // The import suspends and the importer is parked: the statement after it has not run,
    // and only after the host resolves does the module load and the importer resume — in
    // that order (the module's top level completes before the importer continues).
    let mut inst = instance("import a\nprint(\"after import\")\n");
    let first = run(&mut inst, Directive::RunToCompletion);
    let Outcome::SuspendedImport(req) = first else {
        panic!("expected SuspendedImport, got {first:?}");
    };
    assert_eq!(req.path, vec!["a".to_string()]);
    assert_eq!(req.importer, 0);
    assert_eq!(inst.state(), InstanceState::Suspended);
    assert_eq!(
        inst.output(),
        b"",
        "the importer must not run past a parked import"
    );

    let done = resolve_import(
        &mut inst,
        ImportResolution::Source {
            text: "print(\"inside a\")\n".to_string(),
            canonical_id: "a".to_string(),
        },
    );
    assert!(matches!(done, Outcome::Completed(None)), "{done:?}");
    assert_eq!(inst.output(), b"inside a\nafter import\n");
}

#[test]
fn a_missing_module_raises_module_not_found_in_the_importer() {
    let mut inst = instance("import nope\n");
    let outcome = bundle_run(&mut inst, &[]); // nothing bundled → NotFound
    let Outcome::Raised(value, _) = outcome else {
        panic!("expected Raised, got {outcome:?}");
    };
    let (kind, message) = inst.describe_raised(value);
    assert_eq!(kind, "module-not-found");
    assert!(
        message.contains("nope"),
        "message names the path: {message}"
    );
    assert_eq!(inst.state(), InstanceState::Raised);
}

#[test]
fn a_host_raise_on_import_surfaces_in_the_importer() {
    let mut inst = instance("import net_thing\n");
    let first = run(&mut inst, Directive::RunToCompletion);
    assert!(matches!(first, Outcome::SuspendedImport(_)), "{first:?}");
    let reason = inst.make_string(b"network unreachable").unwrap();
    let outcome = resolve_import(&mut inst, ImportResolution::Raise(reason));
    let Outcome::Raised(value, _) = outcome else {
        panic!("expected Raised, got {outcome:?}");
    };
    // The host's own value is raised as-is (not wrapped in an engine `Error`).
    let (kind, message) = inst.describe_raised(value);
    assert_eq!(kind, "raised");
    assert!(
        message.contains("network unreachable"),
        "message: {message}"
    );
}

#[test]
fn a_module_whose_top_level_suspends_resumes_with_the_importer_parked() {
    // `a`'s top level calls the read_line capability while loading: a capability suspend
    // (not an import), with the importer parked beneath. Resolving it lets `a` finish, then
    // the importer resumes.
    let mut inst = instance("import a\nprint(\"main done\")\n");
    let first = run(&mut inst, Directive::RunToCompletion);
    assert!(matches!(first, Outcome::SuspendedImport(_)), "{first:?}");

    let loading = resolve_import(
        &mut inst,
        ImportResolution::Source {
            text: "print(read_line())\n".to_string(),
            canonical_id: "a".to_string(),
        },
    );
    assert!(
        matches!(loading, Outcome::Suspended(_)),
        "the module's top level reached a capability: {loading:?}"
    );

    let line = inst.make_string(b"typed line").unwrap();
    let done = resolve(&mut inst, Resolution::Value(line));
    assert!(matches!(done, Outcome::Completed(None)), "{done:?}");
    assert_eq!(inst.output(), b"typed line\nmain done\n");
}

#[test]
fn a_two_module_cycle_raises_circular_import_naming_the_cycle() {
    // main → a → b → a: importing `a` while it is still loading closes the cycle.
    let mut inst = instance("import a\n");
    let outcome = bundle_run(&mut inst, &[("a", "import b\n"), ("b", "import a\n")]);
    let Outcome::Raised(value, _) = outcome else {
        panic!("expected Raised, got {outcome:?}");
    };
    let (kind, message) = inst.describe_raised(value);
    assert_eq!(kind, "circular-import");
    assert!(
        message.contains("a") && message.contains("b"),
        "the diagnostic names the cycle: {message}"
    );
}

#[test]
fn a_module_raising_at_load_propagates_the_raise_to_the_importer() {
    // `boom` raises (divide by zero) while its top level runs; the raise propagates through
    // the (uncatchable, top-level-only) import to the entry boundary, and the raw error
    // surfaces. The `import` after it never runs (the program terminated). This also marks
    // `boom` `failed` — a state that prevents a re-import from being misread as a cycle;
    // the S-8 *re-raise on re-import* is latent until a reload/catch path exists (M9b).
    let mut inst = instance("import boom\nprint(\"never runs\")\n");
    let outcome = bundle_run(&mut inst, &[("boom", "let x = 1 / 0\n")]);
    let Outcome::Raised(value, _) = outcome else {
        panic!("expected Raised, got {outcome:?}");
    };
    let (kind, _message) = inst.describe_raised(value);
    assert_eq!(kind, "division-by-zero");
    assert_eq!(
        inst.output(),
        b"",
        "the importer must not run past a failed import"
    );
    assert_eq!(inst.state(), InstanceState::Raised);
}

#[test]
fn distinct_paths_with_one_canonical_id_load_once() {
    // The host maps two different import paths to the same canonical module: it loads once.
    let mut inst = instance("import p\nimport q\n");
    let mut outcome = run(&mut inst, Directive::RunToCompletion);
    while let Outcome::SuspendedImport(_) = &outcome {
        outcome = resolve_import(
            &mut inst,
            ImportResolution::Source {
                text: "print(\"shared body\")\n".to_string(),
                canonical_id: "shared".to_string(),
            },
        );
    }
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"shared body\n");
}

// ---- M5.2a: `import m` binding, `m.x` member access, cross-module calls ----

#[test]
fn import_binds_the_module_and_member_access_reads_a_const() {
    // `import lib` binds `lib` to the module value; `lib.answer` reads lib's const member.
    let mut inst = instance("import lib\nprint(lib.answer)\n");
    let outcome = bundle_run(&mut inst, &[("lib", "const answer = 42\n")]);
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"42\n");
}

#[test]
fn import_as_renames_the_bound_module() {
    let mut inst = instance("import lib as l\nprint(l.answer)\n");
    let outcome = bundle_run(&mut inst, &[("lib", "const answer = 7\n")]);
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"7\n");
}

#[test]
fn a_cross_module_call_runs_in_the_callees_module() {
    // `lib.greet()` invokes a function defined in `lib`; `apply` must read its parameters,
    // body, and slot layout from lib's module, not the caller's (the cross-module fix).
    let lib = "fn greet()\n  return \"hello from lib\"\nend\n";
    let mut inst = instance("import lib\nprint(lib.greet())\n");
    let outcome = bundle_run(&mut inst, &[("lib", lib)]);
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"hello from lib\n");
}

#[test]
fn a_cross_module_call_uses_the_callees_parameter_defaults() {
    // The default `b = 10` lives in lib's AST — `apply`'s `default_expr` must read it from
    // the callee's module.
    let lib = "fn add(a, b = 10)\n  return a + b\nend\n";
    let mut inst = instance("import lib\nprint(lib.add(3))\n");
    let outcome = bundle_run(&mut inst, &[("lib", lib)]);
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"13\n");
}

#[test]
fn member_reads_are_live_across_a_cross_module_mutation() {
    // `lib.bump()` mutates lib's own module-level `count`; a later `lib.count` reads the
    // updated cell (member access reads the live cell, not a snapshot).
    let lib = "let count = 0\nto bump()\n  count = count + 1\nend\n";
    let mut inst = instance("import lib\nlib.bump()\nlib.bump()\nprint(lib.count)\n");
    let outcome = bundle_run(&mut inst, &[("lib", lib)]);
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"2\n");
}

#[test]
fn accessing_a_missing_module_member_raises() {
    let mut inst = instance("import lib\nprint(lib.missing)\n");
    let outcome = bundle_run(&mut inst, &[("lib", "const x = 1\n")]);
    let Outcome::Raised(value, _) = outcome else {
        panic!("expected Raised, got {outcome:?}");
    };
    let (kind, message) = inst.describe_raised(value);
    assert_eq!(kind, "no-such-field");
    assert!(message.contains("missing"), "message: {message}");
}

#[test]
fn a_prelude_name_is_not_a_module_member() {
    // `Int` is in lib's namespace (the seeded prelude) but is not one of lib's own
    // definitions, so `lib.Int` is not a member.
    let mut inst = instance("import lib\nprint(lib.Int)\n");
    let outcome = bundle_run(&mut inst, &[("lib", "const x = 1\n")]);
    let Outcome::Raised(value, _) = outcome else {
        panic!("expected Raised, got {outcome:?}");
    };
    assert_eq!(inst.describe_raised(value).0, "no-such-field");
}

// ---- M5.2b: S-7 dotted-path resolution + member imports + cell aliasing ----

#[test]
fn a_member_import_binds_the_member_into_scope() {
    // `import lib.answer` (S-7): "lib.answer" is not a module, so it falls back to member
    // `answer` of module `lib`, bound directly into scope.
    let mut inst = instance("import lib.answer\nprint(answer)\n");
    let outcome = bundle_run(&mut inst, &[("lib", "const answer = 42\n")]);
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"42\n");
}

#[test]
fn a_member_import_as_renames_the_member() {
    let mut inst = instance("import lib.answer as a\nprint(a)\n");
    let outcome = bundle_run(&mut inst, &[("lib", "const answer = 7\n")]);
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"7\n");
}

#[test]
fn a_member_imported_function_is_callable() {
    let lib = "fn greet()\n  return \"hi from member\"\nend\n";
    let mut inst = instance("import lib.greet\nprint(greet())\n");
    let outcome = bundle_run(&mut inst, &[("lib", lib)]);
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"hi from member\n");
}

#[test]
fn a_member_import_is_a_live_alias_of_the_exporters_cell() {
    // `import lib.count` aliases lib's `count` cell (AD5); calling the imported `bump`
    // mutates it inside lib, and the aliased `count` read sees the update.
    let lib = "let count = 0\nto bump()\n  count = count + 1\nend\n";
    let mut inst = instance("import lib.count\nimport lib.bump\nbump()\nbump()\nprint(count)\n");
    let outcome = bundle_run(&mut inst, &[("lib", lib)]);
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"2\n");
}

#[test]
fn a_dotted_path_that_is_a_module_binds_the_module_not_a_member() {
    // S-7 tries the whole path as a module first: "geometry.shapes" IS a module here, so
    // `import geometry.shapes` binds the module (under its last segment), not a member.
    let mut inst = instance("import geometry.shapes\nprint(shapes.name)\n");
    let outcome = bundle_run(
        &mut inst,
        &[("geometry.shapes", "const name = \"the module\"\n")],
    );
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"the module\n");
}

#[test]
fn importing_a_missing_member_raises() {
    let mut inst = instance("import lib.nope\n");
    let outcome = bundle_run(&mut inst, &[("lib", "const x = 1\n")]);
    let Outcome::Raised(value, _) = outcome else {
        panic!("expected Raised, got {outcome:?}");
    };
    let (kind, message) = inst.describe_raised(value);
    assert_eq!(kind, "no-such-field");
    assert!(message.contains("nope"), "message: {message}");
}

#[test]
fn a_member_import_whose_prefix_module_is_missing_raises_module_not_found() {
    // "nope.member" is not a module, and neither is its prefix "nope" — a genuine miss.
    let mut inst = instance("import nope.member\n");
    let outcome = bundle_run(&mut inst, &[]);
    let Outcome::Raised(value, _) = outcome else {
        panic!("expected Raised, got {outcome:?}");
    };
    let (kind, message) = inst.describe_raised(value);
    assert_eq!(kind, "module-not-found");
    assert!(
        message.contains("nope"),
        "message names the prefix: {message}"
    );
}

// ---- M5.2c: wildcard imports + provenance/ambiguity (S-13) ----

#[test]
fn a_wildcard_import_brings_all_exports_into_scope() {
    let lib = "const alpha = 1\nfn beta()\n  return 2\nend\n";
    let mut inst = instance("import lib.*\nprint(alpha)\nprint(beta())\n");
    let outcome = bundle_run(&mut inst, &[("lib", lib)]);
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"1\n2\n");
}

#[test]
fn a_wildcard_imported_name_is_a_live_alias() {
    let lib = "let count = 0\nto bump()\n  count = count + 1\nend\n";
    let mut inst = instance("import lib.*\nbump()\nbump()\nprint(count)\n");
    let outcome = bundle_run(&mut inst, &[("lib", lib)]);
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"2\n");
}

#[test]
fn an_explicit_import_overrides_a_wildcard() {
    // Both wildcards would supply `x`, but the selective `import other.x` wins (explicit
    // beats wildcard, S-13) — no ambiguity.
    let mut inst = instance("import lib.*\nimport other.x\nprint(x)\n");
    let outcome = bundle_run(
        &mut inst,
        &[("lib", "const x = 1\n"), ("other", "const x = 99\n")],
    );
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"99\n");
}

#[test]
fn a_local_declaration_shadows_a_wildcard_import() {
    let mut inst = instance("import lib.*\nlet alpha = 100\nprint(alpha)\n");
    let outcome = bundle_run(&mut inst, &[("lib", "const alpha = 1\n")]);
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"100\n");
}

#[test]
fn a_name_from_two_wildcards_is_ambiguous_on_use() {
    // Exit criterion #3: a deliberate wildcard collision raises on use, naming both modules.
    let mut inst = instance("import one.*\nimport two.*\nprint(shared)\n");
    let outcome = bundle_run(
        &mut inst,
        &[("one", "const shared = 1\n"), ("two", "const shared = 2\n")],
    );
    let Outcome::Raised(value, _) = outcome else {
        panic!("expected Raised, got {outcome:?}");
    };
    let (kind, message) = inst.describe_raised(value);
    assert_eq!(kind, "ambiguous-import");
    assert!(
        message.contains("shared") && message.contains("one") && message.contains("two"),
        "the diagnostic names the name and both sources: {message}"
    );
}

#[test]
fn a_wildcard_name_in_no_source_is_undefined() {
    let mut inst = instance("import lib.*\nprint(missing)\n");
    let outcome = bundle_run(&mut inst, &[("lib", "const x = 1\n")]);
    let Outcome::Raised(value, _) = outcome else {
        panic!("expected Raised, got {outcome:?}");
    };
    assert_eq!(inst.describe_raised(value).0, "name-not-defined");
}

#[test]
fn a_fetched_module_with_static_errors_raises_module_load_error() {
    // A host-supplied source that does not compile is the module author's program error
    // (E§3.2 LoadError): it raises `module-load-error` at the `import` in the importer.
    let mut inst = instance("import broken\n");
    let first = run(&mut inst, Directive::RunToCompletion);
    assert!(matches!(first, Outcome::SuspendedImport(_)), "{first:?}");
    let outcome = resolve_import(
        &mut inst,
        ImportResolution::Source {
            text: "let = = =\n".to_string(),
            canonical_id: "broken".to_string(),
        },
    );
    let Outcome::Raised(value, _) = outcome else {
        panic!("expected Raised, got {outcome:?}");
    };
    let (kind, message) = inst.describe_raised(value);
    assert_eq!(kind, "module-load-error");
    assert!(
        message.contains("broken"),
        "message names the module: {message}"
    );
}
