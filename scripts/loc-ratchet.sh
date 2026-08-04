#!/usr/bin/env bash
# loc-ratchet.sh
#
# P12.4: line-count ratchet for the TUI event-loop spine. After the P12.1–4
# run() split, crates/agent-tui/src/tui/mod.rs is the setup call + the
# select! routing table + bounded teardown (~431 lines). This script fails
# if the file regrows past CEILING so loop logic can't silently accumulate
# there again — new logic belongs in run_setup.rs / dispatch.rs /
# loop_arms.rs / stream_handler.rs.
#
# CEILING = 470 (bumped from 460 for the live-MXC myx-theme arm; margin for
# comments/small glue). Bump only with review, never to "make CI green".
#
# Usage: bash scripts/loc-ratchet.sh
#   Exit 0 — line count at or below ceiling (prints count).
#   Exit 1 — line count exceeds ceiling.

set -euo pipefail

CEILING=470
TARGET_FILE="crates/agent-tui/src/tui/mod.rs"

# Resolve relative to repo root regardless of CWD.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$REPO_ROOT"

COUNT=$(wc -l < "$TARGET_FILE")

if [[ "$COUNT" -gt "$CEILING" ]]; then
    echo "✗ loc-ratchet FAILED: $TARGET_FILE is $COUNT lines, ceiling is $CEILING." >&2
    echo "run() must stay a routing table — move new logic into run_setup.rs," >&2
    echo "dispatch.rs, loop_arms.rs, or stream_handler.rs; bump CEILING only with review." >&2
    exit 1
fi

echo "✓ loc-ratchet OK: $COUNT / $CEILING lines in $TARGET_FILE"
exit 0
