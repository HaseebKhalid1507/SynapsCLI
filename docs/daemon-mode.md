# Daemon mode (`synaps daemon` / `synaps attach`) — phase 2, package B

**Status:** experimental, off by default. Everything here is gated by `SYNAPS_DAEMON=1`.
Protocol v1 (`crates/agent-engine/src/session/wire.rs`), daemon in `agent-engine::daemon`
(no `agent-tui` in its graph — `tests/daemon_no_tui_dep.rs`).

## Flags and kill-switches

| Env | Effect |
|---|---|
| `SYNAPS_DAEMON=1` | Required for `synaps daemon`, `synaps attach`, and `--attach`. Unset → exit 3 with a one-line reason (`--attach` prints a notice and runs the normal TUI). |
| `SYNAPS_DAEMON_ALLOW_LEGACY_MCP=1` | Allow start with `progressive_tool_disclosure=false` **and** MCP servers configured (legacy `McpTool` connections would be shared across sessions). Same as `--allow-legacy-mcp`. |
| `SYNAPS_RUNTIME_DIR` | Where the socket/lock/json/pid live (default `~/.synaps-cli/run`, 0700). |
| `SYNAPS_SESSION_EVENTS_CAP` | Per-session broadcast capacity (default 1024). A slow client gets `SystemNotice("event stream lagged; n dropped")`. |
| `SYNAPS_DAEMON_READY_FD` | Internal: write end of the ready pipe handed to a `--detach`ed child. Scrubbed from the env before accept. |

## CLI

```
synaps daemon [--foreground|--detach] [--socket PATH] [--idle-exit SECS] [--allow-legacy-mcp] [--profile P]
synaps daemon status [--json]        # daemon.json + flock probe + Ping → state/pid/uptime/sessions (exit 1 if not answering)
synaps daemon stop [--force]         # Shutdown{force} over the socket, wait for the flock; --force escalates SIGTERM (10 s) → SIGKILL (+5 s)
synaps daemon sessions [--json]
synaps attach [ID] [--create] [--continue NAME_OR_ID] [-s PROMPT]
synaps --attach [ID]                 # today: notice + routes to `synaps attach` (daemon-attached TUI is day 2)
```

`--detach` forks `current_exe daemon --foreground` under `setsid`, stdout → null, stderr → pipe,
and waits ≤ 5 s for `R` on an anonymous ready pipe (EOF before `R` = child died → error with its
stderr tail). Measured on bella: ready in ~75 ms.

`synaps attach` is a thin line client: stdin lines → `Submit` (or `Steer` while streaming);
`/abort` → `Cancel`; `/detach`, `/quit` or **Ctrl-C → `Detach` + `Bye` — the turn keeps running**;
`/model NAME`; `/cmd NAME [ARG]` (engine command); `/save`; `/new`; `/sessions` (status query).
Prompts render as `[prompt #id] title: prompt > `; `Secret` prompts turn terminal echo off.
With no ID: attaches to the single live session, creates one if none, lists if several.

## Files (under `registry_dir()`, one set per profile: `daemon-<P>.*`)

| File | Mode | Purpose |
|---|---|---|
| `daemon.sock` | 0600 | UDS listener (symlinks refused on cleanup) |
| `daemon.lock` | 0600 | **flock = liveness oracle.** Alive iff someone holds it. Nobody unlinks on ECONNREFUSED alone; `reap_stale` unlinks sock/json/pid only when the lock is free. A second daemon on the same paths is refused. |
| `daemon.json` | 0600 | `{pid, protocol_version, daemon_version, profile, started_at, socket}` — never credentials |
| `daemon.pid` | 0600 | pid |

## Protocol summary (line-JSON over UDS, `MAX_FRAME_BYTES` = 1 MiB, same framing as `synaps rpc`)

```
C: {"type":"hello", protocol_version, client:{kind,terminal,instance}, cwd, client_version}   ← MUST be first
D: {"type":"welcome", protocol_version, daemon_version, pid, profile, sessions:[SessionMeta], progressive_tool_disclosure}
   | {"type":"refused", reason:{reason:"version",daemon_version,min,max}|"protocol"|…, message}   → daemon closes
C: ping | sessions | shutdown{force}          → D: pong{pid,uptime_s,sessions} | session_list{sessions} | bye   (no session allocated)
C: {"type":"attach", attach:"existing", session_id, mode} | {"type":"attach", attach:"create", config:SessionConfig, mode}
D: {"type":"attached", client, meta, view, conversation, streaming, replay:[WireEnvelope], pending_prompts, clients} | error
C: {"type":"cmd", session_id, cmd:SessionCommand} …      → D: {"type":"event", session_id, seq, ts, event:WireSessionEvent} …
C: bye | socket close = Detach (turn keeps running)
```

- Version policy: exact match on `protocol_version` (min == max == 1) → `Refused{Version}` **loudly**
  (client exit 2). Different *binary* version, same protocol → allowed; `attach` prints a notice.
  `WireSessionEvent` has `#[serde(other)] Unknown` so additive variants from a newer daemon are tolerated.
