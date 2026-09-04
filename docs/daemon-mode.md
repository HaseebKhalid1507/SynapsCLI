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

## Protocol summary (line-JSON over UDS, `DAEMON_MAX_FRAME_BYTES` = 64 MiB, same framing as `synaps rpc`)

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
- `WireSessionEvent` mirrors `SessionEventWire` variant-for-variant (`StreamEvent`/`TurnError`/
  `ExtensionLoaderEvent` included; round-trip test covers every variant), with **one lossy variant**:
  `Conversation` goes over the wire as a `ConversationDigest` `{messages_len, messages_hash (FNV-1a),
  tokens, cost, abort_context, queued_message, pending_events_len, consecutive_auto_turns}` — never the
  messages. Full `api_messages` travel only in `Attached` and `QueryResult{Messages}`. `SocketTransport`
  keeps a local mirror (seeded by `Attached`, updated by `Stream(MessageHistory)`), fills the digest in
  when the hash matches, and on a miss (compaction, abort repair, flushed events) issues one
  `Query{Messages}` under the reserved id `DIGEST_RESYNC_QUERY_ID` (= 2^63) and re-emits the
  `Conversation` with the fetched history. Per-event wire cost is O(1) in history size; the O(history)
  cost is paid once per attach and once per tool round (`MessageHistory`, which the engine emits anyway).
- **Frame cap: 64 MiB both directions**, distinct from rpc's 1 MiB (an `Attached` for a long session is
  several MiB). Enforced symmetrically: `encode_line` refuses to build an oversize frame (the daemon sends
  `Error{"daemon could not encode a frame: …"}` and closes), both readers are `take()`-bounded and reply
  `Error{"frame exceeds 64 MiB limit"}` + close. Tested: a > 1 MiB history attaches
  (`attach_to_session_with_history_over_1mib`), a 2 MiB `ping` is answered, a 64 MiB + 10 B frame is refused.
- Query ids ≥ `RESERVED_QUERY_ID_BASE` (2^63) belong to the transport/daemon (`DIGEST_RESYNC_QUERY_ID`,
  `IDLE_PROBE_QUERY_ID`); `SocketTransport` never surfaces them as `QueryResult`.
- First frame not `Hello` / malformed → `Refused{Protocol}`; oversize frame → `Error` + close;
  `Cmd` before `Attach` → `Error`, connection stays open.
- **Client command whitelist** (`conn.rs::client_may_send`): an attached client may send `Submit`,
  `Steer`, `Cancel`, `Answer`, `Set`, `Compact`, `NewSession`, `Save`, `Query`, `EngineCommand`, and
  `Detach` **for its own client id only**. `Detach{other}`, `Attach`, `Resync`, `End`, `HostEvent` →
  `Error{"refused: …"}` and nothing reaches the actor. Sessions are ended by `synaps daemon stop`, not
  by clients. This is a footgun guard on a same-uid 0600 socket, not an auth boundary
  (`client_commands_are_whitelisted`).
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
- **The daemon trusts its uid.** Anyone who can open the socket can `shutdown`, `Attach::Create` with any
  `SessionConfig` (`prompt_manifest`, `auto_approve_confirms`, `persist:false`), and `Cancel`/`Compact`
  a session someone else is attached to. That is the same trust as the shell that started the daemon.

## Lifecycle

- Extension discovery runs **once** per daemon before accept (bounded 10 s) — sidecars are per daemon.
- Daemon SIGTERM/SIGINT/`Shutdown` → `End{HostShutdown}` to every session **concurrently** under ONE
  `TEARDOWN_TIMEOUT` (SAVE + HOOKS) budget, then `ExtensionManager::shutdown_all`, then unlink files.
  Attached clients see `Ended` before `Bye`.
- `--idle-exit SECS`: exit after SECS of **zero connections and no session running a turn** (default:
  never). Sessions never end on their own (no `Parked` yet), so the monitor probes each client-less
  session with `Query{Status}` (reserved id `IDLE_PROBE_QUERY_ID`) and treats `streaming:true`, a pending
  prompt, or **no answer within 2 s** (actor inside `compact()`/preflight, queue full) as busy — the
  daemon never exits under a running turn and never aborts one. Idle client-less sessions are ended
  through the normal `End{HostShutdown}` path (saved to `sessions/<id>.json`; `synaps attach --continue`
  brings them back). Tested: `idle_exit_counts_clientless_idle_sessions_and_never_a_running_turn`.
