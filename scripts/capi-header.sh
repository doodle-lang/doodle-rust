#!/bin/sh
# Usage:
#   ./scripts/capi-header.sh          # check the committed header is current
#   ./scripts/capi-header.sh --write  # regenerate the committed header
#
# Regenerates crates/doodle-capi/include/doodle.h from the crate with cbindgen.
# The default (no argument) mode regenerates to a temp file and fails if it
# differs from the committed header — i.e. the header is stale. Requires
# `cbindgen` on PATH.

set -e

. "$(dirname "$0")/hygiene/lib.sh"
# lib.sh's REPO_DIR assumes a scripts/hygiene/ location; this script is one
# level up in scripts/, so recompute the repo root from here.
REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ensure_cargo

# cbindgen's output format changes across releases, so the committed header is
# pinned to one version. Local regeneration and the CI install (test.yml) must
# use this same version, or the "no diff on regen" gate is not reproducible.
CBINDGEN_VERSION="0.29.4"

if ! command -v cbindgen >/dev/null 2>&1; then
    echo "ERROR: 'cbindgen' not found on PATH"
    echo "    cargo install cbindgen --locked --version $CBINDGEN_VERSION"
    exit 1
fi

have="$(cbindgen --version | awk '{print $2}')"
if [ "$have" != "$CBINDGEN_VERSION" ]; then
    echo "ERROR: cbindgen $have found, but the committed header is pinned to $CBINDGEN_VERSION."
    echo "    cargo install cbindgen --locked --version $CBINDGEN_VERSION"
    exit 1
fi

cd "$REPO_DIR"
CONFIG="crates/doodle-capi/cbindgen.toml"
COMMITTED="crates/doodle-capi/include/doodle.h"

if [ "$1" = "--write" ]; then
    cbindgen --quiet --config "$CONFIG" --crate doodle-capi --output "$COMMITTED"
    echo "wrote $COMMITTED"
    exit 0
fi

TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT
cbindgen --quiet --config "$CONFIG" --crate doodle-capi --output "$TMP"

if ! diff -u "$COMMITTED" "$TMP"; then
    echo ""
    echo "ERROR: $COMMITTED is out of date; regenerate with:"
    echo "    ./scripts/capi-header.sh --write"
    exit 1
fi

echo "=== $COMMITTED is up to date ==="
