#!/bin/bash
# client-ladder.sh BIN [FIXTURE_MB] — the phase-4 boot ladder (PLAN-phase4 §7.1).
#
# Starts a private daemon (fresh SYNAPS_BASE_DIR + SYNAPS_RUNTIME_DIR under
# /tmp, no plugins dir, synthetic auth), writes a fixture session
# (make-fixture-session.sh; MB=0 → the two wrapper messages), launches
#   SYNAPS_MEM_TRACE=1 SYNAPS_NO_BOOT_FX=1 BIN --attach --continue <id>
# in tmux -L bench (120×40), samples /proc/<pid> externally at PRE (default 7 s)
# and SETTLE (default 25 s), sends /quit, and prints the trace file as a table
# with a Δ column (RssAnon delta vs the previous stage).
#
# env: SETTLE (25), PRE (7), KEEP=1 (keep the temp dirs), OUT (table copy),
#      plus anything the client honours (SYNAPS_CLIENT_*, SYNAPS_MEMPROF_PURGE…).
# Never touches tmux servers other than `-L bench`. No root needed.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${1:?usage: client-ladder.sh BIN [FIXTURE_MB]}
MB=${2:-0}
SETTLE=${SETTLE:-25}
PRE=${PRE:-7}
TAG=$(basename "$BIN")
WORK=$(mktemp -d /tmp/ladder-$TAG-XXXX)
export SYNAPS_BASE_DIR=$WORK/base SYNAPS_RUNTIME_DIR=$WORK/run SYNAPS_DAEMON=1 SYNAPS_NO_BOOT_FX=1
export TERM=${TERM:-xterm-256color}
unset SYNAPS_MEM_TRACE
mkdir -p "$SYNAPS_BASE_DIR" "$SYNAPS_RUNTIME_DIR"; chmod 700 "$SYNAPS_BASE_DIR" "$SYNAPS_RUNTIME_DIR"
: > "$SYNAPS_BASE_DIR/config"
printf '{"anthropic":{"type":"oauth","refresh":"synthetic-refresh","access":"LADDER-SYNTHETIC-TOKEN","expires":9999999999999}}' > "$SYNAPS_BASE_DIR/auth.json"
chmod 600 "$SYNAPS_BASE_DIR/auth.json"
TRACE=$WORK/trace.log
cleanup() {
  tmux -L bench kill-server 2>/dev/null
  "$BIN" daemon stop >/dev/null 2>&1
  [ "${KEEP:-0}" = 1 ] || rm -rf "$WORK"
}
trap cleanup EXIT

tmux -L bench kill-server 2>/dev/null; sleep 0.5
ID="ladder-$$"
"$HERE/make-fixture-session.sh" "$ID" "$MB" >/dev/null
tmux -L bench new -d -s d -x 120 -y 40 "exec $BIN daemon --foreground"
for _ in $(seq 1 500); do sleep 0.02; "$BIN" daemon status --json 2>/dev/null | grep -q '"ok":true' && break; done
DPID=$(tmux -L bench list-panes -t d -F '#{pane_pid}')

t0=$(date +%s%N)
tmux -L bench new -d -s c -x 120 -y 40 "exec env SYNAPS_MEM_TRACE=1 SYNAPS_MEM_TRACE_FILE=$TRACE $BIN --attach --continue $ID"
for _ in $(seq 1 1500); do
  sleep 0.02
  tmux -L bench capture-pane -pt c 2>/dev/null | grep -qE '❯ |> $|>$' && break
done
t1=$(date +%s%N); ATTACH_MS=$(( (t1-t0)/1000000 ))
CPID=$(tmux -L bench list-panes -t c -F '#{pane_pid}')

sample() { # label
  local a t
  a=$(awk '/^RssAnon:/{print $2}' "/proc/$CPID/status" 2>/dev/null)
  t=$(awk '/^Threads:/{print $2}' "/proc/$CPID/status" 2>/dev/null)
  echo "extern:$1 t_ms=$(( ($(date +%s%N)-t0)/1000000 )) rss_anon_kb=${a:-?} threads=${t:-?} comms=$(cat /proc/$CPID/task/*/comm 2>/dev/null | sort | uniq -c | awk '{printf "%s×%s,", $2, $1}')"
}
sleep "$PRE"; EXT_PRE=$(sample pre)
sleep $((SETTLE - PRE)); EXT_POST=$(sample post)
DANON=$(awk '/^RssAnon:/{print $2}' "/proc/$DPID/status" 2>/dev/null)

tmux -L bench send-keys -t c -l "/quit"; tmux -L bench send-keys -t c Enter
for _ in $(seq 1 100); do sleep 0.05; kill -0 "$CPID" 2>/dev/null || break; done
kill -0 "$CPID" 2>/dev/null && tmux -L bench kill-session -t c 2>/dev/null

echo "binary=$BIN fixture_mb=$MB attach_ms=$ATTACH_MS client_pid=$CPID daemon_anon_kb=${DANON:-?} trace=$TRACE"
echo "$EXT_PRE"
echo "$EXT_POST"
if [ ! -s "$TRACE" ]; then
  echo "(no ladder lines — binary predates A1 or SYNAPS_MEM_TRACE unsupported)"
  exit 0
fi
{
  printf "%-8s %-16s %10s %8s %10s %10s %10s %10s %8s %4s  %s\n" t_ms stage rss_anon Δ alloc active resident retained meta thr extra
  awk '
  function kv(k,   i) { for (i=1;i<=NF;i++) if (index($i, k"=")==1) return substr($i, length(k)+2); return "" }
  {
    t=kv("t_ms"); s=kv("stage"); a=kv("rss_anon_kb"); al=kv("jemalloc_allocated_kb"); ac=kv("active_kb"); r=kv("resident_kb"); rt=kv("retained_kb"); m=kv("metadata_kb"); th=kv("threads")
    extra=""; for (i=1;i<=NF;i++) if ($i !~ /^(t_ms|stage|rss_anon_kb|jemalloc_allocated_kb|active_kb|resident_kb|retained_kb|metadata_kb|threads)=/) extra=extra " " $i
    d = (prev=="") ? "" : sprintf("%+d", a-prev); prev=a
    printf "%-8s %-16s %10s %8s %10s %10s %10s %10s %8s %4s %s\n", t, s, a, d, al, ac, r, rt, m, th, extra
  }' "$TRACE"
} | tee "${OUT:-/dev/null}"