- **Extension notification router** (`extensions::notify_router`) is spawned by `run_foreground` after
  discovery: every sidecar's `widget.*` frames fan out to **every** live session as
  `ExtensionNotification`. Frames carry no session id, so **widgets are daemon-global under
  `SYNAPS_DAEMON=1`** (a widget upsert from work in session A shows in session B's client). Per-session
  routing needs `params.session_id` from the extension contract — day 2; `heartbeat`/`jawz-widget` are
  last-writer-wins until then (`docs/extensions/session-id.md`).
- **Compaction is inline in the actor**: `Attach`/`Detach`/`Cancel` wait behind a running `compact()`;
  `SocketTransport::attach` gives up after `ATTACH_TIMEOUT` (5 s) with "attach timed out" — retry.
  Spawned compaction is day 2.
- `daemon stop` while a turn is streaming cancels it **and captures an abort context** (`finish()` →
  `cancel_turn()`), so the session is "aborted" on disk; the TUI's quit-mid-turn only cancels the token.
  Defensible (the next `--continue` tells the model the previous answer was cut), documented here.
- Refuse-to-start (exit 3): flag unset; legacy MCP conflict (above); another daemon holds the lock.

## cwd caveats (risk §6.1)

Tools, shell and the memory tool honour the session `cwd` (`Runtime.cwd`). Still process-wide in the
daemon today: memory project scope, `host_project_root`, project-local plugin discovery, extension
process cwd. Start the daemon in the project you care about until day 2's `memory_project_scope(cwd)`.

## What changes on the default (in-process) path — read before merging

Honest list of behaviour changes on this branch with `SYNAPS_DAEMON` **unset**:

1. **`synaps chat` runs on `SessionActor`** (`LocalTransport`, in-process). The differential test
   (`tests/session_actor_differential.rs`) compares the actor against a *frozen re-derivation* of the
   inline engine halves (not a verbatim copy; it has no abort/prompt/save) on **three scenarios only**:
   plain turn, provider error repairing history, idle auto-turn to cap. **Tool loop, steer-mid-stream,
   queue-while-busy, cancel/abort-context, secret-prompt round-trip are asserted by actor unit tests, not
   by the differential.** `tests/chat_stdin.rs` is unchanged and green but never runs a turn through a
   stub. The kill-switch `SYNAPS_CHAT_INLINE=1` only exists in a `--features legacy_inline` build.
   One byte-level difference: **chat's abort context is now `"{ctx}\n\n{msg}"` (context first, wrapper
   applied once — the TUI shape)** where inline chat built `"{msg}\n\n[ABORT CONTEXT…{ctx}…]"` and
   re-wrapped; only visible on `synaps chat --continue` of an aborted session.
2. **MCP descriptor cache write-back is ON by default, in-process too** (`docs/mcp.md`): after the first
   `tools/list` on an exact lease the listing is written to `~/.synaps-cli/mcp-descriptors.json`, so the
   *next* boot registers dormant MCP tools it did not know before → tool list, system prompt and the
   provider prompt-cache prefix differ between run 1 and run 2 for every MCP user. Intended (the cache
   finally has a writer), but it is a default-path change. `SYNAPS_MCP_CACHE_WRITEBACK=0` restores the
   old read-only behaviour.
3. Hook events carry `session_id` (additive; `SYNAPS_HOOK_SESSION_ID=0`).

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

### Fix round: daemon-tree RssAnon column (§9 of the phase-2 review)

`bench-sessions.sh` DAEMON=1 now prints `daemon_anon` (Σ RssAnon over the daemon tree) and
`anon_marginal` (= daemon-side RssAnon cost of one idle session — the number the ≤ 15 MB gate was
written for; PSS "≈ 0 marginal" above was a sharing artefact). bella, 2026-09-04, release @ fix round,
REPEAT=1, real actors, same 2 sidecars. `before` = multi-thread attach client, `after` = current-thread
attach client (commit "attach client diet"):

