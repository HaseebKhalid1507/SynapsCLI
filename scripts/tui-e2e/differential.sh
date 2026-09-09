#!/bin/bash
# differential.sh — PLAN-phase3 §5.1 layer 3: tmux differential of the TUI
# against the REFERENCE BINARY built at f0ee1e62.
#
# usage: scripts/tui-e2e/differential.sh [REF_BIN]
#   REF_BIN defaults to ~/Projects/agent-runtime-ref/target/release/synaps
#   (git worktree add ~/Projects/agent-runtime-ref f0ee1e62 && cargo build --release).
#
# Two panes per scenario: (R) the reference binary, (L) this binary
# in-process. SYNAPS_TUI_E2E_SOCKET=1 adds a third pane (S): a private daemon
# per scenario in the pane's HOME (SYNAPS_DAEMON=1 SYNAPS_RUNTIME_DIR=<home>/run,
# same stub) and `synaps --attach`; L≡S is printed as a second table
# ("SOCKET DIFF: empty (N)") modulo the socket normaliser (drops the
# `attached to … as client #…` banner; session ids as before).
#
# Scenarios (SYNAPS_TUI_E2E_ONLY=<name> to run one): plain_turn, tool_loop,
# abort_mid_stream, steer_mid_stream, settings_model_change, clear,
# compaction, queued_during_compaction, secret_prompt, extension_loaded
# (the only one that runs WITHOUT --no-extensions; plants the in-tree
# process-extension fixture). Needs python3 on PATH for that one.
#
# Runs tests/tui_transport_differential.rs (ignored by default) with
# SYNAPS_TUI_E2E=1. Captures land in target/tui-e2e/<scenario>.<step>.{ref,new}.txt.
# Prints a scenario → "diff empty?" table, then "REFERENCE DIFF: empty (N
# scenarios)" on success. Uses a private tmux server (-L tuidiff).
set -u
HERE=$(cd "$(dirname "$0")/../.." && pwd)
REF=${1:-$HOME/Projects/agent-runtime-ref/target/release/synaps}
[ -x "$REF" ] || { echo "reference binary not found: $REF" >&2; exit 2; }
command -v tmux >/dev/null || { echo "tmux required" >&2; exit 2; }
cd "$HERE"
# `env` so the optional vars are real assignments (a `${X:+X=$X}` word after
# the command name is a command, not an assignment).
env SYNAPS_TUI_E2E=1 SYNAPS_REF_BIN="$REF" \
  ${SYNAPS_TUI_E2E_ONLY:+SYNAPS_TUI_E2E_ONLY=$SYNAPS_TUI_E2E_ONLY} \
  ${SYNAPS_TUI_E2E_SOCKET:+SYNAPS_TUI_E2E_SOCKET=$SYNAPS_TUI_E2E_SOCKET} \
  cargo test --test tui_transport_differential -- --ignored --nocapture 2>&1 | tail -${SYNAPS_TUI_E2E_TAIL:-60}
