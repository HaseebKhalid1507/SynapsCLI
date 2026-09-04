#!/bin/bash
# usage: mem.sh PID [PID...]  -> per-process and total RSS/PSS/USS (kB) for each PID and all descendants
descendants() { local p=$1; echo $p; for c in $(pgrep -P $p); do descendants $c; done; }
tr=0; tp=0; tu=0; n=0
printf "%-8s %-8s %10s %10s %10s  %s\n" PID PPID RSS_kB PSS_kB USS_kB CMD
for root in "$@"; do
  for p in $(descendants $root); do
    [ -r /proc/$p/smaps_rollup ] || continue
    r=$(awk '/^Rss:/{print $2}' /proc/$p/smaps_rollup)
    ps_=$(awk '/^Pss:/{print $2}' /proc/$p/smaps_rollup)
    u=$(awk '/^Private_Clean:|^Private_Dirty:/{s+=$2} END{print s}' /proc/$p/smaps_rollup)
    ppid=$(ps -o ppid= -p $p | tr -d ' ')
    cmd=$(ps -o args= -p $p | cut -c1-70)
    printf "%-8s %-8s %10s %10s %10s  %s\n" $p $ppid $r $ps_ $u "$cmd"
    tr=$((tr+r)); tp=$((tp+ps_)); tu=$((tu+u)); n=$((n+1))
  done
done
awk -v n=$n -v r=$tr -v p=$tp -v u=$tu 'BEGIN{printf "TOTAL procs=%d RSS=%d kB (%.1f MB) PSS=%d kB (%.1f MB) USS=%d kB (%.1f MB)\n",n,r,r/1024,p,p/1024,u,u/1024}'
