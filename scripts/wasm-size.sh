#!/bin/sh
# Usage: ./scripts/wasm-size.sh
#
# Builds crates/doodle-wasm for wasm32 in release, optimizes it with
# `wasm-opt -Oz`, brotli-compresses the result, and checks it against the
# wasm size budget (implementation plan §6.5). Prints the raw, optimized, and
# compressed sizes alongside the budget; exits non-zero if over budget or if a
# required tool is missing or produces no output.
#
# The gate is fail-closed: any build/optimize/compress failure aborts rather
# than measuring a stale or empty artifact (a size gate that passes when the
# pipeline breaks is worse than useless).
#
# NOTE: this measures the raw cargo-built wasm. When the gate becomes binding
# at M3, the pipeline must run the wasm-bindgen CLI before wasm-opt so the
# measured artifact is the actual shippable wasm — the raw wasm still carries
# wasm-bindgen descriptor sections the CLI strips, so today's number
# over-estimates (conservatively).
#
# The budget may be overridden for testing via WASM_BUDGET_BYTES, e.g.
#   WASM_BUDGET_BYTES=1 ./scripts/wasm-size.sh   # force a failure
#
# Requires `wasm-opt` (binaryen) and `brotli` on PATH.

set -e

. "$(dirname "$0")/hygiene/lib.sh"
# lib.sh's REPO_DIR assumes a scripts/hygiene/ location; this script lives one
# level up in scripts/, so recompute the repo root from here.
REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ensure_cargo

# 300 KB brotli-compressed (plan §6.5). "KB" is decimal here (300,000 bytes),
# matching how web bundle sizes are conventionally reported. The gate becomes a
# binding constraint at M3; before then it just exercises the mechanism.
BUDGET_BYTES="${WASM_BUDGET_BYTES:-300000}"

for tool in wasm-opt brotli; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "ERROR: '$tool' not found on PATH (install binaryen / brotli)"
        exit 1
    fi
done

cd "$REPO_DIR"

echo "Building doodle-wasm (release, wasm32-unknown-unknown)..."
cargo build --release --target wasm32-unknown-unknown --package doodle-wasm

RAW="target/wasm32-unknown-unknown/release/doodle_wasm.wasm"
if [ ! -f "$RAW" ]; then
    echo "ERROR: expected wasm artifact not found: $RAW"
    exit 1
fi

WORK="target/wasm-size"
mkdir -p "$WORK"
OPT="$WORK/doodle_wasm.opt.wasm"
BR="$WORK/doodle_wasm.opt.wasm.br"

# Remove any stale outputs so a silently no-op tool cannot be measured.
rm -f "$OPT" "$BR"

wasm-opt -Oz -o "$OPT" "$RAW"
[ -s "$OPT" ] || { echo "ERROR: wasm-opt produced no output"; exit 1; }
brotli -f -o "$BR" "$OPT"
[ -s "$BR" ] || { echo "ERROR: brotli produced no output"; exit 1; }

raw_size=$(wc -c < "$RAW" | tr -d ' ')
opt_size=$(wc -c < "$OPT" | tr -d ' ')
size=$(wc -c < "$BR" | tr -d ' ')
case "$size" in
    '' | *[!0-9]*)
        echo "ERROR: could not measure compressed size"
        exit 1
        ;;
esac

echo ""
echo "raw .wasm:        $raw_size bytes"
echo "wasm-opt -Oz:     $opt_size bytes"
echo "brotli:           $size bytes"
echo "budget (brotli):  $BUDGET_BYTES bytes"

if [ "$size" -gt "$BUDGET_BYTES" ]; then
    echo ""
    echo "FAIL: wasm size $size bytes exceeds budget $BUDGET_BYTES bytes"
    exit 1
fi

echo ""
echo "=== wasm size OK: $size / $BUDGET_BYTES bytes ==="
