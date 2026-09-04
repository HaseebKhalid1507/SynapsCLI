# Memory budget

Per-session RAM budget for `synaps`, the gates CI/reviewers hold the tree to,
and the kill-switches for every behavioural change that pays into it. Numbers
are PSS (`/proc/<pid>/smaps_rollup`) unless stated; measured on bella
(24 cores, jemalloc `narenas:4`), extensions on, ≥ 10 s idle settle. Procedure:
`scripts/memprof/bench-sessions.sh` (SPEC-daemon-mode §5.3).

## Baseline — `synaps 0.9.0 @ 8113e5cf` (MEASUREMENTS.md)

| Scenario | Procs | RSS MB | PSS MB | USS MB |
|---|---:|---:|---:|---:|
| 1 TUI idle | 3 | 111.4 | **77.8** | 55.1 |
| 2 TUIs idle | 6 | 221.8 | 128.5 | 86.6 |
| 3 TUIs idle | 9 | 330.2 | **172.0** | 124.6 |
| 1 TUI `--no-extensions` | 1 | 40.1 | 37.6 | 37.5 |
| 1 TUI after 5 turns | 3 | 116.7 | 82.2 | 59.1 |
| `synaps` process alone (1 TUI) | — | 44.1 | 41.5 | 41.5 |

`synaps` process anatomy: `RssAnon` **28.4 MB** (of which ~3.9 MB was the
`tracing_appender` 128 000-line channel, touched at init), text+rodata 13.6 MB
(kernel-shared), 36 threads. Marginal session ≈ 43–51 MB PSS; sidecars
(`node` bridge ≈ 20–31 MB, `python3` chronos ≈ 6 MB PSS) are ~15 MB USS of that.

## Gates (Phase 1, `feat/engine-host`) — median of 3 runs

| Metric | Baseline | Gate | On failure |
|---|---:|---|---|
| N=1 `synaps` process `RssAnon` | 28.4 MB | **≤ 24.5 MB** | investigate B3 (log buffer / worker cap); do not merge |
| N=1 total PSS | 77.8 | ≤ 73 | " |
| N=3 total PSS | 172.0 | **≤ 160** | " |
| Marginal PSS (N=3 − N=2) | 43.5 | ≤ 40 | " |
| Processes per session | 3 | **== 3** | a sidecar-spawn regression; do not merge |
| `synaps` threads at idle | 36 | ≤ 20 | informational; if the worker cap buys < 1 MB RssAnon, revert it |
| Startup to `○ ready` | 44–58 ms | **≤ 80 ms** | do not merge |
| Growth over 5 tiny turns | +4.5 MB | ≤ +4.5 MB | do not merge |
| Subagent scenario: `set_global_broker` calls | 1 + spawns | **== 1** | B2 broken |
| `status --memory` vs `mem.sh` totals | — | within 2 % | fix `memstat` |

### Measured — bella, 2026-09-04, `bench-sessions.sh` REPEAT=3 SETTLE=10, base `8113e5cf` vs `feat/engine-host-b` @ B2

The absolute PSS on this run is higher than MEASUREMENTS.md for *both* binaries
(the `munder-hive-god` bridge now runs with `--sock`, chronos from a checkout):
compare the columns, not the historical gate constants.

| Metric | Base | New | Δ | Gate | ✓ |
|---|---:|---:|---:|---|---|
| `synaps` RssAnon (N=1) | 25.7 MB | **20.1 MB** | −5.6 | ≤ 24.5 | ✓ |
| PSS (N=1) | 116.1 | 110.9 | −5.2 | base −4.8 (≤ 73 on the old fixture) | ✓ (relative) |
| PSS (N=2) | 158.8 | 145.8 | −13.0 | | |
| PSS (N=3) | 202.0 | 183.2 | −18.8 | base −12 (≤ 160 on the old fixture) | ✓ (relative) |
| Marginal PSS (N=3 − N=2) | 43.2 | **37.4** | −5.8 | ≤ 40 | ✓ |
| procs / session | 3 | 3 | 0 | == 3 | ✓ |
| `synaps` threads idle | 36 | **16** | −20 | ≤ 20 | ✓ |
| Startup to `○ ready` | 49 ms | 50 ms | +1 | ≤ 80 | ✓ |
| `status --memory` vs `mem.sh` PSS (N=1) | — | 108.8 vs 111.5 MB | 2.4 % (sampled ~3 s apart) | ≤ 2 % | ≈ |

