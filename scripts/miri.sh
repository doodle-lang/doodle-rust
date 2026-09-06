#!/bin/sh
# Usage: ./scripts/miri.sh
#
# Runs Miri over doodle-capi's tests (D-M7-11). AD1 concentrates all `unsafe` in
# doodle-capi (the C ABI: raw-pointer <-> reference conversions, Box::from_raw,
# the callback trampoline, the cross-thread control). Miri interprets those under
# Stacked Borrows and its data-race model, so it certifies the boundary has no
# aliasing/use-after-free/provenance UB — the checks the type system cannot make.
# (doodle-core has no `unsafe`; Miri there would certify the wrong crate.)
#
# Miri needs a nightly toolchain (it is not shipped with stable). The project
# BUILDS on the pinned stable in rust-toolchain.toml; this is a separate,
# dev/CI-only toolchain selected explicitly with `+`, so it never changes what
# ships. The nightly is pinned for reproducibility, exactly like the cbindgen
# version in scripts/capi-header.sh.

set -e

. "$(dirname "$0")/hygiene/lib.sh"
# lib.sh's REPO_DIR assumes a scripts/hygiene/ location; this script is one level
# up in scripts/, so recompute the repo root from here.
REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ensure_cargo

# A dated nightly known to carry the `miri` component. Bump deliberately (and
# re-run to confirm green); the CI job installs this exact toolchain.
MIRI_TOOLCHAIN="nightly-2026-09-05"

if ! rustup toolchain list 2>/dev/null | grep -q "^${MIRI_TOOLCHAIN}"; then
    echo "ERROR: toolchain '${MIRI_TOOLCHAIN}' not installed."
    echo "    rustup toolchain install ${MIRI_TOOLCHAIN} --profile minimal --component miri"
    exit 1
fi
if ! rustup component list --toolchain "${MIRI_TOOLCHAIN}" --installed 2>/dev/null \
    | grep -q '^miri'; then
    echo "ERROR: the 'miri' component is not installed for ${MIRI_TOOLCHAIN}."
    echo "    rustup component add miri --toolchain ${MIRI_TOOLCHAIN}"
    exit 1
fi

cd "$REPO_DIR"

# Tests run under Miri's default (Stacked Borrows + isolation). The doodle-capi
# suite is self-contained — no real clock/filesystem/RNG on any path it drives —
# so isolation stays on (a stricter, more deterministic check).
echo "Running Miri over doodle-capi (${MIRI_TOOLCHAIN})..."
cargo "+${MIRI_TOOLCHAIN}" miri test --package doodle-capi

echo "=== miri OK ==="
