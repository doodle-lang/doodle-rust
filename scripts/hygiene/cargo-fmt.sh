#!/bin/sh
# Usage: ./scripts/hygiene/cargo-fmt.sh
#
# Checks that all Rust code is rustfmt-formatted (no changes needed).
#
# Exit code: 1 if any file needs reformatting, 0 otherwise.

. "$(dirname "$0")/lib.sh"
ensure_cargo

cd "$REPO_DIR" || exit 1
cargo fmt --all --check
