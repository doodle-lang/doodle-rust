#!/bin/sh
# Usage: ./scripts/hygiene/file-length.sh
#
# Checks non-test Rust source files for excessive length:
# warns over 500 lines, errors over 600.
#
# The limits are a forcing function against single-file blobs: when a file
# goes over, split it along natural boundaries — do not shrink comments or
# otherwise camouflage the size (see the workspace CLAUDE.md, "Don't Game
# Hygiene Checks").
#
# Excluded: integration tests (*/tests/*), benches, testdata, and files
# named tests.rs / *_test.rs — they may be longer.
#
# Exit code: 1 if any file exceeds the error limit, 0 otherwise.

. "$(dirname "$0")/lib.sh"

WARN_LIMIT=500
ERROR_LIMIT=600

errors=0
warns=0

for f in $(find "$REPO_DIR/crates" -name '*.rs' \
        -not -path '*/target/*' \
        -not -path '*/tests/*' \
        -not -path '*/benches/*' \
        -not -path '*/testdata/*' \
        -not -name 'tests.rs' \
        -not -name '*_test.rs' 2>/dev/null); do
    lines=$(wc -l < "$f")
    rel="${f#"$REPO_DIR"/}"
    if [ "$lines" -gt "$ERROR_LIMIT" ]; then
        echo "ERROR: $rel: $lines lines (limit $ERROR_LIMIT)"
        errors=$((errors + 1))
    elif [ "$lines" -gt "$WARN_LIMIT" ]; then
        echo "WARN:  $rel: $lines lines (soft limit $WARN_LIMIT)"
        warns=$((warns + 1))
    fi
done

if [ "$errors" -gt 0 ] || [ "$warns" -gt 0 ]; then
    echo ""
    echo "=== $errors error(s), $warns warning(s) ==="
fi

if [ "$errors" -gt 0 ]; then
    exit 1
fi
