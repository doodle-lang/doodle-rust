#!/bin/sh
# Usage: ./scripts/hygiene/file-format.sh
#
# Three checks across authored text files in the repo:
#   1. No trailing whitespace (spaces or tabs) at end of any line.
#   2. Every non-empty file ends with a final newline.
#   3. No trailing blank lines — the last byte before the file's
#      final newline must not itself be a newline.
#
# Scope: .rs, .sh, .md, .yml, .toml under the repo root, excluding .git/,
# target/, and testdata/ (fixtures may have intentional formats).
#
# Exit code: 1 if any violations found, 0 otherwise.

. "$(dirname "$0")/lib.sh"

violations=0

# Build file list (one path per line, via a temp file, to avoid shell
# word-splitting surprises).
LIST=$(mktemp -t hygiene-file-format.XXXXXX)
trap 'rm -f "$LIST"' EXIT
find "$REPO_DIR" \
    \( -path "$REPO_DIR/.git" -o -path "$REPO_DIR/target" -o -path '*/testdata' \) -prune \
    -o -type f \( -name '*.rs' -o -name '*.sh' -o -name '*.md' \
                  -o -name '*.yml' -o -name '*.toml' \) -print \
    | sort > "$LIST"

# 1. Trailing whitespace
while IFS= read -r f; do
    out=$(awk '
        /[ \t]+$/ {
            rel = FILENAME; sub(REPO "/", "", rel)
            printf("%s:%d: trailing whitespace\n", rel, NR)
            e++
        }
        END { exit e > 0 ? 1 : 0 }
    ' REPO="$REPO_DIR" "$f")
    if [ -n "$out" ]; then
        echo "$out"
        n=$(printf '%s\n' "$out" | wc -l | tr -d ' ')
        violations=$((violations + n))
    fi
done < "$LIST"

# 2. Final newline
while IFS= read -r f; do
    if [ -s "$f" ]; then
        # tail -c 1 → that one byte. wc -l counts newlines (0 or 1).
        if [ "$(tail -c 1 "$f" | wc -l | tr -d ' ')" -ne 1 ]; then
            rel=${f#"$REPO_DIR"/}
            echo "$rel: missing final newline"
            violations=$((violations + 1))
        fi
    fi
done < "$LIST"

# 3. No trailing blank lines.  A correctly-terminated file ends with
#    `<content>\n`; one trailing blank line makes it `<content>\n\n`.
#    Detect by checking whether the last two bytes are both newlines.
while IFS= read -r f; do
    if [ -s "$f" ]; then
        if [ "$(tail -c 2 "$f" | wc -l | tr -d ' ')" -eq 2 ]; then
            rel=${f#"$REPO_DIR"/}
            echo "$rel: trailing blank line(s) at end of file"
            violations=$((violations + 1))
        fi
    fi
done < "$LIST"

if [ "$violations" -gt 0 ]; then
    echo ""
    echo "=== $violations file-format violation(s) ==="
    exit 1
fi
