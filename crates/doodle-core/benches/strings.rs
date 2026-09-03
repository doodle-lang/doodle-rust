//! String-operation benchmarks (plan R3, implementation.md §5) — the string-churn
//! heap/fragmentation workload and the adversarial combining-mark append (the AD4
//! quadratic-seam case), driven end to end through the real engine so the numbers cover
//! parse → resolve → machine, not a micro-slice. These track a performance **trend**
//! (statement throughput on the non-moving heap); they assert nothing, so a slow machine
//! or CI runner never fails the build. The floors (≥ 1 M native / 300 K wasm statements/s)
//! are evaluated from the trend, not here.
#![allow(missing_docs)] // the criterion_group!/criterion_main! macros expand to undocumented items

use criterion::{Criterion, criterion_group, criterion_main};
use doodle_core::diag::Severity;
use doodle_core::drive::{Directive, Limits, run};
use doodle_core::machine::{Instance, Registry};
use doodle_core::parse::parse_program;
use doodle_core::resolve::resolve;
use doodle_core::source::normalize;
use doodle_core::span::ModuleId;
use std::hint::black_box;

/// Loads `src` (which must load clean) and drives it to completion — the unit each
/// benchmark measures. Panics on a load error so a mis-typed workload is obvious.
fn run_program(src: &str) {
    let nfc = normalize(src);
    let parsed = parse_program(nfc.as_ref(), ModuleId(0));
    assert!(
        parsed
            .diagnostics
            .iter()
            .all(|d| d.severity != Severity::Error),
        "benchmark source must parse cleanly: {:?}",
        parsed.diagnostics
    );
    let resolved = resolve(parsed.ast, parsed.root, ModuleId(0));
    assert!(
        resolved.diagnostics.is_empty(),
        "benchmark source must resolve cleanly: {:?}",
        resolved.diagnostics
    );
    let mut instance = Instance::load(resolved.module, Limits::default(), Registry::new(), "main");
    black_box(run(&mut instance, Directive::RunToCompletion));
}

/// Heap/fragmentation churn: each iteration builds a transient concatenation that is
/// immediately discarded, so the run allocates and reclaims a stream of short-lived
/// strings on the non-moving heap (stresses GC turnover and fragmentation, not growth).
fn string_churn(c: &mut Criterion) {
    let src = "\
let n = 0
while n < 20000 do
  let t = \"hello, \" + \"world\"
  n = n + 1
end
";
    c.bench_function("string_churn", |b| b.iter(|| run_program(src)));
}

/// The adversarial combining-mark append (plan AD4 / R3): appending a lone combining mark
/// in a loop grows an unbounded trailing non-starter run, so each seam scans back over the
/// whole run — O(n) per append, O(n²) overall. This documents (and guards against
/// regressions in) the known-quadratic path the grapheme memo and R8 cap later mitigate.
fn combining_mark_append(c: &mut Criterion) {
    let src = "\
let s = \"a\"
let n = 0
while n < 3000 do
  s = s + \"\\u{301}\"
  n = n + 1
end
";
    c.bench_function("combining_mark_append", |b| b.iter(|| run_program(src)));
}

/// A single large repetition (`\"abc\" * 20000`): the `*` build + the ASCII fast path in
/// the NFC check (an all-ASCII result is already NFC, so no re-normalization allocation).
fn string_repeat(c: &mut Criterion) {
    let src = "let s = \"abc\" * 20000\n";
    c.bench_function("string_repeat", |b| b.iter(|| run_program(src)));
}

criterion_group!(benches, string_churn, combining_mark_append, string_repeat);
criterion_main!(benches);
