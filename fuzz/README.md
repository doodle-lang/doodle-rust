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
cargo +nightly fuzz build                # build all targets
cargo +nightly fuzz run fuzz_smoke       # fuzz a target
```

## Targets

- `fuzz_smoke` — M0 placeholder over `doodle_core::fuzz_smoke`. Real
  lexer/parser targets that drive the front end arrive at M1.

Not wired into CI yet — fuzz CI lands with the M1 targets. The nightly
toolchain and the fuzz dependency versions are intentionally unpinned at M0;
they get pinned (a dated `nightly-YYYY-MM-DD` and a committed `Cargo.lock`) when
fuzz enters CI at M1, so fuzz builds become reproducible then.

[cargo-fuzz]: https://github.com/rust-fuzz/cargo-fuzz
