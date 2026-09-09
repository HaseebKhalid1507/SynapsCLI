# scripts/memprof — memory acceptance tooling

Ported from joestar's `~/tmp/memprof` on bella (MEASUREMENTS.md). Linux, no root.

| script | purpose |
|---|---|
| `mem.sh PID…` | per-process + total RSS/PSS/USS (kB) for each PID and all descendants (`smaps_rollup`, `pgrep -P`) |
| `launch.sh NAME CMD…` | start `CMD` in tmux server `-L bench`, poll for the `○ ready` marker, print ms |
| `smaps_group.py PID` | group `/proc/PID/smaps` by mapping ([anon], [heap], binary, .so, files) |
| `peak.py CMD…` | wall time + peak RSS (`RUSAGE_CHILDREN`) of a one-shot command, e.g. `synaps --version` |
| `bench-sessions.sh BIN [N…]` | the §5.3 procedure: N idle TUIs × REPEAT, medians of PSS / RssAnon / procs / startup |
| `bench-turns.sh turns\|subagents BIN` | the two live-model §5.3 gates: RssAnon growth over 5 tiny turns (≤ +4.5 MB) and `set_global_broker == 1` across 3 parallel subagents (`SYNAPS_MEM_TRACE=1`, profile `bench`) |

`synaps status --memory [--json] [--pid N]` reports the same numbers as `mem.sh`
from inside the binary; `bench-sessions.sh` records both so they can be diffed
(gate: within 2 %). See `docs/memory-budget.md` for the baseline table and gates.
