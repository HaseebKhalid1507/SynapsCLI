#!/usr/bin/env bash
# Runs one Terminal-Bench task through the synaps daemon inside the task container.
#
#   synaps-run.sh <prompts.jsonl> <log-dir>
#
# prompts.jsonl holds one JSON string per line, built on the host: the task
# instruction, then the fixed confirmation message.
#
# This is how Synaps is used day to day (a client attached to `synaps daemon`),
# not `synaps rpc`. The script starts a daemon, then speaks its line-JSON socket
# protocol through `uds-relay` (a static stdin/stdout <-> Unix socket pipe):
# hello -> attach/create (TUI settings: linked-successor compaction) -> submit
# each prompt after the previous turn has finished -> /close (saved) -> stop.
# A turn is finished when every started turn has ended and none starts for 5 s
# (event-woken turns, e.g. subagent completions, count as more work).
set -uo pipefail

prompts="$1"
logs="$2"
mkdir -p "$logs"
cp "$prompts" "$logs/prompts.jsonl"
st="$(mktemp -d /tmp/synaps-run.XXXXXX)"
echo 0 > "$st/turns"
echo 0 > "$st/ends"

# Default socket (no --socket override: `synaps daemon stop` only finds that one).
synaps daemon --detach >"$logs/daemon.out" 2>&1
sock="$(synaps daemon status 2>/dev/null | sed -n 's/^socket: //p' | head -1)"
if ! grep -q '^daemon started' "$logs/daemon.out" || [ ! -S "$sock" ]; then
  echo "synaps-run: the daemon did not start (socket '$sock'):" >&2
  cat "$logs/daemon.out" >&2
  exit 5
fi

{
  uuid="$(cat /proc/sys/kernel/random/uuid 2>/dev/null || echo "synaps-run-$$")"
  printf '{"type":"hello","protocol_version":3,"client":{"kind":"attach","terminal":null,"instance":"%s","history":"digest","tail_items":0},"cwd":"%s","client_version":"%s","env":null}\n' \
    "$uuid" "$PWD" "$(synaps --version | grep -o '[0-9][0-9.]*' | head -1)"
  printf '{"type":"attach","attach":"create","config":{"cwd":"%s","compaction_policy":"linked_successor"},"mode":"mirror"}\n' "$PWD"
  for _ in $(seq 1 120); do { [ -s "$st/sid" ] || [ -e "$st/dead" ]; } && break; sleep 0.5; done
  sid="$(cat "$st/sid" 2>/dev/null)"
  [ -n "$sid" ] || exit 0

  # synaps 0.11.1 replaces a reasoning level the model doesn't support with
  # "off" and only logs a warning. Abort before the first prompt instead.
  if grep -h 'reasoning level not supported' "$HOME"/.synaps-cli/synaps.log* 2>/dev/null | tee "$st/clamped" | grep -q .; then
    exit 0
  fi
  rm -f "$st/clamped"

  while IFS= read -r text; do
    [ -n "$text" ] || continue
    t0="$(cat "$st/turns")"
    printf '{"type":"cmd","session_id":"%s","cmd":{"cmd":"submit","text":%s,"attachments":[]}}\n' "$sid" "$text"
    until [ "$(cat "$st/turns")" -gt "$t0" ] || [ -e "$st/dead" ]; do sleep 1; done
    # Finished: every started turn has ended (done/error/aborted) and none
    # starts for 5 s (event-woken turns, e.g. subagent completions, are work).
    while [ ! -e "$st/dead" ]; do
      n="$(cat "$st/turns")"
      if [ "$(cat "$st/ends")" -ge "$n" ]; then
        sleep 5
        [ "$(cat "$st/turns")" = "$n" ] && [ "$(cat "$st/ends")" -ge "$n" ] && break
      else
        sleep 2
      fi
    done
    [ -e "$st/dead" ] && break
    [ -e "$st/idle" ] || echo "synaps-run: note: no Idle after turn $n" >> "$st/notes"
  done < "$prompts"

  # /close: save and end the session in the daemon, like the TUI's /close.
  printf '{"type":"cmd","session_id":"%s","cmd":{"cmd":"engine_command","id":3,"name":"close","arg":""}}\n' "$sid"
  for _ in $(seq 1 60); do { [ -e "$st/ended" ] || [ -e "$st/dead" ]; } && break; sleep 1; done
} | uds-relay "$sock" \
  | tee "$logs/daemon-events.jsonl" \
  | {
      turns=0 ends=0
      put() { echo "$2" > "$st/$1.tmp"; mv "$st/$1.tmp" "$st/$1"; }
      while IFS= read -r line; do
        case "$line" in
          *'"type":"attached"'*)
            printf '%s' "$line" | grep -o '"meta":{"id":"[^"]*"' | head -1 | sed 's/.*"id":"//; s/"$//' > "$st/sid.tmp"
            mv "$st/sid.tmp" "$st/sid" ;;
          *'"ev":"turn_started"'*) rm -f "$st/idle"; turns=$((turns + 1)); put turns "$turns" ;;
          *'"session":"done"'*|*'"session":"error"'*|*'"ev":"aborted"'*) ends=$((ends + 1)); put ends "$ends" ;;
          *'"ev":"idle"'*) touch "$st/idle" ;;
          *'"ev":"ended"'*) touch "$st/ended" ;;
        esac
      done
      touch "$st/dead"
    }
status=$?
synaps daemon stop >>"$logs/daemon.out" 2>&1 || synaps daemon stop --force >>"$logs/daemon.out" 2>&1

if [ -s "$st/clamped" ]; then
  echo "synaps-run: synaps changed the configured reasoning level; trial aborted:" >&2
  cat "$st/clamped" >&2
  status=4
fi
[ -s "$st/notes" ] && cat "$st/notes" >&2
rm -rf "$st"

# Telemetry (telemetry = full, local only): per-request API log, request-trace
# metadata, the saved session and synaps's own log. Never the config: it holds
# the broker machine token.
cp "$HOME/.cache/synaps/api-log.jsonl" "$HOME/.cache/synaps/request-trace.jsonl" "$logs/" 2>/dev/null
mkdir -p "$logs/session"
cp "$HOME"/.synaps-cli/sessions/*.json "$logs/session/" 2>/dev/null
cp "$HOME"/.synaps-cli/synaps.log* "$logs/session/" 2>/dev/null

# The pipeline succeeds even when the turn failed (auth, API, refusal), so check
# that a turn actually finished: a failed task turn is an agent error, not a
# wrong answer, and Harbor should record it as one.
ev="$logs/daemon-events.jsonl"
if [ "$status" -eq 0 ] && ! grep -q '"session":"done"' "$ev"; then
  echo "synaps-run: no turn completed (see daemon-events.jsonl)" >&2
  grep '"session":"error"' "$ev" | tail -3 | cut -c1-400 >&2
  status=3
fi
echo "synaps-run: prompts=$(grep -c . "$prompts") turns=$(grep -c '"ev":"turn_started"' "$ev") done=$(grep -c '"session":"done"' "$ev") errors=$(grep -c '"session":"error"' "$ev")" >&2
exit "$status"
