# doodle-rust

The Doodle engine, bindings, and CLI host, implemented in Rust.

Doodle is a small, dynamically typed, kid-first teaching language (a modern
Logo successor). The specs and plans live in the
[discussions](https://github.com/doodle-lang/discussions) repo:

- `spec/language.md` — the language specification
- `spec/engine.md` — the engine embedding & instrumentation API
- `plan/implementation.md` — the implementation plan (architecture AD1–AD8,
  milestones M0–M10)

## Layout

- `crates/doodle-core` — front end + resumable machine + heap/GC + engine API
- `scripts/hygiene/` — code-hygiene checks (run all: `./scripts/hygiene/run.sh`)

## Hygiene

Every check in `scripts/hygiene/` runs in CI as its own job (see
`.github/workflows/hygiene.yml`; the job matrix is enumerated from the
directory, so adding a check is just dropping a script in). Run locally with:

```sh
./scripts/hygiene/run.sh
```

A run is green only if the final line reads `=== N hygiene check(s) passed ===`.

## License

[MIT](LICENSE)
