#!/bin/bash
# bench-sessions.sh — the §5.3 memory acceptance procedure (SPEC-daemon-mode).
#
# usage: bench-sessions.sh BIN [N ...]            (default N = 1 2 3)
#        REPEAT=3 bench-sessions.sh /tmp/synaps-new
#
# For each N: start N TUIs under a private tmux server (-L bench), settle
# SETTLE s, then record per-pid RSS/PSS/USS (mem.sh), RssAnon/Threads of every
# `synaps` engine process, the `status --memory --json` report, and the tmux
# startup time to '○ ready'. Prints one summary line per run and writes raw
# output to $OUT (default /tmp/memprof-<binary>-N<N>-r<i>.txt).
#
# DAEMON=1 mode (PLAN-phase2 §5.5): one `synaps daemon --foreground` in tmux
# window `d`, then N `synaps attach --create` thin clients. PSS is measured
# over {daemon tree} ∪ {attach pids}; procs/session = (total − daemon tree)/N.
# Extra columns: daemon_pss, marginal_pss (PSS(N) − PSS(N−1)), daemon_anon
# (sum of RssAnon over the daemon tree — PSS is a sharing artefact once the
# clients map the same binary, RssAnon is the honest daemon-side number) and
# anon_marginal (daemon_anon(N) − daemon_anon(N−1) = daemon-side cost of one
# idle session). Requires SYNAPS_DAEMON=1 (exported here). Sessions are REAL
# SessionActors.
#
# DAEMON=1 PARKED=1 mode (PLAN-phase3 §5.5): every session is created from a
# FIXTURE_MSGS_MB (default 2) MB fixture journal (make-fixture-session.sh) via
# `synaps attach --continue <fixture> --create`; the daemon runs with
# SYNAPS_DAEMON_PARK_GRACE_SECS=$PARK_GRACE (default 5). Samples the daemon
# tree RssAnon with N clients attached (live), then kills every client, waits
# PARK_GRACE+2 s, purges, and samples again (parked). Columns: live_anon,
# live_marginal, parked_anon, parked_marginal, ratio, attach_ms (a fresh
# attach to a parked session = unpark latency). Gates: parked_marginal
# <= 1.0 MB and <= 0.25 x live_marginal; daemon procs constant.
#
# Never touches tmux servers other than `-L bench`. No root needed.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${1:?usage: bench-sessions.sh BIN [N ...]}; shift
NS=${*:-1 2 3}
REPEAT=${REPEAT:-3}
SETTLE=${SETTLE:-12}
TAG=$(basename "$BIN")
DAEMON=${DAEMON:-0}
PARKED=${PARKED:-0}
PARK_GRACE=${PARK_GRACE:-5}
FIXTURE_MSGS_MB=${FIXTURE_MSGS_MB:-2}
[ "$DAEMON" = 1 ] && export SYNAPS_DAEMON=1

median() { sort -n | awk '{a[NR]=$1} END{ if (NR==0) print "n/a"; else if (NR%2) print a[(NR+1)/2]; else print (a[NR/2]+a[NR/2+1])/2 }'; }

run_once() { # N i -> prints "pss_kb rss_anon_kb_of_first_engine procs startup_ms"
  local n=$1 i=$2 out=/tmp/memprof-$TAG-N$n-r$i.txt
  tmux -L bench kill-server 2>/dev/null; sleep 1
  local starts=()
  for s in $(seq 1 "$n"); do
    starts+=("$("$HERE/launch.sh" "s$s" "$BIN" | sed -E 's/.*ready after ([0-9]+) ms.*/\1/')")
    sleep 0.3
  done
  sleep "$SETTLE"
  local pids
  # launch.sh runs `exec BIN`, so the pane pid IS the engine process.
  pids=$(for s in $(seq 1 "$n"); do tmux -L bench list-panes -t "s$s" -F '#{pane_pid}'; done | tr '\n' ' ')
  {
    echo "== $BIN N=$n run=$i startup_ms=${starts[*]}"
    "$HERE/mem.sh" $pids
    for p in $pids; do echo -n "pid=$p "; grep -E 'RssAnon|Threads' "/proc/$p/status" | tr '\n' ' '; echo; done
    "$BIN" status --memory --json 2>/dev/null
  } > "$out"
  local pss procs anon
  pss=$(awk '/^TOTAL/{for(k=1;k<=NF;k++) if($k ~ /^PSS=/){sub("PSS=","",$k); print $k}}' "$out")
  procs=$(awk '/^TOTAL/{for(k=1;k<=NF;k++) if($k ~ /^procs=/){sub("procs=","",$k); print $k}}' "$out")
  anon=$(grep -m1 -oE 'RssAnon:\s+[0-9]+' "$out" | grep -oE '[0-9]+')
  tmux -L bench kill-server 2>/dev/null
  echo "$pss $anon $procs ${starts[0]} $out"
}

