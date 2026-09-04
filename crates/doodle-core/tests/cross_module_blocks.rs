//! Regression tests for passing a **block to a block-consumer procedure imported from another
//! module** (L§8.2 blocks + L§11 modules). A block-consumer like `to twice(do body)` invokes its
//! block via `body()`; when it is imported and handed a block defined in the *importing* module,
//! invoking the block must run that caller's block in its own context.
//!
//! Regression: `block_apply` once indexed the block's `desc.callable` against the *consumer's*
//! module (the imported proc's) instead of the block's defining module, so it read the wrong
//! callable and raised a spurious `missing-argument` naming that module's first procedure's
//! parameter. Fixed by reading the block's `CallableInfo` from its defining module (`block.rs`).

use doodle_core::diag::Severity;
use doodle_core::drive::{Directive, ImportResolution, Limits, Outcome, resolve_import, run};
use doodle_core::machine::{Instance, Registry, print_intrinsic};
use doodle_core::parse::parse_program;
use doodle_core::resolve::resolve;
use doodle_core::source::normalize;
use doodle_core::span::ModuleId;

/// Loads `main` as the entry module with just `print`, asserting it compiles clean.
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
    let resolved = resolve(parsed.ast, parsed.root, ModuleId(0));
    assert!(
        resolved.diagnostics.is_empty(),
        "resolve diagnostic(s): {:?}",
        resolved.diagnostics
    );
    let mut registry = Registry::new();
    registry.register(print_intrinsic()).unwrap();
    Instance::load(resolved.module, Limits::default(), registry, "main")
}

/// Drives `inst` as a bundling host, resolving each import against `modules` (dotted path to
/// source, the path doubling as the canonical id) or `NotFound`.
fn bundle_run(inst: &mut Instance, modules: &[(&str, &str)]) -> Outcome {
    let mut outcome = run(inst, Directive::RunToCompletion);
    while let Outcome::SuspendedImport(req) = &outcome {
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
    outcome
}

#[test]
fn a_block_passed_to_an_imported_consumer_runs_in_its_own_context() {
    // `lib` defines `greet(name)` (its first proc) then the block consumer `twice(do body)`. The
    // importer wildcard-imports `lib` and hands `twice` a `do print("x") end` block. Invoking
    // `body()` inside `twice` must run *that* block (printing "x" twice), not mis-resolve it to
    // `greet` and raise `missing-argument` for `greet`'s `name`.
    let lib = "to greet(name)\n  print(name)\nend\nto twice(do body)\n  body()\n  body()\nend\n";
    let mut inst = instance("import lib.*\ntwice() do\n  print(\"x\")\nend\n");
    let outcome = bundle_run(&mut inst, &[("lib", lib)]);
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"x\nx\n");
}

#[test]
fn an_imported_consumer_invokes_the_block_with_arguments() {
    // The consumer passes an argument into the block (`each_of` calls `body(item)`), exercising the
    // block-argument path (`got_block_arg` then `block_apply`) across modules. The block's
    // parameter list must be read from *its* module, so the argument binds to the block's `n`.
    let lib = "to run_two(do body)\n  body(1)\n  body(2)\nend\n";
    let mut inst = instance("import lib.*\nrun_two() do (n)\n  print(n)\nend\n");
    let outcome = bundle_run(&mut inst, &[("lib", lib)]);
    assert!(matches!(outcome, Outcome::Completed(None)), "{outcome:?}");
    assert_eq!(inst.output(), b"1\n2\n");
}
