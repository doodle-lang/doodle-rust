#!/bin/sh
# Usage: ./scripts/hygiene/cargo-deny.sh
#
# Runs cargo-deny (advisories, license allowlist, bans, sources) per
# deny.toml. Dependency discipline per implementation-plan AD8.
#
# Exit code: 1 on any violation, 0 otherwise.

. "$(dirname "$0")/lib.sh"
ensure_cargo

if ! command -v cargo-deny >/dev/null 2>&1; then
    echo "ERROR: cargo-deny not found on PATH (brew install cargo-deny," \
         "or cargo install --locked cargo-deny)"
    exit 1
fi

cd "$REPO_DIR" || exit 1
cargo deny check
