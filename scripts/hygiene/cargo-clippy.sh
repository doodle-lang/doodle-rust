#!/bin/sh
# Usage: ./scripts/hygiene/cargo-clippy.sh
#
# Runs clippy over the whole workspace (all targets: code, tests, examples,
# benches) with warnings promoted to errors.
#
# Exit code: 1 on any clippy warning/error, 0 otherwise.

. "$(dirname "$0")/lib.sh"
ensure_cargo

cd "$REPO_DIR" || exit 1
cargo clippy --workspace --all-targets --quiet -- -D warnings
