#!/bin/sh
# Usage: ./scripts/hygiene/run.sh
#
# Runs every hygiene check in this directory in sequence and reports a
# summary. Each check is invoked the same way CI invokes it.
#
# Exit code: 0 if every check passed, 1 if any check failed.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SELF="$SCRIPT_DIR/$(basename "$0")"

passed=0
failed=0
failures=""

for s in "$SCRIPT_DIR"/*.sh; do
    [ "$s" = "$SELF" ] && continue
    # lib.sh is a sourced helper, not a check.
    [ "$(basename "$s")" = "lib.sh" ] && continue
    name=$(basename "$s" .sh)
    out=$("$s" 2>&1)
    rc=$?

    if [ -n "$out" ]; then
        echo "--- $name ---"
        echo "$out"
        echo ""
    fi

    if [ $rc -eq 0 ]; then
        passed=$((passed + 1))
        printf "PASS: %s\n" "$name"
    else
        failed=$((failed + 1))
        failures="$failures $name"
        printf "FAIL: %s\n" "$name"
    fi
done

echo ""
total=$((passed + failed))
if [ $failed -gt 0 ]; then
    echo "=== $failed of $total hygiene check(s) failed:$failures ==="
    exit 1
fi
echo "=== $total hygiene check(s) passed ==="
