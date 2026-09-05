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

Phase 4 — thin client (`synaps --attach` / `synaps attach`, requires
`SYNAPS_DAEMON=1`; without it `--attach` boots the ordinary in-process TUI
and **none** of the rows below apply). Every row is live; defaults are what
`client_diet.rs`, `main.rs`, `app.rs`, `signals.rs` and `highlight.rs` read.

| Env | Default | Effect |
|---|---|---|
| `SYNAPS_CLIENT_REEXEC=0` | on | Skip the one-time self re-exec. The re-exec runs only when `/sys/kernel/mm/transparent_hugepage/enabled` is `[always]` **and** THP is not already off for the process: it sets `PR_SET_THP_DISABLE` (inherited across `execve`, so `.bss`/stack/first jemalloc chunks map at 4 KiB instead of 2 MiB — 6 of the pre-diet 7.4 MB at `main`) and boots jemalloc with `narenas:1,background_thread:false,dirty_decay_ms:0,muzzy_decay_ms:0` via `_RJEM_MALLOC_CONF`. Cost 3–5 ms (`reexec` ladder stage). On `[madvise]`/`[never]` kernels it is skipped (`reexec skipped=thp-not-always`) and the mallctls below do the rest in-process; only `narenas` (boot-only) is then left at the binary's 4. |
| `SYNAPS_CLIENT_MALLOC_CONF` | the string above | Replaces our part of the re-exec conf. A user-exported `_RJEM_MALLOC_CONF` is **kept**: the child gets `user,ours` (later keys win) and the user's value is restored in the environment afterwards, so children the client spawns (`tmux`, plugin commands) see exactly what the user set. |
| `SYNAPS_CLIENT_THP=1` | off | Keep transparent huge pages (also skips the re-exec). |
| `SYNAPS_CLIENT_ALLOC=default` (alias `SYNAPS_CLIENT_MALLOC=off`) | tuned | Skip every allocator knob: no re-exec, no `PR_SET_THP_DISABLE`, no bg-thread/decay/tcache mallctls (`alloc skipped=1`). |
| `SYNAPS_CLIENT_TCACHE=0` | on | Disable the main thread's tcache (fallback probe). |
| `SYNAPS_CLIENT_PURGE_IDLE_SECS` (alias `SYNAPS_CLIENT_PURGE_SECS`) | 10 | Seconds after the client goes idle before `arena.<all>.purge` (`idle+N` → `purged` ladder lines), then every 30 s while idle. **0 disables the whole idle arm** — the purge *and* the syntect eviction check (`SYNAPS_TUI_SYNTECT_IDLE_SECS` then never fires). |
| `SYNAPS_MEMPROF_PURGE=1` | off | Purge immediately on every idle (bench/ladder aid). |
| `SYNAPS_CLIENT_HISTORY=full` | `digest` | Restores the **history mirror** only: the client keeps a copy of `api_messages`, receives `MessageHistory` frames and resyncs with `Query{Messages}`. It does *not* undo the rest of the diet — the scrollback cap, re-exec, allocator tuning, tokio signals and syntect eviction stay on (`client_bounded` fails under it: G6 slope 2.49 MB). |
| `SYNAPS_ATTACH_TAIL_ITEMS` | 120 | Display items the daemon projects into `Attached.display_tail` and on `/resync` (user text, assistant thinking/text/tool_use — **no tool output**). |
| `SYNAPS_TUI_SCROLLBACK` / `SYNAPS_TUI_SCROLLBACK_BYTES` (aliases `SYNAPS_CLIENT_SCROLLBACK_MSGS` / `_BYTES`) | Socket 400 / 2 MiB; Local 0 / 0 | 0 = unbounded. Drain once past cap+64 msgs or cap+256 KiB (audited every 64 pushes or 256 KiB pushed); one sentinel line naming what `/resync` reloads. 2 MiB ≈ 70 tool outputs at `max_tool_output` 30 000 B. |
| `SYNAPS_TUI_SYNTECT=full` | curated | Full default `SyntaxSet` (75 grammars) instead of the 26-grammar curated dump. Saves ~180 KB of dump bytes only — see the table below. |
| `SYNAPS_TUI_SYNTECT_IDLE_SECS` | 120 | Drop the syntect state (grammars, compiled regexes, theme) after N s without a highlight; 0 = never. Applies to the in-process TUI too (same idle arm; re-highlight after eviction costs one 20–25 ms load). |
| `SYNAPS_TUI_SYNTECT_REPORT=1` | off | Build-time: `build.rs` prints the curated/full dump sizes as a `cargo:warning`. |
| `SYNAPS_CLIENT_SIGNAL_THREAD=1` | off | Socket client handles SIGTERM/SIGHUP/SIGINT on tokio's signal driver (no `signal-listener` thread); `=1` keeps the signal-hook thread. |
| `SYNAPS_CLIENT_HTTP=eager` | lazy | Build the reqwest client at boot (bisect aid; the `http` ladder stage reappears). |
| `SYNAPS_MEM_TRACE=1`, `SYNAPS_MEM_TRACE_FILE` | off; `${XDG_RUNTIME_DIR:-/tmp}/synaps-memtrace-<pid>.log` | Boot ladder (`memstat::ladder`): `main reexec alloc runtime attach:enter config app attached purge:attached terminal render_thread event_stream hl_first first_frame idle+N purged detach`. |

