#!/usr/bin/env bash
# Runs one Terminal-Bench task through the synaps daemon inside the task container.
#
#   synaps-run.sh <prompts.jsonl> <log-dir> [<completion.env>]
#
# prompts.jsonl holds one JSON string per line, built on the host. Default
# (completion=confirm, the published run): the task instruction, then the fixed
# confirmation message. With completion.env (completion=closeout, opt-in) it
# holds only the instruction; what follows depends on how each turn ended
# (turn_end, next_step), and every decision is logged to completion.jsonl.
#
# This is how Synaps is used day to day (a client attached to `synaps daemon`),
# not `synaps rpc`. The script starts a daemon, then speaks its line-JSON socket
# protocol through `uds-relay` (a static stdin/stdout <-> Unix socket pipe):
# hello -> attach/create (TUI settings: linked-successor compaction) -> submit
# each prompt after the previous turn has finished -> /close (saved) -> stop.
# A turn is finished when every started turn has ended and none starts for 5 s
# (event-woken turns, e.g. subagent completions, count as more work).
set -uo pipefail

# How the last finished turn ended, from the session's last done/error/aborted
# event and the stop_reason of the session's last api-log row written since the
# turn was submitted (<rows-before> of its rows existed then). Child sessions'
# rows carry no session_id and are ignored. Prints "<end> <event> <stop_reason>",
# end one of end_turn max_tokens refusal error cancelled unknown, "-" if missing.
#   turn_end <events.jsonl> <api-log.jsonl> <session-id> <rows-before>
turn_end() {
  local last reason
  last="$(grep -E -o '"session":"(done|error)"|"ev":"aborted"' "$1" 2>/dev/null | tail -n 1 | sed 's/.*:"//; s/"$//')"
  reason="$(grep -F "\"session_id\":\"$3\"" "$2" 2>/dev/null | tail -n +"$(($4 + 1))" | tail -n 1 \
    | sed -n 's/.*"stop_reason":"\([a-z_]*\)".*/\1/p')"
  case "$last:$reason" in
    done:end_turn) echo "end_turn done end_turn" ;;
    done:max_tokens) echo "max_tokens done max_tokens" ;;
    error:refusal) echo "refusal error refusal" ;;
    error:*) echo "error error ${reason:--}" ;;
    aborted:*) echo "cancelled aborted ${reason:--}" ;;
    *) echo "unknown ${last:--} ${reason:--}" ;;
  esac
}

# What to send after a turn that ended <end>: continue (at most 2), then the one
# close-out; after a refusal only the optional note; nothing after the close-out
# or the note, on error/cancellation/unknown signals, or with under 60 s left.
#   next_step <end> <continuations-sent> <final-sent 0|1> <refusal-note 0|1> [<seconds-left>]
# Prints continue | closeout | refusal_note | none:<why>.
next_step() {
  local end="$1" cont="$2" final="$3" note="$4" left="${5:-}"
  if [ "$final" != 0 ]; then echo none:final_sent; return; fi
  if [ -n "$left" ] && [ "$left" -lt 60 ]; then echo none:deadline; return; fi
  case "$end" in
    end_turn) echo closeout ;;
    max_tokens) if [ "$cont" -lt 2 ]; then echo continue; else echo closeout; fi ;;
    refusal) if [ "$note" = 1 ]; then echo refusal_note; else echo none:refusal; fi ;;
    *) echo "none:$end" ;;
  esac
}

# Sourced (tests): functions only.
[ "${BASH_SOURCE[0]}" = "$0" ] || return 0

prompts="$1"
logs="$2"
completion="${3:-}"
mkdir -p "$logs"
cp "$prompts" "$logs/prompts.jsonl"
SX_TIMEOUT_S="" SX_CLOSEOUT="" SX_CONTINUE="" SX_REFUSAL_NOTE=""
if [ -n "$completion" ]; then
  cp "$completion" "$logs/completion.env"
  # shellcheck disable=SC1090
  . "$completion"
  start="$(date +%s)"
fi
api="$HOME/.cache/synaps/api-log.jsonl"
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

  # Submit one prompt (a JSON string) and wait until its turn has finished.
  # Fails if the connection is gone.
  sent=0
  submit() {
    rows0="$(grep -c -F "\"session_id\":\"$sid\"" "$api" 2>/dev/null)"
    rows0="${rows0:-0}"
    t0="$(cat "$st/turns")"
    printf '{"type":"cmd","session_id":"%s","cmd":{"cmd":"submit","text":%s,"attachments":[]}}\n' "$sid" "$1"
    sent=$((sent + 1))
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
    [ -e "$st/dead" ] && return 1
    [ -e "$st/idle" ] || echo "synaps-run: note: no Idle after turn $n" >> "$st/notes"
  }

  ok=1
  while IFS= read -r text; do
    [ -n "$text" ] || continue
    submit "$text" || { ok=0; break; }
  done < "$prompts"

  # completion=closeout: decide each follow-up from how the last turn ended.
  if [ -n "$completion" ] && [ "$ok" = 1 ]; then
    cont=0 final=0 note=0
    [ -n "$SX_REFUSAL_NOTE" ] && note=1
    while :; do
      read -r end event reason <<< "$(turn_end "$logs/daemon-events.jsonl" "$api" "$sid" "$rows0")"
      used=$(( $(date +%s) - start ))
      left=""
      [ -n "$SX_TIMEOUT_S" ] && left=$((SX_TIMEOUT_S - used))
      step="$(next_step "$end" "$cont" "$final" "$note" "$left")"
      action="${step%%:*}" why=""
      [ "$action" = none ] && why="${step#none:}"
      printf '{"after":%s,"end":"%s","event":"%s","stop_reason":"%s","action":"%s","why":"%s","elapsed_s":%s}\n' \
        "$sent" "$end" "$event" "$reason" "$action" "$why" "$used" >> "$logs/completion.jsonl"
      case "$action" in
        continue) msg="$SX_CONTINUE"; cont=$((cont + 1)) ;;
        closeout)
          msg="$SX_CLOSEOUT"; final=1
          if [ -n "$left" ]; then
            msg="${msg//@E@/$((used / 60))}"; msg="${msg//@T@/$((SX_TIMEOUT_S / 60))}"
            msg="${msg//@R@/$((left / 60))}"
          fi ;;
        refusal_note) msg="$SX_REFUSAL_NOTE"; final=1 ;;
        *) break ;;
      esac
      submit "$msg" || break
    done
  fi

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
