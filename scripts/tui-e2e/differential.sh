#!/bin/bash
# differential.sh — PLAN-phase3 §5.1 layer 3: tmux differential of the TUI
# against the REFERENCE BINARY built at f0ee1e62.
#
# usage: scripts/tui-e2e/differential.sh [REF_BIN]
#   REF_BIN defaults to ~/Projects/agent-runtime-ref/target/release/synaps
#   (git worktree add ~/Projects/agent-runtime-ref f0ee1e62 && cargo build --release).
#
# Runs tests/tui_transport_differential.rs (ignored by default) with
# SYNAPS_TUI_E2E=1. Captures land in target/tui-e2e/<scenario>.<step>.{ref,new}.txt.
# Prints "REFERENCE DIFF: empty" on success. Uses a private tmux server (-L tuidiff).
set -u
HERE=$(cd "$(dirname "$0")/../.." && pwd)
REF=${1:-$HOME/Projects/agent-runtime-ref/target/release/synaps}
[ -x "$REF" ] || { echo "reference binary not found: $REF" >&2; exit 2; }
command -v tmux >/dev/null || { echo "tmux required" >&2; exit 2; }
cd "$HERE"
SYNAPS_TUI_E2E=1 SYNAPS_REF_BIN="$REF" ${SYNAPS_TUI_E2E_ONLY:+SYNAPS_TUI_E2E_ONLY=$SYNAPS_TUI_E2E_ONLY} \
  cargo test --test tui_transport_differential -- --ignored --nocapture 2>&1 | tail -60
