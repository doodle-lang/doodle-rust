#!/bin/sh
# Usage: ./scripts/capi-smoke.sh
#
# Builds doodle-capi (release) and compiles + runs the C host smoke program
# (examples/c-host/main.c) against the generated header and the library,
# checking that doodle_version() is callable across the C ABI.
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
LIBDIR="target/release"
INCLUDE="crates/doodle-capi/include"

echo "Building doodle-capi (release)..."
cargo build --release --package doodle-capi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
BIN="$WORK/c-host-smoke"

echo "Compiling C smoke program with $CC..."
# The rpath lets the built binary find the cdylib at runtime without setting a
# library-path environment variable.
"$CC" examples/c-host/main.c \
    -I "$INCLUDE" \
    -L "$LIBDIR" \
    -Wl,-rpath,"$REPO_DIR/$LIBDIR" \
    -ldoodle_capi \
    -o "$BIN"

echo "Running..."
"$BIN"

echo "=== capi smoke OK ==="
