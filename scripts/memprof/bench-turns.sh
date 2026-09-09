#!/bin/bash
# bench-turns.sh — the two §5.3 live-model gates (SPEC-daemon-mode).
#
# usage: bench-turns.sh turns     BIN     # (a) RssAnon after boot vs after 5 tiny turns; gate ≤ +4.5 MB
#        bench-turns.sh subagents BIN     # (b) 3 parallel subagents; gate: set_global_broker == 1
#
# Uses profile $PROFILE (default `bench`, i.e. ~/.synaps-cli/bench/config —
# point it at the cheapest model) so the run's synaps.log is private and
# greppable. Runs the TUI under tmux server `-L bench` with
# SYNAPS_MEM_TRACE=1: every SessionEvent::Done emits a `turn memory` line
# and every set_global_broker a `global broker installed` line.
# Costs real tokens (5 × "reply ok" / 1 × 3 subagents). No root.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
MODE=${1:?usage: bench-turns.sh turns|subagents BIN}
BIN=${2:?usage: bench-turns.sh turns|subagents BIN}
PROFILE=${PROFILE:-bench}
SETTLE=${SETTLE:-10}
TURN_TIMEOUT=${TURN_TIMEOUT:-120}
BASE=${SYNAPS_BASE_DIR:-$HOME/.synaps-cli}
LOG_DIR=$BASE/$PROFILE
S=bt

anon_kb() { awk '/^RssAnon:/{print $2}' "/proc/$1/status"; }
threads() { awk '/^Threads:/{print $2}' "/proc/$1/status"; }
logfile() { ls -t "$LOG_DIR"/synaps.log.* 2>/dev/null | head -1; }
count() { local f; f=$(logfile); if [ -n "$f" ]; then grep -c "$1" "$f" || true; else echo 0; fi; }
wait_count() { # pattern n -> waits until count(pattern) >= n
  local i; for i in $(seq 1 "$TURN_TIMEOUT"); do [ "$(count "$1")" -ge "$2" ] && return 0; sleep 1; done
  echo "timeout waiting for $2 × '$1'" >&2; return 1
}
send() { tmux -L bench send-keys -t $S -l "$1"; sleep 0.3; tmux -L bench send-keys -t $S Enter; }

tmux -L bench kill-server 2>/dev/null; sleep 1
mark=$(date +%s)
export SYNAPS_MEM_TRACE=1
"$HERE/launch.sh" $S "$BIN" --profile "$PROFILE" >/dev/null
pid=$(tmux -L bench list-panes -t $S -F '#{pane_pid}')
sleep "$SETTLE"
turns0=$(count 'turn memory')
boot_anon=$(anon_kb "$pid"); boot_thr=$(threads "$pid")
echo "binary=$BIN profile=$PROFILE pid=$pid log=$(logfile)"
echo "boot: RssAnon=${boot_anon} kB threads=${boot_thr}"

case $MODE in
turns)
  for t in 1 2 3 4 5; do
    send "reply with the single word ok"
    wait_count 'turn memory' $((turns0 + t)) || break
    echo "turn $t: RssAnon=$(anon_kb "$pid") kB threads=$(threads "$pid")"
  done
  sleep "$SETTLE"
  after=$(anon_kb "$pid")
  growth=$((after - boot_anon))
  echo "after 5 turns (+${SETTLE}s settle): RssAnon=${after} kB threads=$(threads "$pid")"
  awk -v g="$growth" 'BEGIN{printf "GROWTH %.2f MB (gate ≤ 4.5 MB) %s\n", g/1024, (g <= 4.5*1024) ? "PASS" : "FAIL"}'
  ;;
subagents)
  installs0=$(count 'global broker installed')
  send "use subagent_start three times in parallel, each task: reply ok"
  peak_rss=0; peak_thr=0
  for i in $(seq 1 60); do
    r=$(awk '/^VmRSS:/{print $2}' "/proc/$pid/status" 2>/dev/null || echo 0); th=$(threads "$pid")
    [ "$r" -gt "$peak_rss" ] && peak_rss=$r; [ "$th" -gt "$peak_thr" ] && peak_thr=$th
    sleep 1
  done
  installs=$(( $(count 'global broker installed') - installs0 ))
  # Per-process truth: the last `turn memory` line carries this process'
  # running set_global_broker count (the log file accumulates across runs).
  in_proc=$(grep 'turn memory' "$(logfile)" | tail -1 | grep -oE 'broker_installs=[0-9]+' | cut -d= -f2)
  echo "subagent_start calls: $(count 'subagent_start') ; turns done: $(( $(count 'turn memory') - turns0 ))"
  echo "peak: RSS=${peak_rss} kB threads=${peak_thr}; after: RssAnon=$(anon_kb "$pid") kB threads=$(threads "$pid")"
  echo "BROKER_INSTALLS this process=${in_proc:-?} (new log lines during turn: $installs) (gate: == 1) $([ "${in_proc:-0}" -eq 1 ] && echo PASS || echo FAIL)"
  ;;
*) echo "unknown mode $MODE" >&2; exit 2 ;;
esac
tmux -L bench send-keys -t $S C-c 2>/dev/null; sleep 1
tmux -L bench kill-server 2>/dev/null
