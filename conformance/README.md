# Doodle conformance suite (format v0)

Language conformance tests: one `.doodle` file per test, each pinned to a
language-spec clause. This document is the **source of truth** for the test
file format; it was ratified as the M0.4 mini-spec in the `discussions` repo
(`plan/plan-m0.md`) and moved here. The runner lives at
`tools/conformance-runner`.

## What runs today (M1.3)

The runner discovers tests, parses and syntax-validates each file's directive
block, and applies the **staged pass policy**: a test whose required pipeline
stage doodle-core implements is **executed** and its `expect-*` directives
matched against real output; a test above the implemented stage is **SKIP**,
not FAIL. As of M1.3 the lexer is implemented
(`doodle_core::stage::implemented_through()` is `Some(Stage::Lex)`), so
`stage: lex` tests run — matching `expect-static-error` / `expect-warning`
against the lexer's diagnostics. `mode: run` and `stage: parse`/`full` tests
still SKIP until those stages land.

Run it from the repo root:

```
cargo run --package conformance-runner            # defaults to ./conformance
cargo run --package conformance-runner -- <root>  # a different suite root
```

Output ends with `=== N passed, N failed, N skipped ===`; the process exits
non-zero only on an unexpected result (a FAIL — e.g. a malformed test file).

## Layout and naming

```
conformance/
  v0.1/
    lang/
      L3.2/sep-001_two_statements_one_line.doodle
      L6.5/arith-001_int_add.doodle
      ...
```

One file per test (multi-module tests get a directory form, spec'd at M5 when
imports land). The path encodes the primary clause; the filename is
`<topic>-<seq>_<slug>.doodle`. The **test id** is `<clause>-<topic>-<seq>`
(e.g. `L6.5-arith-001`), unique across the suite. The runner enforces that a
test's clause directory matches its primary `#! clause:` directive.

Frozen suites (`v0.1` after the M10 freeze) are never edited — later changes
create `v0.2/`.

## Directives

Directives are ordinary Doodle comments beginning `#!` at the top of the file,
before any code (order: header directives, then expectations):

```
#! clause: L6.5            (required; may repeat for secondary clauses)
#! mode: run               (run | static; default run)
#! stage: full             (lex | parse | full; static-mode only; default full)
```

**Directive recognition:** a directive is `#!` followed by a **space**
(`#! …`). `#!/…` is *not* a directive — it is an ordinary comment, so shebang
lines (L§3.3) remain testable. Directives may appear only before the first
non-comment line; comment and blank lines may separate them.

The `stage:` directive lets front-end work items land genuinely green tests
before the whole pipeline exists: `stage: lex` tests only tokenize,
`stage: parse` tests lex+parse, `stage: full` (default) runs the resolver too.
A test SKIPs when doodle-core reports its stage unimplemented.

### `mode: static`

The test is only loaded (lex/parse/resolve). It expects either success (no
expectation directives) or specific static errors:

```
#! expect-static-error: <substring> @ <line>:<col>
#! expect-warning: <substring> @ <line>:<col>
```

Every listed error must be reported at the given position (line/col are
1-based, in the NFC'd source per S-1), and no unlisted **errors** may occur;
matching is an **order-insensitive set match** on (substring, position);
positions disambiguate duplicates.
Warnings (e.g. the L§5.1 shadowing lint): every listed warning must occur;
*unlisted* warnings never fail a test, so success-expecting tests are not
brittle against new lints. These tests are runnable from **M1**.

### `mode: run`

The test is executed under the conformance host. The host registers a `print`
capability whose rendering (each argument via the value's textual rendering,
newline-terminated) is pinned here; other scripted capability stubs are a
**named placeholder**, spec'd with the engine drive-script format at M2b. The
expected **transcript** is the ordered list of:

```
#! expect-out: <text>                          (one print line)
#! expect-raise: <substring> @ <line>:<col>    (uncaught error terminating the test)
```

A `run` test passes iff the produced transcript matches the expectations
exactly (count, order, content) and the program terminates (a runner step
budget bounds it; hitting it is a FAIL). Runnable from **M2b** (the host's
`print` needs foreign-function registration; raise-only tests from M2a).

## Rules

- Expected text uses **substring** match per event (full-match is too brittle
  against message-wording iteration; positions are exact). Error-message
  *quality* is enforced by snapshot tests in `doodle-core`, not conformance.
- Tests are UTF-8, NFC'd by the engine like any source; non-ASCII source is
  encouraged where the clause demands it.
- `clause:` is mandatory; the coverage report (M10) is computed from it.

## Deferred (placeholders)

- **Engine drive scripts** (JSON: directives+resolutions in, expected
  outcome/position/stack stream out) — spec'd at M2b, under
  `conformance/v0.1/engine/`.
- **Multi-module tests** (directory form with a manifest) — M5.
- **Determinism harness** (run twice + GC-stress, diff traces) — a runner
  flag, M2a.
