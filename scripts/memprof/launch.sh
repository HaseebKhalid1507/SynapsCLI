#!/bin/bash
# launch.sh NAME CMD...  -> starts tmux session (server -L bench) with exec, polls for ready marker, prints ms
name=$1; shift
t0=$(date +%s%N)
tmux -L bench new -d -s $name -x 120 -y 40 "exec $*"
for i in $(seq 1 1500); do
  sleep 0.02
  if tmux -L bench capture-pane -pt $name 2>/dev/null | grep -qE '○ ready|❯ |> $|>$'; then break; fi
done
t1=$(date +%s%N); echo "$name: ready after $(( (t1-t0)/1000000 )) ms  pane_pid=$(tmux -L bench list-panes -t $name -F '#{pane_pid}')"
