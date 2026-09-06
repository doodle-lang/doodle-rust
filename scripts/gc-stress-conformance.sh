#!/bin/sh
# Usage: ./scripts/gc-stress-conformance.sh
#
# The GC-stress determinism gate over the whole conformance corpus, on the native AND C surfaces
# (M7.6, D-M7-11). Sets the `DOODLE_GC_STRESS` host hook so every instance collects at **every**
# safe point, then drives the corpus twice -- once natively (`conformance-runner`) and once through
# the example C host -- comparing each trace to the **same committed un-stressed oracle** (M7.5d).
# GC timing is unobservable (E§11), so any divergence under stress is a determinism leak and a
# release blocker.
#
# Driving through the C host is what makes this more than the in-crate GC unit tests: it exercises
# host-held handles as GC roots under a real churn pattern, and foreign-value finalizers firing at
# GC time across the C trampoline (the abort-on-unwind path) -- neither of which doodle-core's own
# tests can reach. The hook is a host-side certification toggle only: the engine never reads the
# environment (see `Instance::enable_gc_stress`), and it adds no symbol to the frozen `doodle.h`.
#
# The C compiler defaults to `cc`; override with the CC environment variable.

set -e

. "$(dirname "$0")/hygiene/lib.sh"
# lib.sh's REPO_DIR assumes a scripts/hygiene/ location; this script is one level up in scripts/.
REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ensure_cargo

cd "$REPO_DIR"

# The host hook: latched (pre-first-drive) by the native runner and the C host's `doodle_load`.
export DOODLE_GC_STRESS=1

echo "Native conformance under GC-stress..."
cargo run --quiet --package conformance-runner -- conformance

CC="${CC:-cc}"
INCLUDE="crates/doodle-capi/include"
STATIC="target/release/libdoodle_capi.a"

echo "Building doodle-capi (release static library)..."
cargo build --release --package doodle-capi
[ -f "$STATIC" ] || { echo "ERROR: expected static archive not found: $STATIC"; exit 1; }

# A Rust staticlib does not bundle the system libraries it needs; rustc reports them on a
# `note: native-static-libs:` line.
native_libs=$(cargo rustc --release --package doodle-capi --quiet \
    -- --print native-static-libs 2>&1 \
    | sed -n 's/^note: native-static-libs: //p' | tail -1)
[ -n "$native_libs" ] || { echo "ERROR: could not parse native-static-libs"; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
HOST="$WORK/c-host-conformance"

echo "Compiling + static-linking the C conformance host with $CC..."
# shellcheck disable=SC2086
"$CC" examples/c-host/conformance.c \
    -I "$INCLUDE" "$STATIC" $native_libs -o "$HOST"

echo "Conformance through the C host under GC-stress..."
cargo run --quiet --package conformance-runner -- --c-host "$HOST" conformance

echo "=== gc-stress conformance OK ==="