- `WireSessionEvent` is a lossless mirror of `SessionEventWire` (`StreamEvent`/`TurnError`/
  `ExtensionLoaderEvent` mirrored variant-for-variant; round-trip test covers every variant).
  Conversion happens only at the socket boundary; in-process transports never serialise.
- First frame not `Hello` / malformed → `Refused{Protocol}`; oversize frame → `Error` + close;
  `Cmd` before `Attach` → `Error`, connection stays open.
- `Attach::Create` fills `cwd` from `Hello.cwd`; it must be absolute and exist.

## Security posture

- Socket 0600 in a 0700 dir; same trust domain as every existing per-session socket.
- **No secret ever crosses daemon → client.** `Welcome`/`Attached` carry summaries only (grep-tested for
  token/secret/key names). Client → daemon `Answer.value` (e.g. a sudo password) is forwarded to the actor
  and nowhere else: `ClientFrame`'s `Debug` redacts `Answer`/`Submit`/`Steer` bodies, and
  `tests/daemon_no_tui_dep.rs::daemon_never_traces_answer_bodies` greps the daemon + socket transport
  sources for any `tracing::` line mentioning `answer`/`value`.
- Backpressure, never silent drops: bounded writer queues both ways (`CMD_CHAN_CAP` = 256);
  `send` returns `TransportError::Backpressure`; the daemon replies `Error{"backpressure…"}`.

## Lifecycle

- Extension discovery runs **once** per daemon before accept (bounded 10 s) — sidecars are per daemon.
- Daemon SIGTERM/SIGINT/`Shutdown` → `End{HostShutdown}` to every session **concurrently** under ONE
  `TEARDOWN_TIMEOUT` (SAVE + HOOKS) budget, then `ExtensionManager::shutdown_all`, then unlink files.
  Attached clients see `Ended` before `Bye`.
- `--idle-exit SECS`: exit after SECS with zero connections **and** zero live sessions (default: never).
- Refuse-to-start (exit 3): flag unset; legacy MCP conflict (above); another daemon holds the lock.

## cwd caveats (risk §6.1)

Tools, shell and the memory tool honour the session `cwd` (`Runtime.cwd`). Still process-wide in the
daemon today: memory project scope, `host_project_root`, project-local plugin discovery, extension
process cwd. Start the daemon in the project you care about until day 2's `memory_project_scope(cwd)`.

## Memory acceptance — `DAEMON=1 SYNAPS_DAEMON=1 scripts/memprof/bench-sessions.sh BIN 1 2 3`

bella, 2026-09-04, release build @ B6, **real `SessionActor` sessions** (A merged), 2 sidecars
(synaps-chronos, munder-hive-god), settle 8 s, median of 3:

| N | PSS total (daemon tree ∪ clients) | daemon tree PSS | daemon process PSS | marginal PSS | procs beyond daemon / session | daemon procs | attach ready |
|---|---|---|---|---|---|---|---|
| 1 | 108.1 MB | 85.0 MB | 31.5 MB | – | 1.00 | 3 | 47 ms |
| 2 | 128.1 MB | 82.2 MB | ~28 MB | 19.9 MB | 1.00 | 3 | 46 ms |
| 3 | 145.6 MB | 81.8 MB | 27.5 MB | 17.6 MB | 1.00 | 3 | 46 ms |

Reading:
- **procs/session == 1** (the attach client) and **daemon procs constant (3)** across N: sidecars spawn
  once per daemon, not per session. Gates pass.
- **Daemon-side marginal cost per idle session ≈ 0 MB** (daemon tree PSS is flat/slightly down 85 → 82 MB
  as text pages get shared with more clients).
- **Marginal PSS as measured (17.6–19.9 MB) is above the 15 MB gate**, and it is entirely the
  `synaps attach` client process (PSS ≈ 21.5 MB, RssAnon ≈ 19.3 MB each, 10 threads): it is the full
  22 MB `synaps` binary with jemalloc + a tokio runtime. The plan lists the attach client RSS as
  informational ("≪ 40 MB"); the ≤ 15 MB gate was written for actor/session allocations in the daemon,
  which are within noise here. Cutting the client below 15 MB is a client-diet task (single-thread runtime,
  no jemalloc arenas, lazy statics), not a daemon one.
- Compare in-process (Phase 1 gates): PSS(N=3) ≤ 160 MB with 3 procs/session → daemon mode at 145.6 MB
  with 1 proc/session, and every additional session costs a 20 MB thin client instead of a full engine.

Raw runs: `/tmp/memprof-synaps-b-daemon-N<N>-r<i>.txt` on bella.

## Not landed today (day 2/3)

`--attach` driving the TUI over `SocketTransport` (needs A4); `--continue X` attach-if-live; `Resync` after
`Lagged` via `turn_replay`; delta coalescing; `--tcp`; HTTP+SSE front-end; `prompt_over_wire` and
`detach_without_abort_over_socket` integration tests (need a secret-prompting tool fixture + `Endless`
script through the actor — the paths are exercised by the actor unit tests and the socket tests above).
