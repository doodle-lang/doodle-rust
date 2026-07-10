#!/bin/sh
# Usage: ./scripts/hygiene/line-length.sh
#
# Checks .rs files for lines exceeding 100 characters (matching rustfmt's
# max_width; rustfmt cannot always enforce it, e.g. long string literals).
# Test files are included in the check.
#
# A line may opt out by ending with the marker "// LONG-LINE ALLOWED"
# (anywhere on the line). Use sparingly — only when splitting or
# shortening the line is impractical (e.g. a long error-message string
# literal).
#
# Exit code: 1 if any violations found, 0 otherwise.

. "$(dirname "$0")/lib.sh"

LINE_LIMIT=100

count=0

for f in $(find "$REPO_DIR/crates" -name '*.rs' \
        -not -path '*/target/*' \
        -not -path '*/testdata/*' 2>/dev/null); do
    rel="${f#"$REPO_DIR"/}"
    awk -v limit="$LINE_LIMIT" -v file="$rel" \
        'length > limit && index($0, "// LONG-LINE ALLOWED") == 0 {
             printf "%s:%d: %d chars\n", file, NR, length; found++
         }
         END { exit (found > 0) }' "$f"
    if [ $? -ne 0 ]; then
        count=$((count + 1))
    fi
done

if [ "$count" -gt 0 ]; then
    echo ""
    echo "=== $count file(s) with lines over $LINE_LIMIT chars ==="
    exit 1
fi