run_once_daemon() { # N i -> prints "pss_kb daemon_pss_kb procs_beyond_daemon startup_ms out"
  local n=$1 i=$2 out=/tmp/memprof-$TAG-daemon-N$n-r$i.txt
  tmux -L bench kill-server 2>/dev/null; sleep 1
  "$BIN" daemon stop >/dev/null 2>&1
  local t0 t1
  t0=$(date +%s%N)
  tmux -L bench new -d -s d -x 120 -y 40 "exec $BIN daemon --foreground"
  for _ in $(seq 1 500); do sleep 0.02; "$BIN" daemon status --json 2>/dev/null | grep -q '"ok":true' && break; done
  t1=$(date +%s%N)
  local dstart=$(( (t1-t0)/1000000 ))
  local dpid; dpid=$(tmux -L bench list-panes -t d -F '#{pane_pid}')
  local starts=()
  for s in $(seq 1 "$n"); do
    starts+=("$("$HERE/launch.sh" "s$s" "$BIN" attach --create | sed -E 's/.*ready after ([0-9]+) ms.*/\1/')")
    sleep 0.3
  done
  sleep "$SETTLE"
  local apids; apids=$(for s in $(seq 1 "$n"); do tmux -L bench list-panes -t "s$s" -F '#{pane_pid}'; done | tr '\n' ' ')
  {
    echo "== $BIN DAEMON N=$n run=$i daemon_start_ms=$dstart attach_start_ms=${starts[*]}"
    echo "-- daemon tree"; "$HERE/mem.sh" "$dpid"
    echo "-- attach clients"; "$HERE/mem.sh" $apids
    echo "-- all"; "$HERE/mem.sh" "$dpid" $apids
    for p in $dpid $apids; do echo -n "pid=$p "; grep -E 'RssAnon|Threads' "/proc/$p/status" | tr '\n' ' '; echo; done
    echo "-- daemon tree RssAnon"; tree_anon "$dpid"
    "$BIN" daemon status --json 2>/dev/null
    "$BIN" status --memory --json --pid "$dpid" 2>/dev/null
  } > "$out"
  local pss dpss procs dprocs danon
  danon=$(awk '/^-- daemon tree RssAnon/{f=1} f&&/^TREE_ANON/{print $2; exit}' "$out")
  pss=$(awk '/^-- all/{f=1} f&&/^TOTAL/{for(k=1;k<=NF;k++) if($k ~ /^PSS=/){sub("PSS=","",$k); print $k; exit}}' "$out")
  procs=$(awk '/^-- all/{f=1} f&&/^TOTAL/{for(k=1;k<=NF;k++) if($k ~ /^procs=/){sub("procs=","",$k); print $k; exit}}' "$out")
  dpss=$(awk '/^-- daemon tree/{f=1} f&&/^TOTAL/{for(k=1;k<=NF;k++) if($k ~ /^PSS=/){sub("PSS=","",$k); print $k; exit}}' "$out")
  dprocs=$(awk '/^-- daemon tree/{f=1} f&&/^TOTAL/{for(k=1;k<=NF;k++) if($k ~ /^procs=/){sub("procs=","",$k); print $k; exit}}' "$out")
  "$BIN" daemon stop >/dev/null 2>&1
  tmux -L bench kill-server 2>/dev/null
  echo "$pss $dpss $((procs-dprocs)) $dprocs ${starts[0]:-0} ${danon:-0} $out"
}

purge() { "$BIN" daemon purge >/dev/null 2>&1 || true; sleep 1; }