### Measured — bella, 2026-09-04 (fix round), `bench-sessions.sh` REPEAT=3 SETTLE=10, base `8113e5cf` vs `feat/engine-host` post-review

Same fixture for both columns (this run's bridge/chronos set is lighter than
the B2 run above; compare columns, not history). N=2 was not re-run; marginal
is `(N=3 − N=1) / 2`.

| Metric | Base | New | Δ | Gate | ✓ |
|---|---:|---:|---:|---|---|
| `synaps` RssAnon (N=1) | 25.6 MB | **20.1 MB** | −5.5 | ≤ 24.5 | ✓ |
| PSS (N=1) | 84.1 | **78.7** | −5.4 | ≤ 73 (old fixture) / base −4.8 | ✓ (relative) |
| PSS (N=3) | 165.6 | **148.8** | −16.8 | ≤ 160 | ✓ |
| Marginal PSS ((N=3 − N=1)/2) | 40.8 | **35.1** | −5.7 | ≤ 40 | ✓ |
| procs / session | 3 | 3 | 0 | == 3 | ✓ |
| `synaps` threads idle | 36 | **16** | −20 | ≤ 20 | ✓ |
| Startup to `○ ready` | 49 ms | 50 ms | +1 | ≤ 80 | ✓ |
| `status --memory` vs `mem.sh` PSS (N=1, 3 runs) | — | 1.8 / 1.9 / 1.9 % | | ≤ 2 % | ✓ |

Live-model gates (`scripts/memprof/bench-turns.sh`, profile `bench` =
`claude-haiku-4-5`, `SYNAPS_MEM_TRACE=1`, new binary only):

| Scenario | Measured | Gate | ✓ |
|---|---|---|---|
| Turn growth: RssAnon after boot → after 5 × "reply ok" (+10 s settle) | 22.56 → 23.17 MB = **+0.60 MB** (per-turn: 23.3 / 23.1 / 23.5 / 24.0 / 23.3) | ≤ +4.5 MB | ✓ |
| S7: 3 × `subagent_start` in parallel — `set_global_broker` calls in-process | **1** (`broker_installs=1` on every `turn memory` line; 0 new install lines during the turn) | == 1 | ✓ |
| S7: `synaps` peak RSS / threads during the subagent turn | 44.3 MB / 22 threads → back to RssAnon 21.0 MB / 16 threads | informational | |

## Where the budget comes from (Phase 1 changes)

