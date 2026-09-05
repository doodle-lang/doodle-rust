# Doodle conformance suite (format v0)

Language conformance tests: one `.doodle` file per test, each pinned to a
language-spec clause. This document is the **source of truth** for the test
file format; it was ratified as the M0.4 mini-spec in the `discussions` repo
(`plan/plan-m0.md`) and moved here. The runner lives at
`tools/conformance-runner`.

## What runs today (M2a.12)

The runner discovers tests, parses and syntax-validates each file's directive
block, and applies the **staged pass policy**: a test whose required pipeline
stage doodle-core implements is **executed** and its `expect-*` directives
matched against real output; a test above the implemented stage is **SKIP**,
not FAIL. As of M2a.12 the machine runs the demo subset
(`doodle_core::stage::implemented_through()` is `Some(Stage::Run)`), so every
stage executes: `stage: lex`/`parse`/`full` match `expect-static-error` /
`expect-warning` against the front-end diagnostics, and `mode: run` drives the
program and matches `expect-raise` (message substring + source position) against
the uncaught exception. A `mode: run` test whose transcript needs a **capability**
that has not landed — `expect-out` needs `print` (M2b) — still SKIPs, keyed on the
test's expectations rather than the stage scalar, so raise-only run tests execute
now while output tests wait.

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
      L3.2/sep-001_two_statements_one_line.doodle              # single-module
      L6.5/arith-001_int_add.doodle
      L11.2/import-010_selective/main.doodle                   # multi-module
      L11.2/import-010_selective/lib.doodle
      ...
```

A **single-module** test is one `.doodle` file. A **multi-module** test
(directory-as-fixture, M5) is a *directory* holding `main.doodle` — the entry,
which carries the `#!` directives — plus sibling `<name>.doodle` module files;
an `import name` in the program resolves to `<name>.doodle` in that directory
(a nested `import a.b` to `a/b.doodle`), and any other module resolves
`module-not-found`. A directory holding `main.doodle` is one fixture: its other
`.doodle` files are its modules, not separate tests. Native-module and
suspending-capability scenarios are not expressible as pure-Doodle fixtures and
stay as `doodle-core` integration tests.

The path encodes the primary clause; a single-module file is
`<topic>-<seq>_<slug>.doodle` and a multi-module directory is
`<topic>-<seq>_<slug>/`. The **test id** is `<clause>-<topic>-<seq>`
(e.g. `L6.5-arith-001`), unique across the suite. The runner enforces that a
test's clause directory (the file's, or the fixture directory's, parent)
matches its primary `#! clause:` directive.

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
newline-terminated) is pinned here; the suspending capabilities `read_line`/
`time`/`random` are answered from the fixture's scripted `input:` queue (M7.5a).
The expected **transcript** is the ordered list of:

```
#! expect-out: <text>                          (one print line)
#! expect-raise: <substring> @ <line>:<col>    (uncaught error terminating the test)
#! expect-fault: <kind>                        (a non-resumable engine fault terminating the test)
#! input: <capability> -> <response>           (scripted answer to the next request for it)
#! requires: <name>[, <name>…]                 (manifest primitives the fixture selects)
```

Each `input:` `<response>` is a `<value>` (a string `"…"`, integer, float,
`true`/`false`, or `nil`) or `raise "<msg>"`; entries for one capability form an
in-order FIFO drawn from as it suspends. An unscripted capability request is a
fixture error (never a silent pass).

`expect-fault:` asserts a non-resumable **engine fault** (E§10) as the run's
terminal outcome — the S-15 `nested-suspend` (a capability inside a foreign
block-consumer, D-M7-1), a limit (`step-budget`, `heap`, …), etc. A fault is not
a Doodle-catchable raise, so it needs its own expectation; the `<kind>` spelling
is the drive-script fault vocabulary. Output is matched independently, so a
fixture may print and then fault.

`requires:` names the **manifest primitives** (below) the fixture calls — a test
capability (`read_line`) or the test foreign fn (`test_greet`). It is loud both
ways: a name not in this harness's manifest fails the fixture at start (not
mid-run with a confusing name-not-defined), and a typo'd requirement is a fixture
error. It is a **selection**, not a definition — the primitive's shape lives in
the manifest, identically on every surface. Comma- and/or whitespace-separated;
repeatable across lines.

A `run` test passes iff the produced transcript matches the expectations
exactly (count, order, content) and the program terminates (a runner step
budget bounds it; hitting it is a FAIL). Runnable from **M2b** (the host's
`print` needs foreign-function registration; raise-only tests from M2a).

### `mode: drive` (engine drive scripts, §4.3)