### Phase 4 — what the thin client costs (bella, THP=`[always]`, release @ c8a7e030, median of 3)

| | before (741b6b60 client path) | after |
|---|---|---|
| RssAnon idle, empty session, post-purge (G1) | 19.35 MB (18.4 MB of it huge pages), 4.10 MB after jemalloc's own purge | **2.27 MB** |
| RssAnon idle, 20 MB history (G2) | 78.5 MB | **2.38 MB** (`bounded_delta` 0.11 MB) |
| threads at idle (G3) | 8 (`jemalloc_bg_thd`×4, `signal-listener`, render, main×2) | **3** (main ×2 + render; no jemalloc bg threads, no signal thread) |
| first frame (G4) | attach_ms 47 | **7–10 ms** to `first_frame`, attach_ms 46–48 |
| all-in marginal per extra session, N=2→3 (G5) | 17.6 MB | **3.18 MB** (daemon anon 0.91 + client 2.27) |
| RssAnon after 80 tool turns (G6) | grows unbounded | capped by the 2 MiB scrollback (slope ≤ 1.5 MB over turns 30→80, max ≤ 14 MB) |

**With code on screen the number is not 2.3 MB.** One rendered code block
(any language) compiles that grammar's fancy-regex programs: `first_frame`
+11.2 MB RssAnon → the client idles at **≈ 13.7 MB** until the syntect idle
eviction fires (`SYNAPS_TUI_SYNTECT_IDLE_SECS`, default 120 s), then
**≈ 4.9 MB** (the residual is transcript + pane buffers). Each further
language rendered adds ~8–10 MB until eviction. The curated dump (C1) saves
dump *bytes* (163 vs 341 KB) and nothing on the heap — the expected
−1.5…−3 MB was not achieved; C2's eviction is what returns the memory.

### Phase 4 C — syntect (bella, release, `tests/highlight_mem.rs` jemalloc-accounted, 3 runs each)

| | curated (C1) | full (`SYNAPS_TUI_SYNTECT=full`) |
|---|---|---|
| dump bytes embedded / copied at load | 163 KB (26 grammars) | 341 KB (75 grammars) |
| `SyntaxSet` load (`hl_first load_ms=`) | 0 ms | 0 ms |
| first Rust highlight (contexts + regex compile) | 19–24 ms (32 ms via `highlight_code_block` incl. theme) | 20–21 ms |
| reload after eviction, first highlight | 22–26 ms | — |
| jemalloc `allocated` after first Rust fence | +9.8–10.6 MB | +10.0–10.4 MB |
| after 10 languages (rs py js go sh json yaml md diff sql) | +83 MB | +83 MB |
| returned by `evict_if_idle` (C2) | −83 MB allocated | −83 MB allocated |
| `ThemeSet::load_defaults()` retained pre-C3 vs one `Theme` (C3) | 67 KB → one `Theme` (dropped with the set on eviction) | |

Reading: syntect deserialises contexts and compiles regexes lazily *per grammar
touched*, so the curated dump (C1) only saves the ~180 KB of dump bytes and the
first-fence latency is unchanged — the heap is fancy-regex compiled programs,
~8–10 MB **per language rendered**, monotone until dropped. C2's eviction is
what returns it (83 MB after 10 languages). RssAnon follows `allocated` only
after a purge (the idle arm); the numbers above are jemalloc `stats.allocated`,
the real-client RssAnon step is +11.2 MB (table above).
Gate G10: `hl_first` ≤ 60 ms warm / ≤ 120 ms after eviction — 32 / 26 ms.
Golden: `tests/highlight_curated.rs` — 14 fixtures, curated ≡ full spans;
unknown languages fall back to Plain Text identically in both.

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
