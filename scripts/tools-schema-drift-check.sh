#!/usr/bin/env bash
# tools-schema-drift-check.sh
#
# CI drift check for the committed tool-schema manifest (docs/tools.json).
# Runs `synaps tools export` against the current binary, diffs the output
# against the committed file.  Exits non-zero if they differ.
#
# Usage:
#   ./scripts/tools-schema-drift-check.sh
#
# In CI, build the binary first:
#   cargo build --release --bin synaps
#   SYNAPS_BIN=./target/release/synaps ./scripts/tools-schema-drift-check.sh
#
# If drift is detected:
#   1. Run `synaps tools export --pretty > docs/tools.json`
#   2. Review the diff and commit.
#
# Note: MCP-bridged tools and runtime-loaded extension tools are NOT included
# in the committed snapshot because they require live subprocesses / user config.
# The manifest covers the builtin surface, which is the stable contract.

set -euo pipefail

SYNAPS_BIN="${SYNAPS_BIN:-./target/debug/synaps}"
COMMITTED="${COMMITTED:-docs/tools.json}"

if [[ ! -x "$SYNAPS_BIN" ]]; then
    echo "error: synaps binary not found at $SYNAPS_BIN" >&2
    echo "       Build it first: cargo build --bin synaps" >&2
    exit 2
fi

if [[ ! -f "$COMMITTED" ]]; then
    echo "error: committed manifest not found at $COMMITTED" >&2
    exit 2
fi

GENERATED=$("$SYNAPS_BIN" tools export --pretty)

if diff <(echo "$GENERATED") "$COMMITTED" > /dev/null 2>&1; then
    echo "✓ tool-schema manifest is up to date"
    exit 0
else
    echo "✗ tool-schema drift detected — committed docs/tools.json does not match binary output" >&2
    echo ""
    echo "Diff (generated vs committed):"
    diff <(echo "$GENERATED") "$COMMITTED" || true
    echo ""
    echo "To fix: run \`synaps tools export --pretty > docs/tools.json\` and commit."
    exit 1
fi
