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
| `synaps daemon reload [--now] [--drain-secs N] [--exe PATH]` / `SYNAPS_DAEMON_RELOAD_DRAIN_SECS` (default 30) | Re-exec the daemon in place (C3). `--now` = drain 0. `--exe` overrides (and records) the binary. |
| `SYNAPS_TUI_ATTACH_RECONNECT_SECS` (default 60) | Total `SocketTransport::reconnect` budget after `Reloading`/EOF (backoff 100 ms ×2, cap 5 s). |
| `SYNAPS_DAEMON_RELOAD_STATE` / `SYNAPS_DAEMON_LOCK_FD` | Internal: handed to the new image by `reload`; both scrubbed at start. `RELOAD_STATE` without `LOCK_FD` refuses to start (the flock is the liveness oracle). |
| `synaps daemon purge` / `scripts/memprof/purge.sh` | jemalloc purge in the daemon before an RssAnon sample (C2). `SYNAPS_MEMPROF_PURGE=1` makes the `--attach` client purge on every busy→idle edge (phase 4 A2). |
| `SYNAPS_CLIENT_REEXEC=0`, `SYNAPS_CLIENT_MALLOC=off`, `SYNAPS_CLIENT_THP=1`, `SYNAPS_CLIENT_PURGE_SECS`, `SYNAPS_CLIENT_TCACHE=0`, `SYNAPS_CLIENT_SIGNAL_THREAD=1`, `SYNAPS_MEM_TRACE=1` | Phase 4 A thin-client diet — see `docs/memory-budget.md` §Kill-switches (phase 4 rows) and §"Phase 4 A" for the measured table. |
| `SYNAPS_DAEMON_READY_FD` | Internal: write end of the ready pipe handed to a `--detach`ed child. Scrubbed from the env before accept. |
| `SYNAPS_DAEMON_PARK_GRACE_SECS` (default 60; `never` disables) | B3: seconds after the last detach (idle, no prompts, no compaction, not keep-warm, journal on disk) before a session is **Parked** — `Runtime` + `ConversationState` dropped, restored from the journal on the next attach/turn/`synaps send`. A session that never ran a turn has no journal and never parks. |
| `synaps attach --keep-warm` / `/keep-warm on\|off` / `SessionCommand::KeepWarm` | Pin: never park this session. Survives `daemon reload`. |
| `synaps attach --observe` / `--takeover` | B1 attach modes: `--observe` never owns input (setters/`Submit` are `Refused`); `--takeover` steals ownership from the current owner (who is told via `InputOwnerChanged{Takeover}`); default `Mirror` owns input iff nobody does. |
| `SYNAPS_SESSION_COMPACT_INLINE=1` | One-release kill-switch: run `/compact` and auto-compaction inline on the actor task (the #107 body — `Attach`/`Cancel` wait behind it) instead of the spawned job. Deleted in phase 4. |
| `synaps daemon reload` / `stop` / `purge` from **any** same-uid client | Not a privilege boundary (0600 socket): a stray `synaps daemon reload` from any shell checkpoints every session and closes everyone's PTYs. `Checkpoint` over the wire is owner-only. |

## CLI

```
synaps daemon [--foreground|--detach] [--socket PATH] [--idle-exit SECS] [--allow-legacy-mcp] [--profile P]
synaps daemon status [--json]        # daemon.json + flock probe + Ping → state/pid/uptime/sessions (exit 1 if not answering)
synaps daemon stop [--force]         # Shutdown{force} over the socket, wait for the flock; --force escalates SIGTERM (10 s) → SIGKILL (+5 s)
synaps daemon sessions [--json]
synaps daemon purge                  # Purge frame → memstat::purge_arenas() in the daemon; reply Pong (bench hygiene, C2)
synaps daemon reload [--now] [--drain-secs N] [--exe PATH] [--json]   # re-exec in place, same pid (C3; §Reload below)
synaps attach [ID] [--create] [--continue NAME_OR_ID] [-s PROMPT] [--observe|--takeover] [--keep-warm]
synaps --attach [ID]                 # today: notice + routes to `synaps attach` (daemon-attached TUI is day 2)
```

`--detach` forks `current_exe daemon --foreground` under `setsid`, stdout → null, stderr → pipe,
and waits ≤ 5 s for `R` on an anonymous ready pipe (EOF before `R` = child died → error with its
stderr tail). Measured on bella: ready in ~75 ms.

`synaps attach` is a thin line client: stdin lines → `Submit` (or `Steer` while streaming);
`/abort` → `Cancel`; `/detach`, `/quit` or **Ctrl-C → `Detach` + `Bye` — the turn keeps running**;
`/model NAME`; `/cmd NAME [ARG]` (engine command); `/save`; `/new`; `/sessions` (status query);
`/keep-warm on|off`. Typed events render as `[aborted…]`, `[session cleared → id]`, `[compacting: …]`,
`[compacted N messages]`, `[refused cmd: reason]`, `[input owner → …]`, `[session Parked]`.
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
  discovery: a sidecar's `widget.*` frame goes to the session named by `params.session_id` (dropped with
  a `debug!` if that session is not live), or to **every** live session when the frame carries none
  (daemon-global — the pre-phase-3 behaviour). Plugin contract: `docs/extensions/session-id.md`;
  `heartbeat`/`jawz-widget` stay last-writer-wins until their own repos adopt it.