run_once_parked() { # N i -> prints "live_anon_kb parked_anon_kb dprocs_live dprocs_parked attach_ms out"
  local n=$1 i=$2 out=/tmp/memprof-$TAG-parked-N$n-r$i.txt
  tmux -L bench kill-server 2>/dev/null; sleep 1
  "$BIN" daemon stop >/dev/null 2>&1
  local ids=() s
  for s in $(seq 1 "$n"); do
    local id="memprof-fix-$s-$i-$$"
    "$HERE/make-fixture-session.sh" "$id" "$FIXTURE_MSGS_MB" >/dev/null
    ids+=("$id")
  done
  tmux -L bench new -d -s d -x 120 -y 40 "exec env SYNAPS_DAEMON_PARK_GRACE_SECS=$PARK_GRACE $BIN daemon --foreground"
  for _ in $(seq 1 500); do sleep 0.02; "$BIN" daemon status --json 2>/dev/null | grep -q '"ok":true' && break; done
  local dpid; dpid=$(tmux -L bench list-panes -t d -F '#{pane_pid}')
  for s in $(seq 1 "$n"); do
    "$HERE/launch.sh" "s$s" "$BIN" attach --continue "${ids[$((s-1))]}" --create >/dev/null
    sleep 0.3
  done
  sleep "$SETTLE"; purge
  {
    echo "== $BIN PARKED N=$n run=$i fixture_mb=$FIXTURE_MSGS_MB grace=$PARK_GRACE"
    echo "-- live daemon tree RssAnon"; tree_anon "$dpid"
    "$BIN" daemon status --json 2>/dev/null
  } > "$out"
  local live_anon live_procs
  live_anon=$(awk '/^-- live daemon tree RssAnon/{f=1} f&&/^TREE_ANON/{print $2; exit}' "$out")
  live_procs=$(awk '/^-- live daemon tree RssAnon/{f=1} f&&/^pid=/{c++} /^TREE_ANON/{print c; exit}' "$out")
  # Detach every client (kill its tmux session) → grace → parked.
  for s in $(seq 1 "$n"); do tmux -L bench kill-session -t "s$s" 2>/dev/null; done
  sleep $((PARK_GRACE + 2)); purge
  {
    echo "-- parked daemon tree RssAnon"; tree_anon "$dpid"
    "$BIN" daemon status --json 2>/dev/null
  } >> "$out"
  local parked_anon parked_procs
  parked_anon=$(awk '/^-- parked daemon tree RssAnon/{f=1} f&&/^TREE_ANON/{print $2; exit}' "$out")
  parked_procs=$(awk '/^-- parked daemon tree RssAnon/{f=1} f&&/^pid=/{c++} /^TREE_ANON/{print c; exit}' "$out")
  # Unpark latency: one fresh attach to a parked session.
  local attach_ms
  attach_ms=$("$HERE/launch.sh" "u1" "$BIN" attach "${ids[0]}" | sed -E 's/.*ready after ([0-9]+) ms.*/\1/')
  echo "-- attach_to_parked_ms=$attach_ms" >> "$out"
  "$BIN" daemon stop >/dev/null 2>&1
  tmux -L bench kill-server 2>/dev/null
  for id in "${ids[@]}"; do rm -f "${SYNAPS_BASE_DIR:-$HOME/.synaps-cli}/sessions/$id".json*; done
  echo "${live_anon:-0} ${parked_anon:-0} ${live_procs:-0} ${parked_procs:-0} ${attach_ms:-0} $out"
}

# Sum RssAnon (kB) over PID and all descendants; prints per-pid lines + TREE_ANON <kB>.
tree_anon() {
  local root=$1 tot=0
  descendants() { local p=$1; echo "$p"; for c in $(pgrep -P "$p"); do descendants "$c"; done; }
  for p in $(descendants "$root"); do
    [ -r "/proc/$p/status" ] || continue
    local a; a=$(awk '/^RssAnon:/{print $2}' "/proc/$p/status")
    echo "pid=$p RssAnon_kB=${a:-0} $(ps -o args= -p "$p" | cut -c1-60)"
    tot=$((tot + ${a:-0}))
  done
  echo "TREE_ANON $tot"
}

if [ "$DAEMON" = 1 ] && [ "$PARKED" = 1 ]; then
  echo "binary=$BIN repeat=$REPEAT settle=${SETTLE}s mode=DAEMON+PARKED fixture_mb=$FIXTURE_MSGS_MB grace=${PARK_GRACE}s"
  printf "%-4s %-12s %-14s %-12s %-16s %-8s %-12s %-10s\n" N "live_anon_MB" "live_marginal" "parked_anon" "parked_marginal" "ratio" "daemon_procs" "attach_ms(med)"
  prev_live=""; prev_parked=""
  for n in $NS; do
    L=(); PK=(); DP=(); S=()
    for i in $(seq 1 "$REPEAT"); do
      read -r live parked lprocs pprocs ams out < <(run_once_parked "$n" "$i")
      L+=("$live"); PK+=("$parked"); DP+=("$lprocs/$pprocs"); S+=("$ams")
    done
    l_med=$(printf '%s\n' "${L[@]}" | median)
    p_med=$(printf '%s\n' "${PK[@]}" | median)
    s_med=$(printf '%s\n' "${S[@]}" | median)
    lmarg=$( [ -n "$prev_live" ] && awk -v a="$l_med" -v b="$prev_live" 'BEGIN{printf "%.2f", (a-b)/1024}' || echo "-" )
    pmarg=$( [ -n "$prev_parked" ] && awk -v a="$p_med" -v b="$prev_parked" 'BEGIN{printf "%.2f", (a-b)/1024}' || echo "-" )
    ratio=$( [ -n "$prev_live" ] && awk -v a="$p_med" -v b="$prev_parked" -v c="$l_med" -v d="$prev_live" 'BEGIN{ if (c-d>0) printf "%.2f", (a-b)/(c-d); else print "n/a" }' || echo "-" )
    printf "%-4s %-12.1f %-14s %-12.1f %-16s %-8s %-12s %-10s\n" "$n" "$(awk -v k="$l_med" 'BEGIN{print k/1024}')" "$lmarg" "$(awk -v k="$p_med" 'BEGIN{print k/1024}')" "$pmarg" "$ratio" "${DP[0]}" "$s_med"
    prev_live=$l_med; prev_parked=$p_med
  done
  echo "gates (§5.5): parked_marginal <= 1.0 MB; parked_marginal <= 0.25 x live_marginal; daemon_procs constant; attach_ms to a parked session <= 500 ms (informational above)"
  exit 0
