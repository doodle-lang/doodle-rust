# Shared helpers for hygiene checks. Source, don't execute:
#     . "$(dirname "$0")/lib.sh"
#
# Provides:
#   REPO_DIR      — absolute path of the doodle-rust repo root.
#   ensure_cargo  — make sure `cargo` is on PATH (adds the usual rustup
#                   locations if needed); prints an error and exits 1 if
#                   cargo still can't be found.

REPO_DIR="$(cd "$(dirname "$0")/../.." && pwd)"

ensure_cargo() {
    if command -v cargo >/dev/null 2>&1; then
        return 0
    fi
    for d in "$HOME/.cargo/bin" /opt/homebrew/opt/rustup/bin; do
        if [ -x "$d/cargo" ]; then
            PATH="$d:$PATH"
            export PATH
            return 0
        fi
    done
    echo "ERROR: cargo not found on PATH (install Rust via rustup)"
    exit 1
}