- **Compaction is inline in the actor**: `Attach`/`Detach`/`Cancel` wait behind a running `compact()`;
  `SocketTransport::attach` gives up after `ATTACH_TIMEOUT` (5 s) with "attach timed out" — retry.
  Spawned compaction is day 2.
- **Quit mid-turn saves an abort context — default path, every host.** `End{ClientQuit}` (chat's
  stdin EOF / `/quit`, the in-process TUI's quit — never `synaps attach`, which only detaches) and `daemon stop` while a turn
  is streaming run `finish()` → `cancel_turn()`: the turn is cancelled **and** the partial output is
  captured as `abort_context` and saved, so the next `--continue` prepends `[ABORT CONTEXT…]` — where the
  pre-actor TUI/chat only cancelled the token. Defensible (the model is told the previous answer was cut);
  `/clear` or a fresh session discards it. Documented in `synaps chat /help` too.
- Refuse-to-start (exit 3): flag unset; legacy MCP conflict (above); another daemon holds the lock.

## Reload (`synaps daemon reload`, phase 3 C3)

Sequence (`daemon/reload.rs`, PLAN-phase3 §2.8), all on the requesting control connection:

1. **Version gate** before anything is disturbed: `<exe> daemon --print-version` (hidden flag, 5 s) must
   succeed, speak protocol ≥ ours, and be **newer or equal** by semver (`RefuseReason::ReloadRefused{why}`
   otherwise; equal allows same-version rebuilds). `exe` = `daemon.json.exe` (argv[0] canonicalised at
   first start — not `/proc/self/exe`, which reads "(deleted)" after an in-place rebuild) or `--exe`.
2. **Drain**: `Attach::Create` → `Refused{Busy}`, `Submit`/`SubmitPrepared`/`Compact` → `Error("daemon
   reloading; retry in a moment")`; wait ≤ `--drain-secs` for every session to be idle; then
   `Checkpoint{Reload}` every session concurrently (≤ `SAVE_TIMEOUT`+1 s each): cancel with abort context,
   answer prompts `None`, save, close PTYs, notice.
3. **reload-state** `daemon[-P].reload.json` (0600): generation, per-session `{id, journal_id, config
   (continue_session = journal_id, cwd, model), keep_warm, lifecycle}`.
4. **Announce**: every other connection gets `Event(Reloading{generation, retry_after_ms: 500})` +
   `Bye{Reloading}` (forwarders drain the checkpoint's events first); the requester gets `Bye{Reloading}`.
5. **Exec**: `daemon.json` rewritten (`generation+1`, same pid), sidecars stopped (≤ 5 s), the flock fd made
   inheritable (`SYNAPS_DAEMON_LOCK_FD`), `execv(exe, original argv)`. The listener is CLOEXEC (asserted in
   `listener::bind`) — closed at exec; the new image rebinds. **Exec failure**: `daemon.json`/reload-state
   restored, `reloading` cleared, the reload-announce token re-armed (new connections are served again),
   extension discovery re-spawned (the sidecars were stopped for the exec — bounded 10 s like boot), old
   image keeps serving (sessions checkpointed but alive; the requester already got `Bye{Reloading}` and
   reconnects to the same generation). `--exe` is validated **before** the probe: canonical path, regular
   file, executable, owned by our uid or root, not group/world-writable; then `--print-version` must parse
   and pass the gate. Tested: `exe_validation_refuses_and_exec_failure_keeps_serving`.
6. **New image**: `Daemon::start` sees `SYNAPS_DAEMON_RELOAD_STATE` → `DaemonLock::adopt(fd)` (same open file
   description = same flock, no gap), no `reap_stale`, rehydrates every recorded session
   (`create_session(continue_session = journal_id)`) **before** accepting; `reload_aliases` maps old→new
   ids for one generation (they differ only after a LinkedSuccessor compaction).
7. **Clients**: `SocketTransport::next_event` returns `None` with `last_error = Reloading{generation}`;
   `reconnect(mode)` backs off, sends `Hello{reconnect_of}` and `Attach::Existing{id, Takeover iff
   was_owner else mode}` — two reconnecting mirrors cannot both take over.

**Not preserved** (stated once): in-flight turns (checkpointed = cancelled with abort context), pending
prompts (`None`), PTY/background shells (closed, announced before exec), `turn_replay`, un-persisted
`TurnLog`, input ownership (re-established by reconnect order + `was_owner`), the subagent registry.
**Preserved** — each session's `Checkpoint{Reload}` reply carries a `SessionReloadRecord` that the new
image rehydrates from: the journal and session id (same journal continued; `reload_aliases` only after a
LinkedSuccessor compaction), the `SessionConfig` as created (cwd, `--system`, prompt manifest,
compaction policy, auto-compact, persist), the keep-warm pin, the lifecycle (**a Parked session comes
back Parked** — rehydrate creates it then sends the host-only `Park`), the non-persisted runtime knobs
(`settings_replay`: `/context`, compaction model, API retries, subagent/bash timeouts, tool-output cap,
worker grants, **`/system`**), and the **current** model/thinking (a `/model` change mid-session
survives; the create-time `--model` override is not re-applied). A session that never ran a turn has no
journal (`save` skips an empty conversation): it is recreated fresh under a new id and aliased.

Tested against a **real** `synaps daemon --foreground` process (`tests/daemon_reload.rs`): same pid before
and after, `generation` 1→2, flock held throughout, conversation identical after reconnect, client is
owner again, second turn works; older `--exe` refused with the daemon still serving; a turn in flight is
checkpointed and its abort context comes back from the journal; `/model` + `/context` + `/system` +
keep-warm + a Parked session survive (`reload_preserves_model_keep_warm_settings_and_parked`).

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
4. **`synaps chat` renders typed events**: `/compact` prints `compacting...`, the disclosure line and
   `[compacted → ~N tokens]` (one each — the actor emits `CompactionStarted/Applied/Failed/Cancelled`,
   never a "compacting..." notice); `/abort`-equivalents render `Aborted{context_saved}`, `/clear` renders
   `Cleared{session_id}`. Quit mid-turn saves an abort context (§Lifecycle above).

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

### Phase 4 A — the thin client (bella, 2026-09-04, release @ A3, `THP=always`)

`scripts/memprof/client-ladder.sh BIN 0` — before (P4-0, external `/proc` sample; no stages yet):
`RssAnon 17.3–19.4 MB, threads 8` (`agent-tui-rende`, `jemalloc_bg_thd`×4, `signal-listener`, `synaps`×2),
unchanged between 7 s and 25 s (the bg thread never purged). A1 ladder on the same code:

```
t_ms  stage          rss_anon      Δ  alloc active resident retained  meta  thr
0     main               6876          109    128     2568     1920  2460    2
…     terminal           6968     +0  1636   1776     4224      272  2469    2
5     render_thread     11092  +4124  1665   1812     4352     2284  2587    4
5     event_stream      11124    +32                                       6
5     first_frame       13180  +2048  1819   1952     4584     2144  2733    6
25908 detach            19356  +6176  2200   2504     5248     5672  2829    8
```

RssAnon moved in 2 MiB steps while jemalloc's own resident barely moved: `THP=always` hands every
touched thread stack (and the `.bss`, the main stack, jemalloc's first chunk) a huge page. After A2
(re-exec with `PR_SET_THP_DISABLE` + `_RJEM_MALLOC_CONF=narenas:1,background_thread:false,
dirty_decay_ms:0`, then bg-threads-off / decay-0 mallctls, purge on `Attached` and 10 s after idle):

```
t_ms  stage          rss_anon      Δ  alloc active resident retained  meta huge thr
0     reexec             6876          109    128     2568     1920  2460 6144   2
0     main                848  -6028   109    128     2456     1920  2349    0   1
0     alloc              1016   +168   thp_off=ok bg_off=ok decay0=ok bg_now=Some(false)
4     attached           1464    +12   messages_len=2 replay=0 tail_items=120
4     terminal           1972   +476   cols=120 rows=40
5     render_thread      1996    +24                                         2
5     event_stream       2064    +68                                         4
6     first_frame        2136    +44  1807   1952     4292       96  2444    0   4
10006 idle+N             2336   +200                                       n=10
10007 purged             2336     +0  2152   2300     4644     2308  2448    0   4
```

`DAEMON=1 CLIENT=1 BOUNDED=1 SETTLE=12 bench-sessions.sh /tmp/synaps-a3 1 2 3` (median of 3):

| N | fixture | client_anon_post | client_anon_pre | retention | threads | first_frame_ms | attach_ms | daemon_anon | anon_marginal | all_in_marginal |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 0 MB | 2.25 MB | 2.25 | 0.00 | 3 | 9 | 46 | 33.2 | – | – |
| 2 | 0 MB | 2.26 MB | 2.26 | 0.00 | 3 | 8.5 | 47 | 37.0 | 3.77 | 6.03 |
| 3 | 0 MB | 2.27 MB | 2.27 | 0.00 | 3 | 10 | 47 | 36.5 | −0.55 | **1.71** |
| 3 | 20 MB | 78.54 MB | 78.54 | 0.00 | 3 | 167 | 201 | 269.4 | – | – |

`bounded_delta = 76.27 MB` (pre-B: the history mirror, PLAN-phase4 §2 — B4/B7 own it).
Gates at A3: G1 ✓ 2.3 MB (≤ 10), G3 ✓ 3 (≤ 4), G4 ✓ 9 ms (≤ 100), G5 ✓ 1.7 MB (≤ 10), G7 ✓ 0.00
(≤ 0.5); G2 ✗ / G6 ✗ pre-B (`client_bounded`: 3.6 → 6.3 MB over 40 tool turns, slope 2.62 MB).
The `anon_marginal` column is daemon-side purge noise (−0.55 … +3.77); the daemon is not on A's
list. Threads at idle: `synaps`×2 (main + crossterm's unnamed reader), `agent-tui-rende`.

**Why THP was invisible until now:** `RssAnon` counts the 2 MiB page the moment one byte of it is
touched; jemalloc's `stats.resident` counts what jemalloc touched. The 8-thread, 19 MB client was
~6 × 2 MiB of thread stacks + bss + arena chunks and ~2 MB of real heap. The same mechanism inflates
the daemon (one huge page per touched thread stack × 26 threads) — not addressed here; a
`PR_SET_THP_DISABLE` before the runtime is the obvious day-2 lever for it.

**§9 asks.** (2) The 2.1 MB `.bss` is jemalloc: `_rjem_je_arena_emap_global` = 0x200078 (2,097,272 B,
the extent radix tree), then `_rjem_je_arenas` 32 KB and crossbeam's `atomic_cell` lock table 8.5 KB —
nothing of ours. Resident only where touched, i.e. one 2 MiB huge page the moment jemalloc boots on a
`THP=always` box (that is the `reexec`→`main` −6 MB above), a few 4 KiB pages after the re-exec.
(1) `cargo bloat --release --crates` (unstripped release, LTO): `.text` 18.1 MiB of a 34.4 MiB
unstripped file — agent_engine 4.0 MiB (22.3 %), std 3.6, agent_tui 1.0, agent_core 1.0, tokio 0.86,
synaps (bin) 0.79, h2 0.65, serde_json 0.44, rustls 0.41, regex_automata 0.39, serde+serde_core 0.64,
ring 0.28, clap 0.23, hyper 0.23, axum 0.22, jemalloc 0.19, reqwest 0.16, syntect 0.07 (+ fancy_regex
0.07). A separate `synaps-attach` binary could drop at most agent_engine's provider/MCP/daemon paths
and axum/h2/rustls/ring (≈ 5–6 MiB of text) — none of which is RssAnon; §5's "no separate binary"
stands: the client's private memory is now 2.3 MB and text is shared with the daemon.

## Not landed today (day 2/3)

`--attach` driving the TUI over `SocketTransport` (needs A4); `Resync` after
`Lagged` via `turn_replay`; delta coalescing; `--tcp`; HTTP+SSE front-end; `prompt_over_wire` and
`detach_without_abort_over_socket` integration tests (need a secret-prompting tool fixture + `Endless`
script through the actor — the paths are exercised by the actor unit tests and the socket tests above).
