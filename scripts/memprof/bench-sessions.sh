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
# Never touches tmux servers other than `-L bench`. No root needed.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${1:?usage: bench-sessions.sh BIN [N ...]}; shift
NS=${*:-1 2 3}
REPEAT=${REPEAT:-3}
SETTLE=${SETTLE:-12}
TAG=$(basename "$BIN")

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