The **normative** drive-script format (this section is the spec; the Rust runner
in `tools/conformance-runner` is its reference parser — a second parser, e.g.
M7's C-ABI harness, MUST accept exactly this grammar). A drive fixture is a
program (the file body) plus a header **script**: debug **setup** applied once,
then an **ordered** sequence of drive steps whose actual outcome/position/stack
**transcript** is compared whole against the declared one. Setup:

```
#! break: [<canonical>] <line>   (a breakpoint, E§8.6; canonical id defaults to `main`)
#! raise-trap: on                (enable raise-trapping, E§8.7)
#! obs: subexpr | statement      (observation mode, E§8.8/S-62; default statement)
```

All setup directives MUST precede the first step. Each step begins with a `do:`
(a driving directive), a `resolve:`, or a `resolve-raise:`, followed by one
`expect:` (the stop it must produce) and an optional `stack:`:

```
#! do: run | continue | step | into | over | out    (a directive, E§7.3)
#! resolve: <value>                                  (fulfil the suspended capability, E§7.5)
#! resolve-raise: "<msg>"                            (raise <msg> at its call site)
#! expect: <stop>
#! stack: <elem>, <elem>, …                          (call frames, innermost first)
```

A `resolve:`/`resolve-raise:` step fulfils the capability the previous step
suspended on. A `<value>` is a string (`"…"`), integer, float (has a `.`),
`true`/`false`, or `nil`.

A `<stop>` is one of:

```
completed                          (the driven unit finished)
paused <reason> @ <line>:<col>     (reason: step | breakpoint | host-pause | raise-trap | slice-end)
raised <substring> @ <line>:<col>
suspended <capability> @ <line>:<col>   (a capability request, Outcome::Suspended)
import <path> @ <line>:<col>            (an import, Outcome::SuspendedImport; dotted path)
faulted <kind>                     (step-budget | heap | op-result | stack-depth | tail-history | cancelled | …)
```

A stack `<elem>` is `<line>`, `<name>@<line>`, or `<name>@<line>×<n>` (the
tail-iteration count `n`, E§8.3; `x` is accepted for `×`); the matcher checks
only what an element pins. Positions are 1-based, in the NFC'd source, and are
**absolute file lines** (the `#!` header counts). Imports resolve transparently,
like `mode: run` (sibling module file, else `NotFound`) — **unless** a step
asserts an `import` stop, which leaves the suspension for that step to inspect. An
**unknown** `do:`/`expect:`/stack token, or a **reserved-but-unimplemented** directive (`local:`,
`render:` — the named slots for a future value/inspection extension), is a
fixture **error**, never silently ignored. The format is versioned implicitly by
`mode: drive`; a future incompatible grammar would be `mode: drive2`. Drive
fixtures live under `conformance/v0.1/eng/<clause>/` and are runnable from **M6**.

## Capability manifest (portable, M7.5b)

The conformance host registers a **fixed, ordered** set of intrinsics + capabilities,
installed **identically** by every surface — native (`conformance-runner`), wasm
(`DoodleInstance.demo`), and the M7 C host. Registration order is a capability's
**replay identity** (E§5.5/§11): a capability's id is its registration index, and a
`suspended <capability>` / `input:` / `resolve:` directive resolves against that index,
so the three surfaces MUST register this list in this order:

```
0 print       6 time
1 length      7 random
2 each        8 test_greet
3 encode      (drawing/trig capabilities append here at M7.5d,
4 decode       preserving indices 0–8)
5 read_line
```

`print`/`length`/`each`/`encode`/`decode` are synchronous natives; `read_line`/`time`/
`random` are suspending capabilities a fixture scripts. Divergence in this order across
surfaces is a determinism leak (E§11) and a release blocker; the cross-surface trace
schema (M7.5d) is what catches it. The manifest is **append-only ordered** — new test
primitives append, never reorder, so existing transcripts (and capability ids, which
are replay identity) stay valid.

`test_greet(name, punct = "!", do body)` is the S-42 conformance foreign function
(E§5.1): a host `fn` with an **immutable default** parameter and a **trailing block**
parameter, both binding per L§8.3. It invokes the block once with `name`, then returns
`name + punct`. Its default is immutable **by construction** (built from `ConstValue`,
which has no list/dict/record variant) — the D-M7-8 mutable-default rejection, enforced
identically on native (`ConstValue`) and C (typed `set_default_*`, no handle variant), so
there is no runtime rejection to test; a fixture exercises it three ways (default omitted,
`punct:` keyword, a block that makes a non-local exit). A fixture selects it with
`#! requires: test_greet`.

## Rules

- Expected text uses **substring** match per event (full-match is too brittle
  against message-wording iteration; positions are exact). Error-message
  *quality* is enforced by snapshot tests in `doodle-core`, not conformance.
- Tests are UTF-8, NFC'd by the engine like any source; non-ASCII source is
  encouraged where the clause demands it.
- `clause:` is mandatory; the coverage report (M10) is computed from it.

## Deferred (placeholders)

- Value/local **inspection** assertions (`local:`, `render:`) are
  reserved-but-unimplemented named slots (§ `mode: drive`). (Capability-resolution
  steps — `resolve:`/`resolve-raise:` in a drive script, and `input:` in a `run`
  fixture — landed at M7.5a.)
- **Determinism harness** (run twice + GC-stress, diff traces) — a runner
  flag, M2a.
