# Benchmarks

How Synaps measures startup latency and memory footprint, how to reproduce it,
and what the numbers were on the reference machine.

The harnesses live in [`benchmarks/`](benchmarks/). Methodology and portions of
their source are adapted from [jcode](https://github.com/1jehuang/jcode)'s
benchmark scripts (MIT, Jeremy Huang) — see [`benchmarks/NOTICE`](benchmarks/NOTICE).

## TL;DR

- **Warm start ~10 ms** to first frame (attach to the running daemon).
- **Cold start ~90–160 ms** (spawns the daemon on first launch).
- **~3.3 MB per additional session** — the daemon amortizes hard.
- On the reference machine, at 10 concurrent sessions Synaps used **less RAM
  than every other harness tested**, including jcode.

## Methodology

Both harnesses drive tools through a real pseudo-terminal (PTY), answer the
terminal-capability queries a TUI emits at boot (cursor position, device
attributes, color/OSC, synchronized-output, bracketed paste — without answering
these you measure the query timeout, not the tool), and render output through
the [`pyte`](https://pypi.org/project/pyte/) terminal screen model.

### Latency — `benchmarks/bench_latency.py`
Two user-visible metrics over N interactive launches:
1. **first-visible frame** — first rendered line with ≥3 alphanumerics.
2. **input-ready** — a probe string is injected on first-visible; time until it
   echoes back to the rendered screen.

Reports median + range over N runs (default 10).

> Synaps and other daemon-backed harnesses have **bimodal** latency: the first
> launch spawns the daemon (cold), later launches attach to it (warm). Both are
> reported below; run 1 is cold, runs 2–N are warm.

### Memory — `benchmarks/bench_memory.py`
Total **PSS** (Proportional Set Size, from `/proc/<pid>/smaps_rollup`) summed
across the whole process tree — daemon, workers, extension processes, and every
client. PSS is used rather than RSS because it splits shared pages proportionally,
which is the correct metric for multi-process / daemon architectures (RSS would
overcount shared memory). For Synaps, one daemon is shared across N
`synaps --attach` sessions; the daemon and all clients are summed together.

## Reproduce

```bash
pip install pyte                       # only dependency beyond python3 + Linux

# Latency (10 runs)
python3 benchmarks/bench_latency.py --runs 10 --tools synaps

# Memory at 1 and 10 concurrent sessions
python3 benchmarks/bench_memory.py --sessions 1  --tools synaps
python3 benchmarks/bench_memory.py --sessions 10 --tools synaps

# Point at a specific binary and/or compare against other harnesses on PATH
SYNAPS_BIN=./target/release/synaps \
  python3 benchmarks/bench_memory.py --sessions 10 --tools synaps jcode codex pi opencode
```

Memory is measured with extensions disabled (`disabled_plugins = ...` in
`~/.synaps-cli/config`) for the core-runtime number; each crash-isolated
extension process adds a fixed ~24 MB regardless of session count.

## Results

**Reference machine:** 24-core x86_64, 30 GB RAM, Linux. All tools measured on
the same machine with the same harness. Numbers are indicative of that hardware;
rerun locally for your own.

### Latency (10 runs)

| Phase | first-visible | input-ready |
|-------|--------------:|------------:|
| **warm** (attach to daemon) | **~10 ms** | **~11 ms** |
| **cold** (spawn daemon) | ~90–160 ms | — |

### Memory — total PSS across the process tree

| Tool | 1 session | 10 sessions | per added session |
|------|----------:|------------:|------------------:|
| **synaps** (core, no extensions) | **~38.8 MB** | **~68.5 MB** | **~3.3 MB** |
| jcode (memory off) | 62.7 MB | 136.2 MB | ~8.2 MB |
| synaps (with 3 extensions) | ~111 MB | ~141.7 MB | ~3.4 MB |
| pi | 119.6 MB | 717.8 MB | ~66.5 MB |
| codex | 137.0 MB | 1213.1 MB | ~119.6 MB |

At 10 concurrent sessions on the reference machine, Synaps core used **~50% less
RAM than jcode**, ~10× less than pi, and ~18× less than codex. The daemon is why:
sessions share one runtime, so the marginal cost of a session is a thin client.

## Caveats (read these)

- **Single machine.** Cross-machine comparison to any vendor's *published*
  numbers is unreliable — the same tools measured here differed substantially
  from their published figures (both directions). Only same-machine numbers are
  meaningful; reproduce on your own hardware.
- **Latency vs other daemon harnesses is not settled here.** A daemon harness's
  warm latency depends on daemon state; measure it yourself in your environment.
- **"Extensions off" is the core-runtime number.** Synaps runs extensions as
  separate, crash-isolated processes; each adds a fixed footprint that does not
  scale with sessions. The "with extensions" row shows a 3-extension config.
- **What is *not* measured here:** time-to-first-token, cost-per-task, and task
  success. First-frame latency and idle RAM are necessary, not sufficient.