| binary | settle | N | PSS total | daemon tree PSS | daemon_anon (tree) | daemon proc RssAnon | **anon_marginal / session** | attach client RssAnon | client threads |
|---|---|---|---|---|---|---|---|---|---|
| before | 8 s | 1 | 111.1 MB | 85.8 MB | 36.4 MB | 23.6 MB | – | 21.4 MB | 10 |
| before | 8 s | 3 | 128.9 MB | 82.7 MB | 34.9 MB | 22.0 MB | **−0.7 MB** | 19.1 / 19.1 / 1.7 MB | 10 |
| before | 15 s | 1 | 82.3 MB | 61.6 MB | 18.5 MB | 7.4 MB | – | 1.7 MB | 10 |
| before | 15 s | 3 | 70.3 MB | 61.9 MB | 20.6 MB | 7.4 MB | **+1.0 MB** | 1.7 / 1.7 / 1.8 MB | 10 |
| after | 8 s | 1 | 98.8 MB | 83.9 MB | 34.0 MB | ~21 MB | – | ~11 MB | 4 |
| after | 8 s | 3 | 77.8 MB | 66.6 MB | 18.6 MB | 5.9 MB | **−7.7 MB** | 1.4 / 1.4 / 1.4 MB | 4 |
| after | 15 s | 1 | 91.8 MB | 78.2 MB | 33.0 MB | 20.6 MB | – | 11.1 MB | 4 |
| after | 15 s | 3 | 116.2 MB | 77.4 MB | 33.5 MB | 20.6 MB | **+0.25 MB** | 11.1 / 11.1 / 13.1 MB | 4 |

Reading, honestly:
- **Daemon-side RssAnon per idle session is ≈ 0–1 MB** (−0.7, +1.0, −7.7, +0.25 MB across the four
  pairs). A `SessionActor` + `Runtime` + `ConversationState` with no turn is small; the registries are
  `Arc`-shared. The ≤ 15 MB daemon-side gate passes with a wide margin. This is the number the gate was
  about; it was simply unmeasured before.
- **Everything else in this table is jemalloc purge timing, not code.** The daemon process swings
  7 → 23 MB and the *same* attach binary reads 1.7 MB or 19 MB depending on whether jemalloc's background
  thread (sleeps up to 10 s when idle) has purged the startup garbage before the sample. An 8 s or 15 s
  settle lands on either side of that window at random. `bench-turns.sh`-style double settle (> 20 s) or
  a `mallctl("arena.<i>.purge")` before sampling is the day-2 fix for the *script*.
- **Client diet**: the current-thread runtime for `synaps attach` cuts threads 10 → 4 and removes the
  worker pool; its RssAnon effect cannot be resolved under the purge noise above (both binaries bottom
  out at 1.4–1.8 MB once purged, i.e. the post-purge client is already tiny — the 21 MB PSS headline is
  mostly shared text of the 22 MB binary). Remaining day-2 diet items: skip `EngineHost`/config/skills
  statics on the attach path (nothing in `main.rs` boots them before dispatch today — verified), and
  `narenas:1,background_thread:false` for the client via a per-command `MALLOC_CONF` (needs the static
  conf to move to a runtime `mallctl`).
- procs/session == 1.00 and daemon procs == 3 held in all eight runs.

Raw runs: `/tmp/memprof-synaps-fb-{before,after}-daemon-N<N>-r1.txt` on bella (the 15 s runs
overwrote the 8 s ones for the same N; the 8 s values above are transcribed from the earlier console).

## Not landed today (day 2/3)

`--attach` driving the TUI over `SocketTransport` (needs A4); `Resync` after
`Lagged` via `turn_replay`; delta coalescing; `--tcp`; HTTP+SSE front-end; `prompt_over_wire` and
`detach_without_abort_over_socket` integration tests (need a secret-prompting tool fixture + `Endless`
script through the actor — the paths are exercised by the actor unit tests and the socket tests above).
