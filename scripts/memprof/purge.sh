#!/usr/bin/env bash
# Purge-before-sample helper (PLAN-phase3 C2). Run right before `mem.sh` /
# RssAnon sampling so the daemon (and, with SYNAPS_MEMPROF_PURGE=1, the
# attach clients on their next tick) return jemalloc dirty pages first.
#
#   scripts/memprof/purge.sh [BIN] [PROFILE]
#
# BIN defaults to $BIN or `synaps`; PROFILE is passed as `--profile`.
# Exit 0 whether or not a daemon is running (bench scripts call it
# unconditionally); prints the daemon's reply on success.
#
# bench-sessions.sh (B's file) can opt in with `PURGE=1` by calling this
# script before its sample block; nothing here touches that script.
set -u
BIN="${1:-${BIN:-synaps}}"
PROFILE="${2:-}"
args=()
[ -n "$PROFILE" ] && args+=(--profile "$PROFILE")
if ! SYNAPS_DAEMON=1 "$BIN" "${args[@]}" daemon status --json >/dev/null 2>&1; then
  echo "purge: no daemon running" >&2
  exit 0
fi
SYNAPS_DAEMON=1 "$BIN" "${args[@]}" daemon purge
# jemalloc's purge is asynchronous with respect to /proc accounting; give
# the kernel a beat before the caller samples.
sleep 0.5
