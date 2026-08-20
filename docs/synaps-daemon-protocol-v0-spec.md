# Synaps Daemon + Control Protocol v0 — Spec

**Status:** DRAFT — milestone 1 of the ADE track
**Created:** 2026-08-03 (S289)
**Owner:** Jawz / Haseeb
**Depends on:** agent-runtime v0.7.x workspace (agent-core, agent-engine, agent-tui)
**Consumers:** ADE (separate repo, Tauri/React), future: TUI, event bus (#91), reactive subagents (#90), Shadow/Discord ports (#83)

---

## 1. Thesis

Synaps gains a **long-lived daemon** exposing a **versioned control API** over a local
Unix domain socket, backed by an **append-only structured event log** per session.
The ADE is strictly a client of this API. The daemon is core Synaps value even if
the ADE frontend never ships: it delivers multi-session management, observability,
replay, and a stable boundary that turns every future integration (bots, watchers,
remote runners) into "just a client."

**One rule above all:** if a client can't do something through the protocol, that is
a daemon feature request — never a reason to reach into internals.

---

## 2. Current state (recon, 2026-08-03)

What already exists and gets promoted, not rebuilt:

| Asset | Where | State |
|---|---|---|
| `synaps server` | `src/cmd/server.rs` (1,465 loc) | Axum + WebSocket, **single session**, broadcast to all clients, optional bearer auth, auto-turn reactor integration |
| Wire protocol | `crates/agent-core/src/core/protocol.rs` | `ClientMessage` (Message/Command), `ServerMessage` (Thinking/Text/ToolUseStart/ToolUse/ToolResult/Usage/Error/System/History/Status), `HistoryEntry` |
| Engine conversation state | `agent-engine/src/engine/session.rs` — `ConversationState` | Single source of truth: api_messages, token counters, cost, abort context, queued messages, pending events |
| Stream events | `agent-engine/src/engine/stream.rs` — `EngineStreamEvent` | Engine-level streaming events incl. subagent tracking |
| Events subsystem | `agent-engine/src/events/` (format, ingest, queue, registry, socket, types) | Per-session socket listener, event queue, reactor wake |
| Headless engine | `docs/headless-engine-spec.md`, `engine/setup.rs` (`EngineOpts`, `BackgroundTasks`) | Engine boots without TUI |

### Gaps the daemon closes

1. **Single-session.** `ServerState` holds ONE `ConversationState`. No create/list/attach N sessions.
2. **No persistence.** Events are broadcast and gone. No replay, no post-mortem, no reconnect-with-catchup.
3. **No sequencing.** `ServerMessage` has no session id, no monotonic sequence, no timestamps at the protocol layer. A dropped WebSocket loses truth.
4. **Untyped surface.** No OpenAPI/schema artifact; clients hand-write types and drift.
5. **TCP-first.** Server binds a port; local-first security wants a UDS default.

---

## 3. Goals / Non-goals

### Goals (v0)

- G1. `synaps daemon` — long-lived process managing **N concurrent sessions**.
- G2. **Control API v0** over UDS: HTTP/JSON commands + SSE event streams.
- G3. **Append-only event log** per session, persisted to SQLite, replayable.
- G4. **Versioned protocol** with an OpenAPI artifact and generated TS client.
- G5. **Steer + approve** running sessions through the API (no abort-resynthesize).
- G6. Existing TUI and `synaps server` remain untouched and working.

### Non-goals (v0 — deliberately)

- Remote/network transport, TLS, multi-user auth, tenancy. (UDS + filesystem perms is the v0 security model. The transport abstraction must not preclude these later.)
- Agent manifest format (`synaps.agent.yaml`) — milestone 2.
- Evals, fleet scheduling, memory browser — later milestones.
- Migrating the TUI to be a daemon client. (Desirable end-state; not v0.)
- Cross-process event relay / clustering.

---

## 4. Architecture

```text
┌────────────┐  ┌────────────┐  ┌──────────────┐
│  ADE (UI)  │  │  jawz-*    │  │ future: bots │
└─────┬──────┘  └─────┬──────┘  └──────┬───────┘
      │   HTTP/JSON + SSE over UDS     │
┌─────▼───────────────▼────────────────▼───────┐
│ crates/agent-daemon  (new)                   │
│  api/        Axum router, SSE, OpenAPI       │
│  sessions/   SessionSupervisor (N sessions)  │
│  eventlog/   append-only log + SQLite store  │
│  proto/      v0 types (re-exported to core)  │
└─────┬────────────────────────────────────────┘
      │ in-process
┌─────▼────────────────────────────────────────┐
│ agent-engine: ConversationState, stream,     │
│ reactor, tools, providers, events            │
└──────────────────────────────────────────────┘
```

### 4.1 New crate: `crates/agent-daemon` (`synaps-daemon`)

- Depends on `agent-core` + `agent-engine`. **`agent-tui` must not appear in its
  dependency graph.** If something in TUI is needed, it moves down to engine/core first.
- Root binary gains `synaps daemon [--socket PATH] [--foreground]` in `src/cmd/`.
- One tokio runtime; each session is a supervised task owning its
  `ConversationState` + engine loop, communicating via mpsc command channel and
  broadcasting `SessionEvent`s.

### 4.2 SessionSupervisor

- Owns the session registry: `session_id → SessionHandle { cmd_tx, state, meta }`.
- Session lifecycle: `created → running → idle → (sleeping) → ended | failed`.
- Enforces per-session serialization: one in-flight turn per session; steering
  injects between tool calls (aligns with #112 — protocol reserves the semantics
  even if v0 implementation is "queue until next tool boundary").
- On daemon start: recovers session metadata from store; historical sessions are
  replayable, not auto-resumed.

### 4.3 Event log (the backbone)

Every session produces an ordered stream of `SessionEvent`:

```rust
struct Envelope {
    session_id: Uuid,
    seq: u64,          // per-session, monotonic, gapless — assigned at append
    ts: DateTime<Utc>, // daemon clock at append
    event: SessionEvent,
}

enum SessionEvent {
    // lifecycle
    SessionCreated { config: SessionConfig },
    RunStarted { run_id: Uuid, trigger: RunTrigger },       // user | steer | auto_turn | api
    RunEnded { run_id: Uuid, outcome: RunOutcome },          // completed | aborted | error
    SessionEnded { reason: EndReason },
    // model turn
    ThinkingDelta { run_id: Uuid, text: String },
    TextDelta { run_id: Uuid, text: String },
    MessageComplete { run_id: Uuid, role: Role, content: String },
    // tools
    ToolRequested { run_id: Uuid, call_id: String, tool: String, input: Value },
    ToolApprovalRequired { call_id: String, prompt: String },
    ToolApprovalResolved { call_id: String, approved: bool, by: Actor },
    ToolCompleted { call_id: String, ok: bool, output: String, duration_ms: u64 },
    // orchestration
    SubagentSpawned { handle: String, agent: Option<String>, task: String },
    SubagentEnded { handle: String, status: String },
    SteerInjected { text: String, by: Actor },
    // accounting + errors
    Usage { run_id: Uuid, input_tokens: u64, output_tokens: u64,
            cache_read: u64, cache_creation: u64, cost_usd: f64 },
    EngineError { scope: ErrorScope, message: String },
    Notice { text: String },
}
```

Mapping from existing `EngineStreamEvent`/`ServerMessage` variants is a pure
translation layer — the engine is not rewritten. Deltas are logged (bounded, see
retention) so replay reproduces the live experience; `MessageComplete` rows make
coarse replay cheap.

**Invariants:**
- Append-only. No updates, no deletes (retention prunes whole sessions only).
- `seq` gapless per session; clients detect loss and re-sync via catch-up reads.
- The log is the source of truth for *what happened*; `ConversationState`
  remains the source of truth for *what the model sees next*.

### 4.4 Storage

SQLite (rusqlite, WAL) at `~/.synaps-cli/daemon/daemon.db`:

```sql
sessions(id TEXT PK, created_ts, ended_ts, title, agent, model,
         cwd, status, protocol_version)
events  (session_id TEXT, seq INTEGER, ts TEXT, kind TEXT, run_id TEXT,
         payload TEXT/JSON,  PRIMARY KEY(session_id, seq))
```

- Delta events may be coalesced at flush time (e.g. merge consecutive
  `TextDelta` within a message) to bound row counts; coalescing must preserve
  reconstructed content byte-for-byte.
- Retention: config `daemon.retention_days` (default 30); prune runs on start.
- **Blast radius:** daemon.db is daemon-owned. Nothing else writes it. Tests run
  against tempdir copies, never the live file (S51 rule).

---

## 5. Control API v0

### 5.1 Transport

- Default: **UDS** at `~/.synaps-cli/daemon/daemon.sock` (0600). HTTP/1.1 over UDS.
- `--tcp 127.0.0.1:PORT` opt-in for tooling that can't UDS; loopback only in v0.
- Content type `application/json`; streams are `text/event-stream` (SSE).
- SSE chosen over WebSocket for v0: unidirectional server→client fits the model
  (commands are plain HTTP), trivial catch-up semantics via `Last-Event-ID`.

### 5.2 Handshake & versioning

```
GET /v0/health   → { status, daemon_version, protocol_version: "0.x", pid, uptime_s }
```

- Path-versioned (`/v0/...`) + `protocol_version` in health. Breaking changes bump
  the path version; additive changes bump minor. Clients pin a minimum minor.
- Unknown enum variants MUST be tolerated by clients (serde `other`-style) —
  additive event kinds are a minor bump, not a break.

### 5.3 Endpoints

```
# sessions
POST   /v0/sessions                 { config: SessionConfig } → { session }
GET    /v0/sessions                 ?status=…  → [ SessionMeta ]
GET    /v0/sessions/{id}            → SessionMeta + counters (tokens, cost, seq)
DELETE /v0/sessions/{id}            → end session (graceful; body: { reason })

# interaction
POST   /v0/sessions/{id}/messages   { content } → { run_id }        # user turn
POST   /v0/sessions/{id}/steer      { content } → { accepted }      # mid-run injection
POST   /v0/sessions/{id}/abort      → { aborted }                   # cancel in-flight run
POST   /v0/sessions/{id}/approvals/{call_id}  { approved: bool }

# events
GET    /v0/sessions/{id}/events     ?from_seq=N&limit=M → { events: [Envelope], next_seq }
GET    /v0/sessions/{id}/stream     SSE; ?from_seq=N replays catch-up then goes live
GET    /v0/stream                   SSE; daemon-level: session lifecycle + notices

# introspection
GET    /v0/agents                   → agents in ~/.synaps-cli/agents/ (name, description)
GET    /v0/models                   → provider/model catalog visible to the daemon
GET    /v0/openapi.json             → the contract, served by the thing that speaks it
```

`SessionConfig` v0: `{ agent?: string, model?: string, cwd?: string,
system_prompt?: string, title?: string, auto_approve: bool }`. Manifest files
come in milestone 2; the API shape already accommodates them (`agent` field).

### 5.4 SSE contract

- `id:` = `seq` (enables `Last-Event-ID` resume), `event:` = event kind,
  `data:` = full `Envelope` JSON.
- On subscribe with `from_seq`, daemon streams stored events (catch-up) then
  transitions seamlessly to live. Client code path is identical for both —
  **this is the replay feature falling out of the architecture for free.**
- Heartbeat comment frame every 15s; client reconnect with `Last-Event-ID` is
  lossless by construction.

### 5.5 Auth (v0)

Socket file permissions (0600, user-owned). No tokens over UDS. The optional TCP
listener reuses the existing bearer-token scheme from `synaps server`. Remote
auth is explicitly a vNext concern; the router layering must keep it insertable
as middleware.

---

## 6. Contract artifact & client generation

- OpenAPI generated from the Axum router + types via `utoipa` at build/test time;
  committed as `docs/api/daemon-v0.openapi.json`.
- CI check: regenerate and diff — drift fails the build.
- TS client for the ADE generated from the committed spec (openapi-typescript);
  ADE repo pins a `protocol_version` and checks it against `/v0/health` at connect.
- **No hand-maintained duplicate types anywhere.** Rust structs are the truth.

---

## 7. Delivery plan

### Phase D0 — skeleton (small)
`agent-daemon` crate; `synaps daemon` subcommand; UDS Axum server; `/v0/health`;
OpenAPI generation wired; CI drift check. *Proves the boundary end-to-end.*

### Phase D1 — one session, full loop
`POST /sessions` boots a headless engine session (reuse `EngineOpts`/`setup`);
`POST /messages` runs a turn; translation layer `EngineStreamEvent → SessionEvent`;
in-memory event log + SSE live stream. *Proves engine embedding.*

### Phase D2 — persistence + replay
SQLite store; gapless seq; catch-up reads; `?from_seq` SSE; retention; recovery
of session metadata on daemon restart. *Proves the backbone.*

### Phase D3 — N sessions + control
SessionSupervisor registry; concurrent sessions; abort; approvals over API;
steer (v0 semantics: inject at next tool boundary); daemon-level `/v0/stream`.

### Phase D4 — hardening
Backpressure on slow SSE consumers (bounded buffer + drop-to-catchup),
delta coalescing, `synaps daemon status` CLI, docs, contract tests vs. the
committed OpenAPI spec, soak test: 3 concurrent sessions × 100 turns replayed
byte-identical.

Each phase lands independently green: `cargo test` workspace-wide, no TUI
regressions, no changes to existing `synaps server` behavior.

## 8. Testing

- **Contract tests:** golden OpenAPI diff; serde round-trip on every event kind;
  unknown-variant tolerance test.
- **Integration:** spin daemon on tempdir UDS + mock provider; run scripted
  session; assert event log content and SSE ≡ store replay.
- **Replay invariant (the flagship test):** for any completed session,
  reconstruct transcript from events and diff against `ConversationState`
  history — byte-identical text content.
- **Crash test:** kill -9 mid-turn; restart; store is consistent (WAL), session
  marked failed, replay works up to last flushed seq.

## 9. Risks & open questions

| # | Risk / question | Position |
|---|---|---|
| 1 | Engine assumes single global session in places (statics, config load) | D1 will surface these; fix in engine, not by daemon workarounds |
| 2 | Steer semantics (#112) — true mid-turn injection is engine work | Protocol ships the endpoint; v0 implements queue-at-tool-boundary; engine upgrade slots in later without API change |
| 3 | Delta volume in SQLite | Coalescing + retention; measure in D2 soak before optimizing further |
| 4 | Does the TUI adopt the daemon? | Not v0. Revisit after D4 — sleep/wake lifecycle (#88) is the natural moment |
| 5 | Event schema overlap with `agent-engine/src/events/` (watcher/inbox events) | Keep separate in v0; unify under event bus (#91) once both exist |
| 6 | uds + axum: `hyperlocal`/`tokio` UDS accept loop | Known pattern; existing per-session socket listener is prior art in-tree |

## 10. Decision record

- **D-1:** Daemon lives in agent-runtime as a workspace crate; ADE UI is a
  separate repo consuming the generated client. Split at the protocol.
- **D-2:** UDS + HTTP/JSON + SSE for v0. WebSocket not used by the daemon (the
  existing `synaps server` keeps its WS; untouched).
- **D-3:** Append-only per-session event log in SQLite is the observability,
  replay, and reconnect backbone. Log ≠ model context; both coexist.
- **D-4:** OpenAPI generated from Rust types, committed, drift-gated in CI.
- **D-5:** v0 is local-single-user; security = socket permissions.
