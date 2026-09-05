#!/bin/sh
# Usage: ./scripts/capi-conformance.sh
#
# The M7.5e cross-surface conformance gate THROUGH THE C ABI: builds doodle-capi
# (release static library), compiles + statically links the example C conformance
# host (examples/c-host/conformance.c) against it, then drives every `mode: run`
# and `mode: drive` conformance fixture through that host with the Rust
# orchestrator (`conformance-runner --c-host`), comparing each transcript to the
# committed canonical `.transcript` oracle (M7.5d). Passing certifies the C surface
# produces traces identical to native/wasm — transitively, via the shared oracle.
#
# The C compiler defaults to `cc`; override with the CC environment variable.

set -e

. "$(dirname "$0")/hygiene/lib.sh"
# lib.sh's REPO_DIR assumes a scripts/hygiene/ location; this script is one level
# up in scripts/, so recompute the repo root from here.
REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ensure_cargo

cd "$REPO_DIR"

CC="${CC:-cc}"
INCLUDE="crates/doodle-capi/include"
STATIC="target/release/libdoodle_capi.a"

echo "Building doodle-capi (release static library)..."
cargo build --release --package doodle-capi
[ -f "$STATIC" ] || { echo "ERROR: expected static archive not found: $STATIC"; exit 1; }

# A Rust staticlib does not bundle the system libraries it needs; rustc reports
# them (platform-specific) on a `note: native-static-libs:` line.
native_libs=$(cargo rustc --release --package doodle-capi --quiet \
    -- --print native-static-libs 2>&1 \
    | sed -n 's/^note: native-static-libs: //p' | tail -1)
[ -n "$native_libs" ] || { echo "ERROR: could not parse native-static-libs"; exit 1; }
echo "native-static-libs: $native_libs"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
HOST="$WORK/doodle-conformance-host"

echo "Compiling + static-linking the C conformance host with $CC..."
# Link order: the C object, then the archive it calls into, then the system
# libraries the archive needs. $native_libs is intentionally word-split.
# shellcheck disable=SC2086
"$CC" examples/c-host/conformance.c \
    -I "$INCLUDE" \
    "$STATIC" \
    $native_libs \
    -o "$HOST"

echo "Driving the conformance suite through the C host..."
cargo run --quiet --package conformance-runner -- --c-host "$HOST" conformance

echo "=== capi conformance OK ==="
