#!/usr/bin/env bash
# ignore-ratchet.sh
#
# Counts attribute-position #[ignore lines under crates/agent-tui/tests/
# and fails if the count exceeds the baseline. Keeps us honest about
# not silently accumulating deferred/skipped tests.
#
# Pattern: ^[[:space:]]*#\[ignore
# This matches real #[ignore] attributes only — NOT doc-comment templates
# that merely mention the attribute in prose or // comments.
#
# Usage: bash scripts/ignore-ratchet.sh
#   Exit 0 — count is at or below baseline (prints count).
#   Exit 1 — count exceeds baseline (prints offending lines).

set -euo pipefail

BASELINE=1  # T241 slice 0: mem_transcript synthetic benchmark is #[ignore]d (slow, loads syntect)
TARGET_DIR="crates/agent-tui/tests"

# Resolve relative to repo root regardless of CWD.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$REPO_ROOT"

mapfile -t matches < <(grep -rn '^[[:space:]]*#\[ignore' "$TARGET_DIR" || true)
COUNT=${#matches[@]}

if [[ "$COUNT" -gt "$BASELINE" ]]; then
    echo "✗ ignore-ratchet FAILED: $COUNT #[ignore] attribute(s) found, baseline is $BASELINE." >&2
    echo "Offending lines:" >&2
    for line in "${matches[@]}"; do
        echo "  $line" >&2
    done
    echo "Burn down ignored tests before adding new ones, then bump BASELINE." >&2
    exit 1
fi

echo "✓ ignore-ratchet OK: $COUNT / $BASELINE #[ignore] attribute(s) in $TARGET_DIR"
exit 0