fi

if [ "$DAEMON" = 1 ]; then
  echo "binary=$BIN repeat=$REPEAT settle=${SETTLE}s mode=DAEMON"
  printf "%-4s %-10s %-12s %-14s %-12s %-14s %-12s %-12s %-10s\n" N "PSS_MB(med)" "daemon_pss" "marginal_pss" "daemon_anon" "anon_marginal" "procs/sess" "daemon_procs" "attach_ms(med)"
  prev=""; prev_anon=""
  for n in $NS; do
    P=(); D=(); C=(); DP=(); S=(); AN=()
    for i in $(seq 1 "$REPEAT"); do
      read -r pss dpss procs dprocs start danon out < <(run_once_daemon "$n" "$i")
      P+=("$pss"); D+=("$dpss"); C+=("$procs"); DP+=("$dprocs"); S+=("$start"); AN+=("$danon")
    done
    pss_med=$(printf '%s\n' "${P[@]}" | median)
    d_med=$(printf '%s\n' "${D[@]}" | median)
    an_med=$(printf '%s\n' "${AN[@]}" | median)
    st_med=$(printf '%s\n' "${S[@]}" | median)
    procs_per=$(awk -v c="${C[0]}" -v n="$n" 'BEGIN{printf "%.2f", c/n}')
    marg=$( [ -n "$prev" ] && awk -v a="$pss_med" -v b="$prev" 'BEGIN{printf "%.1f", (a-b)/1024}' || echo "-" )
    amarg=$( [ -n "$prev_anon" ] && awk -v a="$an_med" -v b="$prev_anon" 'BEGIN{printf "%.1f", (a-b)/1024}' || echo "-" )
    printf "%-4s %-10.1f %-12.1f %-14s %-12.1f %-14s %-12s %-12s %-10s\n" "$n" "$(awk -v k="$pss_med" 'BEGIN{print k/1024}')" "$(awk -v k="$d_med" 'BEGIN{print k/1024}')" "$marg" "$(awk -v k="$an_med" 'BEGIN{print k/1024}')" "$amarg" "$procs_per" "${DP[0]}" "$st_med"
    prev=$pss_med; prev_anon=$an_med
  done
  echo "gates: anon_marginal (daemon-side RssAnon per idle session) <= 15 MB; procs/sess == 1.00 (the attach client); daemon_procs constant across N"
  echo "note: marginal_pss is dominated by the attach client and PSS divides shared text by sharers — read anon_marginal for the daemon"
  exit 0
fi

echo "binary=$BIN repeat=$REPEAT settle=${SETTLE}s"
printf "%-4s %-10s %-14s %-12s %-10s\n" N "PSS_MB(med)" "RssAnon_MB(med)" "procs/sess" "startup_ms(med)"
for n in $NS; do
  P=(); A=(); C=(); S=()
  for i in $(seq 1 "$REPEAT"); do
    read -r pss anon procs start out < <(run_once "$n" "$i")
    P+=("$pss"); A+=("$anon"); C+=("$procs"); S+=("$start")
  done
  pss_med=$(printf '%s\n' "${P[@]}" | median)
  anon_med=$(printf '%s\n' "${A[@]}" | median)
  st_med=$(printf '%s\n' "${S[@]}" | median)
  procs_per=$(awk -v c="${C[0]}" -v n="$n" 'BEGIN{printf "%.2f", c/n}')
  printf "%-4s %-10.1f %-14.1f %-12s %-10s\n" "$n" "$(awk -v k="$pss_med" 'BEGIN{print k/1024}')" "$(awk -v k="$anon_med" 'BEGIN{print k/1024}')" "$procs_per" "$st_med"
done
