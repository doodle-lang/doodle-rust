# Doodle fuzz targets

libFuzzer targets for `doodle-core`, driven by [cargo-fuzz]. This is a
**separate crate on its own workspace** (note the empty `[workspace]` in
`Cargo.toml`), so it does not perturb the pinned-stable main workspace.

Fuzzing requires a **nightly** toolchain: cargo-fuzz uses `-Zbuild-std` and
AddressSanitizer, which are nightly-only. The main engine stays on the pinned
stable toolchain; only fuzzing needs nightly.

## Running

```sh
rustup toolchain install nightly --component rust-src
cargo +nightly fuzz build                     # build all targets
cargo +nightly fuzz run parse                 # fuzz a target
cargo +nightly fuzz run full -- -max_total_time=3600   # a 1 h soak
```

## Targets (M1.14)

Each drives a real `doodle-core` front-end entry over arbitrary input; the
invariant is that **no input panics, hangs, or OOMs** (it always terminates with
a `Vec<Diagnostic>`):

- `lex` — the lexer (`lex_to_diagnostics`) over arbitrary bytes (valid UTF-8,
  load-normalized as the host feeds it).
- `parse` — lex + parse (`parse_to_diagnostics`) over arbitrary text.
- `full` — lex + parse + **resolve** (`full_to_diagnostics`); catches resolver
  panics too (the resolver landed at M1.10/M1.11).

Smoke soak (60 s each, this machine): lex 3.8M / parse 1.4M / full 2.0M runs, zero
crashes. The M1 exit criterion is a **1 h soak** with zero panics/hangs/OOMs.

Not wired into CI yet — the fuzz-smoke CI job is a separate (user) decision. The
nightly toolchain date and the fuzz dependency versions get pinned (a dated
`nightly-YYYY-MM-DD` and a committed `Cargo.lock`) when fuzz enters CI, so fuzz
builds become reproducible then.

[cargo-fuzz]: https://github.com/rust-fuzz/cargo-fuzz
