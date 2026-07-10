#!/bin/sh
# Usage: ./scripts/capi-smoke.sh
#
# Builds doodle-capi (release, a static library) and compiles + STATICALLY
# links the C host smoke program (examples/c-host/main.c) against it, checking
# that doodle_version() is callable across the C ABI. Static linking is the
# embedding form: the archive is linked into the host binary — one artifact,
# nothing to locate at runtime.
#
# The C compiler defaults to `cc`; override with the CC environment variable.

set -e

. "$(dirname "$0")/hygiene/lib.sh"
# lib.sh's REPO_DIR assumes a scripts/hygiene/ location; this script is one
# level up in scripts/, so recompute the repo root from here.
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
# Fail loudly if the parse regressed: the `cc` driver auto-adds libc/libgcc, so
# an empty list would still link and run, silently masking a broken parse.
[ -n "$native_libs" ] || { echo "ERROR: could not parse native-static-libs"; exit 1; }
echo "native-static-libs: $native_libs"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
BIN="$WORK/c-host-smoke"

echo "Compiling + static-linking the C smoke program with $CC..."
# Link order: the C object, then the archive it calls into, then the system
# libraries the archive needs. $native_libs is intentionally word-split into
# separate linker flags.
# shellcheck disable=SC2086
"$CC" examples/c-host/main.c \
    -I "$INCLUDE" \
    "$STATIC" \
    $native_libs \
    -o "$BIN"

echo "Running..."
"$BIN"

echo "=== capi smoke OK ==="
