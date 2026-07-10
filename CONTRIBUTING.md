# Contributing to doodle-rust

doodle-rust is the Rust implementation of the Doodle engine. This repo is a
submodule of the [workspace](https://github.com/doodle-lang/workspace); the
process rules that govern all Doodle work live in the workspace
[`.claude/CLAUDE.md`](https://github.com/doodle-lang/workspace/blob/main/.claude/CLAUDE.md)
and take precedence over anything here.

## Build, test, hygiene

The toolchain is pinned in `rust-toolchain.toml` (a stable release, currently
1.97.0); rustup installs it automatically on first use.

```sh
cargo build --workspace
cargo test --workspace
cargo check --workspace --target wasm32-unknown-unknown
./scripts/hygiene/run.sh    # fmt, clippy -D warnings, cargo-deny, file checks
```

Hygiene must be green before pushing. Every hygiene check also runs as its own
CI job; adding a check is just dropping a script into `scripts/hygiene/`.

Other gates (see `.github/workflows/test.yml`): the conformance runner
(`cargo run -p conformance-runner`, see `conformance/README.md`), the wasm size
gate (`scripts/wasm-size.sh`), and the C ABI header + smoke
(`scripts/capi-header.sh`, `scripts/capi-smoke.sh`). Fuzzing needs a nightly
toolchain: `cargo +nightly fuzz build` (see `fuzz/README.md`).

## Don't game the hygiene checks

The hygiene rules exist to improve code quality, not as obstacles. Never take an
action that satisfies a check while undermining its purpose — e.g. when a file
exceeds the length limit, split it along natural boundaries; do not shrink
comments, fold lines, or sprinkle opt-out markers to camouflage size. See the
workspace `.claude/CLAUDE.md` ("Don't Game Hygiene Checks").

## Specs are the source of truth

Never change language or engine semantics without agreement. When the
implementation must diverge from the specs
([`language.md`](https://github.com/doodle-lang/discussions/blob/main/spec/language.md),
[`engine.md`](https://github.com/doodle-lang/discussions/blob/main/spec/engine.md))
or decide something they leave open, follow the
spec-delta process (plan §8): record it (Appendix C plus a `spec-delta` issue on
the **discussions** tracker), proceed only with a documented provisional choice,
resolve it in the spec by the close of the milestone that ships the behavior,
and land the conformance test citing the clause in the same change.

Determinism is load-bearing: no nondeterminism on any Doodle-observable path (no
default-hasher `HashMap`, no address-derived identity or iteration order, fixed
float formatting). Treat any determinism-gate diff as a release blocker.

## Review policy

Every change is reviewed before it lands, in two tiers:

- **Agent review suffices** for work that is mechanical or fully pinned by the
  specs, tests, and hygiene gates: routine implementation of a ratified plan
  item, refactors, test additions, and doc fixes — anything whose correctness is
  established by the conformance suite, unit/snapshot tests, and a focused
  adversarial review. A minimal adversarial review (spec-faithfulness,
  soundness, scope) accompanies each chunk of work.
- **User sign-off is required** for: any change to language or engine semantics
  or the resolution of a spec delta; scope calls (deferring a piece, dropping a
  declared output, adding a dependency or toolchain); anything outward-facing
  (pushing, publishing, enabling automation); choosing between materially
  different designs; and **certifying work against a quality bar the agent
  itself authored** — e.g. the M1.1 error-message rubric and the M1.13
  broken-syntax message review (the agent may do a rubric-pass and write
  per-program notes, but the user approves; the author of the bar must not
  self-certify against it). When in doubt, ask rather than assume.

This settles M1.13's reviewer question consistent with the ratified plan
(`discussions/plan/plan-m1.md`): front-end *implementation* lands under agent
review plus the conformance/snapshot gates, but the M1.1 error-message rubric
and the M1.13 message review require user sign-off (the agent writes per-program
rubric notes; the user approves all 40).

## Filing issues

- **Implementation bugs** → this repo (`doodle-rust`), using the bug template.
  Also follow the Bug Discovery Protocol (workspace `.claude/CLAUDE.md`): add a
  failing (expected-fail) test and a `discussions/claude-todo.md` entry.
- **Spec deltas** (divergences from, or open decisions left by, L/E) → the
  **discussions** repo, using the spec-delta template. Appendix C
  (`discussions/plan/implementation.md`) remains the record of substance.