| Change | Expected | Switch |
|---|---:|---|
| Log-appender buffer 128 000 → 16 384 lines (`agent-core/src/core/logging.rs`) | −3.9 MB anon / process | `SYNAPS_LOG_BUFFER_LINES=128000` |
| Tokio workers `ncpu` → `min(4, ncpu)` (`src/main.rs`) | −1…−4 MB (stacks + jemalloc tcaches) | `SYNAPS_WORKER_THREADS=0` (ncpu) or `=n` |
| Drop derived request body after serialisation (`runtime/api.rs`) | 0 idle; less transient during long-session turns | none (lifetime only) |
| Subagents spawn from `EngineHost` (shared creds/token cache/cached worker registry; own HTTP client — hyper parks connection drivers on the worker's throwaway runtime, so a shared pool would hand dying connections to the foreground) | ~1–2 MB per live subagent; no token-cache eviction per spawn | `SYNAPS_SUBAGENT_FRESH_RUNTIME=1` |
| `SessionToolSet` round-top rebuild carries forward activations | 0 | `SYNAPS_TOOLSET_CARRY_FORWARD=0` |
| `write_config_value` fs4 lock | 0 | `SYNAPS_CONFIG_LOCK=0` |

## Kill-switches (all env vars)

```
SYNAPS_TOOLSET_CARRY_FORWARD=0     round-top rebuild resets activations (pre-engine-host behaviour)
SYNAPS_SUBAGENT_FRESH_RUNTIME=1    subagents build Runtime::new() + re-install broker (old path)
SYNAPS_WORKER_THREADS=<n|0>        tokio async workers; 0 = one per core (old default)
SYNAPS_LOG_BUFFER_LINES=128000     old tracing-appender buffer
SYNAPS_CONFIG_LOCK=0               skip fs4 lock in write_config_value
```

Phase 4 — thin client (`synaps --attach`), PLAN-phase4-client §8.5. Rows are
"(phase 4, pending)" until the owning package lands; P4-0 rows are live.

| Env | Default | Effect | Status |
|---|---|---|---|
| `SYNAPS_CLIENT_HISTORY=full\|digest` | `digest` (B7) | `full` restores the 741b6b60 mirror + `Query{Messages}` resync + `MessageHistory` forwarding | phase 4 B1–B7 |
| `SYNAPS_ATTACH_TAIL_ITEMS` | 120 | display items in `Attached.display_tail` / `/resync` | phase 4 B7 |
| `SYNAPS_TUI_SCROLLBACK` / `SYNAPS_TUI_SCROLLBACK_BYTES` | Socket 400 / 2 MiB; Local 0 / 0 | 0 = unbounded; drain past cap+64 msgs / cap+256 KiB, one sentinel line, `/resync` reloads | phase 4 B6 |
| `SYNAPS_CLIENT_MALLOC=off` | on | skip bg-thread/decay/tcache mallctls | (phase 4, pending A) |
| `SYNAPS_CLIENT_PURGE_SECS` | 10 | idle purge delay; 0 disables | (phase 4, pending A) |
| `SYNAPS_CLIENT_TCACHE=0` | 1 | disable main-thread tcache (fallback) | (phase 4, pending A) |
| `SYNAPS_CLIENT_REEXEC_MALLOC=1` | off | re-exec with `_RJEM_MALLOC_CONF=narenas:1,…` (fallback) | (phase 4, pending A) |
| `SYNAPS_CLIENT_SIGNAL_THREAD=1` | off | keep the signal-hook thread on the socket client | (phase 4, pending A) |
| `SYNAPS_MEMPROF_PURGE=1` | off | purge on every `Idle` immediately (bench) | (phase 4, pending A) |
| `SYNAPS_MEM_TRACE=1`, `SYNAPS_MEM_TRACE_FILE` | off; `${XDG_RUNTIME_DIR:-/tmp}/synaps-memtrace-<pid>.log` | boot ladder (`memstat::ladder`) | sink + `http` stage (P4-0); remaining stages (phase 4, pending A) |
| `SYNAPS_CLIENT_HTTP=eager` | lazy | build the reqwest client at boot (bisect aid) | live (P4-0) |
| `SYNAPS_TUI_SYNTECT=full` | curated | full default `SyntaxSet` | (phase 4, pending C) |
| `SYNAPS_TUI_SYNTECT_IDLE_SECS` | 120 | 0 = never evict | (phase 4, pending C) |

## Observability

- `synaps status --memory [--json] [--pid N]` — per-session process trees
  (engine / `ext:<name>` / `mcp:<name>` / shell) with RSS/PSS/USS/RssAnon/threads
  and totals; reads the run registry (`~/.synaps-cli/run/*.json`). Linux only.
- `agent_core::memstat::self_snapshot()` — in-process RssAnon + jemalloc
  `allocated/active/resident/retained`; `purge_arenas()` is the manual purge.
- `SYNAPS_MEM_TRACE=1` — `agent_core::memstat::log_turn_memory()` emits one
  `agent_core::memstat` info line ("turn memory": RssAnon, jemalloc, threads,
  `broker_installs`) at every `SessionEvent::Done`, and `set_global_broker`
  logs "global broker installed" with its running count. Off: one atomic
  load per turn.
- `tests/memory_baseline.rs` (`cargo test --test memory_baseline -- --ignored`)
  prints the current process' numbers; informational, never gates.

## Rules

- Any change that adds a long-lived per-process or per-session allocation
  > 1 MB updates the table above in the same PR.
- The gates are re-run on bella before merging anything that touches
  `logging.rs`, `main.rs` runtime construction, `host.rs`, or subagent spawn.
