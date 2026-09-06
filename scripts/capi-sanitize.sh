#!/bin/sh
# Usage: ./scripts/capi-sanitize.sh
#
# The sanitizer half of the D-M7-11 split (Miri covers the Rust side, this covers
# the C side): builds the example C hosts with AddressSanitizer +
# UndefinedBehaviorSanitizer (+ LeakSanitizer on Linux), links the release static
# library, and runs them. So C-host memory misuse (overflow / use-after-free /
# double-free of the host's OWN buffers), undefined behavior at the boundary, and
# (Linux) leaked host resources abort the gate. Runs both hosts: main.c (the smoke
# -- load/drive/handle + registry/print + foreign-fn + observation) and
# conformance.c driven over the whole run/drive corpus.
#
# Only the C hosts are instrumented; the Rust staticlib is the ordinary release
# build (its own aliasing/UAF is Miri's job -- scripts/miri.sh). ASAN still tracks
# the C-allocated copy-out buffers that cross the boundary, so a boundary write
# past a host buffer is caught here.
#
# LeakSanitizer is Linux-only; on macOS the same build runs ASAN + UBSan without
# leak checks (`detect_leaks=1` is quietly ignored there). The C compiler defaults
# to `cc`; override with the CC environment variable.

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
SANFLAGS="-fsanitize=address,undefined -fno-sanitize-recover=all -g"

# ASAN aborts on the first memory error; UBSan prints a stack and halts. Exported
# so the C hosts the orchestrator spawns as child processes inherit the same strict
# options. LeakSanitizer is part of ASAN on Linux and on by default there; macOS
# ASAN has no LSAN and *aborts* if asked for it, so only request leak detection on
# Linux (leaving macOS at its no-leak default -- ASAN + UBSan still run).
case "$(uname -s)" in
    Linux) export ASAN_OPTIONS="detect_leaks=1" ;;
esac
export UBSAN_OPTIONS="print_stacktrace=1:halt_on_error=1"

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

echo "Building the C smoke host with ASAN+UBSan..."
# shellcheck disable=SC2086
"$CC" $SANFLAGS examples/c-host/main.c \
    -I "$INCLUDE" "$STATIC" $native_libs -o "$WORK/c-host-smoke"
echo "Running the smoke host under sanitizers..."
"$WORK/c-host-smoke"

echo "Building the C conformance host with ASAN+UBSan..."
# shellcheck disable=SC2086
"$CC" $SANFLAGS examples/c-host/conformance.c \
    -I "$INCLUDE" "$STATIC" $native_libs -o "$WORK/c-host-conformance"
echo "Driving the conformance suite through the sanitized C host..."
cargo run --quiet --package conformance-runner -- --c-host "$WORK/c-host-conformance" conformance

echo "=== capi sanitize OK ==="
