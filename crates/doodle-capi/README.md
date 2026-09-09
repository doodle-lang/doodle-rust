# Embedding the Doodle engine in C (`doodle-capi`)

`doodle-capi` is the **stable C embedding surface** for the Doodle engine. The distribution
artifact is three things:

- **`include/doodle.h`** — the C header (generated from this crate; the authoritative,
  per-function contract lives in its doc comments).
- **a built library** — a static archive (`libdoodle_capi.a`) or shared library you link.
- **this README** — the build/link recipe and the load-bearing contracts below.

The Rust crates behind it (`doodle-core`, the `doodle` CLI) are **internal**: no API-stability
promise. The **C ABI is the stable surface**; program against `doodle.h`, not the Rust crates.
The engine spec (referred to as *E* throughout `doodle.h`) is the normative description of
behavior; this README orients a C embedder and states the contracts that are easy to get wrong.

## Build and link

The crate builds a `staticlib` (and an `rlib` for the project's own tests). A Rust static
archive does **not** bundle the system libraries it needs, so you must also link the libraries
`rustc` reports for the target.

```sh
# 1. Build the release static archive → target/release/libdoodle_capi.a
cargo build --release --package doodle-capi

# 2. Ask rustc which system libraries the archive needs (varies by platform).
native_libs=$(cargo rustc --release --package doodle-capi --quiet \
    -- --print native-static-libs 2>&1 \
    | sed -n 's/^note: native-static-libs: //p' | tail -1)

# 3. Compile + link your host against the header and the archive.
cc your_host.c \
    -I crates/doodle-capi/include \
    target/release/libdoodle_capi.a \
    $native_libs \
    -o your_host
```

`examples/c-host/main.c` is a complete, minimal host you can copy; `scripts/capi-smoke.sh` runs
exactly the recipe above. The header is self-contained — it needs only standard C headers
(`<stdint.h>`, `<stdbool.h>`, `<stdlib.h>`, `<stdarg.h>`) — and declares no dependencies beyond the
archive and the system libraries from step 2.

## ABI versioning

The header pins a two-part **ABI contract version**, distinct from the engine version:

- `DOODLE_ABI_VERSION_MAJOR` / `DOODLE_ABI_VERSION_MINOR` — compile-time macros in `doodle.h`.
- `doodle_abi_version()` — the value the linked library was built with, as `(major << 16) | minor`.
- `doodle_version()` — the engine (semantic) version string, a separate axis.

Check the library against the header you compiled against at startup: a differing **major** is a
breaking mismatch (do not run); a newer **minor** is additive and safe (the library has symbols or
fields your header does not name). The surface is designed so this is the only check you need —
see "Growth without breaking" below.

## The core loop

```c
DoodleInstance *inst = NULL;
if (doodle_load(source, source_len, /*config*/ NULL, &inst, NULL, 0, NULL) != DoodleStatus_Ok)
    { /* a load error: parse/resolve diagnostics, or a rejected config */ }

DoodleOutcome out;
doodle_drive(inst, DoodleDirective_RunToCompletion, &out);
switch (out.kind) {
  case DoodleOutcomeKind_Completed:  /* done */ break;
  case DoodleOutcomeKind_Suspended:  /* a capability request — fulfil it, then doodle_resolve */ break;
  case DoodleOutcomeKind_SuspendedImport: /* an import — doodle_resolve_import* */ break;
  case DoodleOutcomeKind_Raised:     /* an uncaught exception — inspect it (below) */ break;
  case DoodleOutcomeKind_Faulted:    /* a non-resumable engine fault */ break;
  case DoodleOutcomeKind_Paused:     /* a debugger stop (breakpoint/step/host-pause) */ break;
}

doodle_free(inst); /* runs every live foreign value's finalizer exactly once */
```

The engine is a **resumable state machine the host drives**: every effect is a host capability,
and — given a program and a fixed sequence of capability resolutions — execution is
**deterministic and replayable**. Nothing runs except when you drive it.

## Contracts that are easy to get wrong

### Handles are owned, reference-counted, and instance-scoped

A value crossing to the host is a `DoodleHandle` (`uint64_t`) into **one instance's** heap.

- **You own every handle you receive** and must `doodle_release` it exactly as many times as it
  was **obtained** — and a reference is obtained by *minting* one (any accessor that hands you a
  handle) **or** by `doodle_retain`. Releasing too few leaks; too many is a stale-handle error.
- `DOODLE_NULL_HANDLE` (`0`) is **not** a handle — it means "no value" (e.g. a Void result, or an
  absent optional). Never release or inspect it.
- A handle is valid only on the **instance that minted it**. Using it on another instance is a
  host bug: in debug builds the engine detects it (`DoodleStatus_ErrContract`); in release it is
  never memory-unsafe, but may read a wrong-but-live value from the resolving instance. Using a
  released handle is `DoodleStatus_ErrStaleHandle`.

### Strings (and bytes) cross by copy — no validity window

A string reader (`doodle_string_bytes`, `doodle_raised_message`, …) **copies** UTF-8 bytes into a
caller-provided buffer and writes the full length needed to an out-parameter. You own the buffer;
the engine hands out **no interior pointer**, so there is no lifetime window to get wrong. Call
with a zero-capacity buffer first to size it, then again to fill it, if you like.

### Invalid calls reject and change nothing; `Faulted` is for execution

An **invalid call** — driving a terminal or suspended instance, resolving at the wrong time, or a
resolution carrying a stale/released/foreign handle — is **rejected**: it returns
`DoodleStatus_ErrContract`, writes no outcome, and **changes nothing**. The instance's state and
any pending request are untouched, so you can correct the call and try again (a resumable
suspension stays resumable). This is distinct from `DoodleOutcomeKind_Faulted`, which is a
*terminal state reached by execution* (a resource limit, cancellation, or an internal fault). In
short: a bad **call** is an `ErrContract` status; a bad **run** is a `Faulted` outcome.

### Post-mortem inspection of an uncaught raise

When a drive ends `Raised`, the exception and its trace are **retained** for inspection:

- `DoodleOutcome.value` — a host-owned handle to the raised value itself (the same value a Doodle
  `rescue e` binds), for structural inspection (`doodle_kind_of`, an `Error` record's fields, …).
- `doodle_raised_kind` / `doodle_raised_message` — the described (kebab-case kind + message) form.
- `doodle_raised_trace_frame_count` / `_frame_at` / `_frame_callable` and
  `doodle_raised_trace_tail_count` / `_tail_at` — the call stack at the raise (live frames plus the
  bounded tail-elided history), each as a position and a callable.

### Nothing panics across the boundary

Every entry point runs behind a panic firewall: a Rust panic becomes `DoodleStatus_ErrPanic`
rather than crossing the FFI (which would be undefined behavior). `ErrPanic` is a last-resort
signal of an engine bug, not a normal path; the instance may be unusable after it.

### Growth without breaking

Fixed-layout structs (e.g. `DoodleOutcome`) carry a `reserved` tail, and enums/tags cross as
fixed-width integers, so a newer (minor-bumped) library can add a field or an outcome kind an
older header does not name without changing any layout. Two rules keep this safe on your side:
always write `reserved` fields as `0`, and give every `switch` on an engine-produced enum a
`default` arm for a value you do not recognize.

## The GC-stress certification hook (not part of the ABI)

For determinism certification, the library can honor a `DOODLE_GC_STRESS` environment variable:
when set, every instance collects at every safe point, so a test harness can confirm a trace is
byte-identical under maximal GC pressure. This is **off by default** and gated behind the
`gc-stress` cargo feature — a shipped library must not vary with an ambient variable, so the
**distribution artifact is built without it** (`cargo build --release --package doodle-capi`,
no `--features`). It adds no `doodle.h` symbol. An embedder who wants to run the determinism gate
against their own build opts in with `cargo build --release --package doodle-capi --features
gc-stress` and sets `DOODLE_GC_STRESS=1`; the explicit `Instance::enable_gc_stress` path (Rust)
is unaffected by the feature.
