# Event Bus Enhancements — Full-Scope Implementation Plan

**Branch home base:** `nightly`  
**Filename rule:** this effort is called **enhancements**, not v2/v3.  
**Status:** planning/specification only — no implementation in this checkout.  
**Research inputs:** local gitignored `docs/research/event-bus-{openclaw,hermesagent,academic}.md` plus SynapsCLI `src/events/` review.

---

## Assumptions

1. SynapsCLI remains a single-machine CLI/runtime first, not a distributed broker service.
2. Rust + Tokio remains the implementation stack.
3. Existing event senders (`synaps send`, file inbox, Unix socket, registry) must remain backward compatible during migration.
4. Event bus enhancements are allowed to introduce new persistent local state under `~/.synaps-cli/` if the schema is documented and migration-safe.
5. We are not deferring known gaps to a future "v3"; durability, idempotency, hooks, tracing, backpressure, typed events, and observability are all in scope.
6. Implementation work must happen from a dedicated worktree/branch based on `nightly`.

---

## Objective

Upgrade SynapsCLI's event bus from a useful external-event inbox into a robust local event fabric for agent sessions.

Success means:

- External events from Discord/Slack/cron/CLI/agents/webhooks can be ingested, deduplicated, persisted when needed, traced, prioritized, transformed, vetoed, delivered, and replied to with clear semantics.
- The event schema is typed and evolvable.
- Slow consumers, queue pressure, dropped events, retries, and replay are observable instead of invisible.
- Users can extend behavior with file-discovered hooks without recompiling SynapsCLI.
- Running sessions can drain events in deterministic, bounded, tick-like batches.

---

## Existing Strengths to Preserve

SynapsCLI already has things OpenClaw/Hermes do not:

1. **Envelope-rich events** — `id`, `timestamp`, `source`, `channel`, `sender`, `content`, `expects_response`, `reply_to`.
2. **Severity-aware priority queue** — Critical/High events have meaningful placement.
3. **Multiple ingress transports** — file inbox, Unix socket, in-process registry.
4. **Per-session registry** — PID + socket path + session id/name.
5. **Request/reply primitives** — `expects_response` and `reply_to` are already present.
6. **Filesystem-first local UX** — aligns with SynapsCLI plugins/skills/agents.

These should be evolved, not replaced.

---

## Research-Derived Patterns to Import

### From OpenClaw

- Typed event identity.
- Double-buffered queue drain: active queue snapshot + pending queue for newly arriving events.
- Bounded per-tick work budget.
- Queue cancellation/abort semantics.
- Synchronous escape hatch for immediate delivery where appropriate.

### From HermesAgent

- File-discovered hooks.
- `emit_collect` style decision hooks that can return values.
- Thread/sync-to-async bridge patterns.
- Wildcard/prefix matching for hooks (`session:*`, `agent:*`, etc.).
- Gateway-style platform delivery separation.

### From Academic/Systems Literature

- Pub/sub type-based subscriptions.
- Bulkhead separation of critical vs lossy channels.
- Lamport/monotonic ordering.
- Idempotency keys and replay discipline.
- Trace context propagation.
- Backpressure and explicit lag/drop accounting.
- Single-writer / bounded-queue mechanical sympathy.
- Supervision and graceful shutdown.

---

## Scope: Everything Included

### 1. Typed, Evolving Event Schema

Add a real typed event identity layer while preserving JSON compatibility.

Required elements:

- `EventKind` enum: `Cli`, `Cron`, `Discord`, `Slack`, `Webhook`, `Agent`, `System`, `Tool`, `File`, `Socket`, `Unknown(String)` or equivalent compatibility variant.
- `EventContentKind` enum: `Message`, `Command`, `Alert`, `ToolResult`, `HookDecision`, `Trace`, `Heartbeat`, `Unknown(String)`.
- `schema_version: u16` on every event.
- `#[non_exhaustive]` on public enums where appropriate.
- Compatibility parsing from current `source.source_type` and `content.content_type` strings.
- Serializer emits both typed fields and legacy string fields during transition.

### 2. Monotonic Ordering and Causality

Add bus-assigned ordering fields:

- `seq: u64` — monotonic local event sequence.
- `ingested_at: DateTime<Utc>` — local receipt time.
- Optional `source_timestamp` when source supplies one.
- Optional `causation_id` and `correlation_id`.
- Deterministic ordering tuple: `(priority, seq)` for local delivery; source timestamp is metadata, not ordering authority.

### 3. Idempotency and Deduplication

Prevent duplicate external events from being processed repeatedly.

Required elements:

- `idempotency_key: Option<String>` in event envelope.
- Helper constructors per source, e.g. Discord message id, Slack event id, cron schedule+fire time.
- Bounded in-memory dedup cache for low-latency path.
- Optional persisted dedup window for durable sources.
- Metrics/logging for duplicate suppression.

### 4. Durable Queue / Replay

Add local persistence for events that cannot be safely lost.

Required elements:

- Configurable durability level per event/source:
  - `Volatile` — memory only.
  - `DurableUntilDelivered` — persisted until session receives it.
  - `DurableUntilAcked` — persisted until handler/agent explicitly acknowledges it.
- Local store under `~/.synaps-cli/events/`.
- Prefer append-only JSONL or SQLite; decision must be explicit before implementation.
- Replay command/API: inspect pending, replay by id, replay by session, drop by id.
- Crash recovery path on startup.

### 5. Backpressure and Queue Policy

Make queue pressure explicit and configurable.

Required elements:

- Separate channel tiers:
  - `critical` — bounded, durable/ack-capable, never silently drops.
  - `normal` — bounded, may reject with explicit error.
  - `lossy` — bounded, drop-oldest/drop-newest allowed for telemetry-like events.
- Queue policy enum: `RejectNew`, `DropOldest`, `DropLowestSeverity`, `BlockWithTimeout`, `PersistOverflow`.
- Per-source rate limits.
- Structured error type for rejected events.
- Metrics for depth, rejected, evicted, persisted overflow, lagged subscribers.

### 6. Double-Buffered Batch Drain

Adopt OpenClaw-style tick drain semantics for session delivery.

Required elements:

- `drain_batch(max_events, max_millis)` returns deterministic snapshot.
- Newly arriving events during drain go to pending queue, not the active snapshot.
- Agent session loop drains at safe boundaries: before turn, after tool completion, before sleep.
- Batch formatting produces one coherent system message or structured message group.
- Tests for re-entrant enqueue during drain.

### 7. Hooks System

Add file-discovered event hooks.

Required elements:

- Hook root: `~/.synaps-cli/hooks/`.
- Hook manifest format, likely `hook.toml` or `hook.yaml`.
- Initial hook runtimes:
  - Shell command hooks (`handler.sh`) for zero-dependency power users.
  - Rhai hooks if already acceptable in dependency policy; otherwise ask before adding.
- Hook phases:
  - `pre_ingest`
  - `post_ingest`
  - `pre_deliver`
  - `post_deliver`
  - `pre_reply`
  - `post_reply`
- Hook matching:
  - exact event kind
  - source/channel filters
  - wildcard/prefix patterns
  - severity filters
- Hook sandbox rules: env allowlist, timeout, stdout/stderr capture, max output size.

### 8. Decision / Veto Hooks (`emit_collect`)

Hooks must be able to return decisions, not just side effects.

Required decision results:

- `Allow`
- `Drop { reason }`
- `Defer { until }`
- `Rewrite { event }`
- `Escalate { severity }`
- `Route { session_id }`
- `RequireApproval { prompt }`

The bus should collect decisions deterministically and apply a documented precedence order. Example: `Drop` beats `Allow`; `Rewrite` composes only when unambiguous.

### 9. Observability and Trace Context

Every event flow should be debuggable.

Required elements:

- `trace_id`, `span_id`, `parent_span_id` or W3C `traceparent` equivalent.
- Structured tracing spans around ingest, queue, hook, deliver, reply.
- Metrics:
  - `event_bus.ingested_total`
  - `event_bus.delivered_total`
  - `event_bus.dropped_total`
  - `event_bus.duplicates_total`
  - `event_bus.queue_depth`
  - `event_bus.hook_duration_ms`
  - `event_bus.persisted_total`
  - `event_bus.replayed_total`
- Debug command: show event by id with trace trail.

### 10. Delivery Semantics and Ack Model

Define what delivery means.

Required elements:

- At-most-once path for volatile events.
- At-least-once path for durable/acked events.
- Explicit `EventAck` record: event id, session id, status, timestamp, handler result.
- Ack timeout behavior.
- Dead-letter queue for repeated failures.
- Manual DLQ inspect/retry/drop commands.

### 11. CLI / Operator UX

Expose event bus operations in the CLI.

Required commands or extensions:

- `synaps send` keeps working.
- `synaps events list` — pending/recent/dlq.
- `synaps events inspect <id>`.
- `synaps events replay <id|--session ...>`.
- `synaps events drop <id>`.
- `synaps events hooks list/test`.
- `synaps events metrics` or status integration.

### 12. Security / Safety

Event bus handles external input and can run hooks, so it is security-sensitive.

Required elements:

- Strict path validation for socket/session paths.
- Hook timeout and output limits.
- No secrets in event logs by default.
- Redaction helpers for event payloads.
- Optional allowlist for executable hooks.
- Durable store permissions: `0700` directories, `0600` files.
- Tests for traversal and malformed JSON.

---

## Non-Goals

Because the user explicitly said not to hold back for a future v3, these are narrow:

- No multi-node distributed broker cluster.
- No dependency on Redis/NATS/Kafka for core local functionality.
- No network-exposed event API without an explicit later security review.
- No breaking removal of current `synaps send`, file inbox, or Unix socket formats during the first implementation pass.

---

## Open Design Decisions

These need human confirmation before implementation:

1. **Persistence backend:** DECIDED — JSONL append-only.
   - Rationale: simple, inspectable, git/unix-friendly, easy emergency recovery, no DB dependency.
   - Required mitigation: store/index layout must make list/inspect/replay/DLQ efficient enough locally. Use append-only logs plus compacted sidecar indexes/checkpoints if needed.
2. **Hook runtime:** DECIDED — shell hooks first.
   - Rationale: zero new runtime dependency, easiest to audit, aligns with CLI power-user workflows.
   - Rhai can be added later within enhancements only if shell hooks prove too clumsy, but not required for first implementation.
3. **Durable default:** DECIDED — High and Critical default durable unless explicitly marked volatile; Medium/Low default volatile unless source requests durability.
   - Rationale: important external events should survive crashes by default without making all chatty telemetry durable.
4. **CLI shape:** DECIDED — add a new top-level `synaps events ...` command family.
5. **Convergence mode:** DECIDED — `holdout`.
6. **Metrics backend:** tracing-first counters/spans; add `metrics` crate only if already present or separately approved.
7. **Compatibility window:** legacy `source_type` / `content_type` fields retained for at least one release cycle.

---

## Dependency Graph

```text
Typed schema + errors
    ├── compatibility serde + constructors
    ├── idempotency key derivation
    ├── trace/correlation metadata
    └── queue policy decisions

Persistent store
    ├── durable enqueue
    ├── replay
    ├── ack / DLQ
    └── CLI inspection

Queue core
    ├── tiered channels
    ├── double-buffer batch drain
    ├── backpressure policy
    └── session delivery

Hook engine
    ├── manifest discovery
    ├── safe execution
    ├── emit_collect decisions
    └── pre/post phases

Observability
    ├── tracing spans
    ├── metrics
    ├── debug CLI
    └── tests + docs
```

---

## Implementation Tasks

### Task 1: Event schema foundation

**Description:** Add typed event identity, schema versioning, local sequencing, correlation fields, and compatibility conversion.

**Acceptance criteria:**

- [ ] `EventKind` and `EventContentKind` exist and are serde-compatible.
- [ ] Existing JSON events still deserialize.
- [ ] New events include `schema_version`, `seq`, `ingested_at`, optional trace/correlation/idempotency fields.
- [ ] Unit tests cover legacy and new JSON round trips.

**Verification:**

- [ ] `cargo test events::types`
- [ ] `cargo check`

**Dependencies:** None  
**Likely files:** `src/events/types.rs`, `src/events/format.rs`, tests  
**Scope:** M

---

### Task 2: Event error and policy types

**Description:** Introduce structured errors and queue/durability/backpressure policy enums.

**Acceptance criteria:**

- [ ] `EventBusError` or equivalent replaces stringly queue errors.
- [ ] `Durability`, `QueueTier`, `QueuePolicy`, `DeliveryGuarantee` are defined.
- [ ] Existing callers compile with minimal mapping.

**Verification:**

- [ ] `cargo test events`
- [ ] `cargo check`

**Dependencies:** Task 1  
**Likely files:** `src/events/types.rs`, `src/events/queue.rs`, `src/core/error.rs` if needed  
**Scope:** S

---

### Task 3: Idempotency cache

**Description:** Add bounded in-memory deduplication keyed by event idempotency key.

**Acceptance criteria:**

- [ ] Duplicate events with same key are suppressed.
- [ ] Suppression is logged with event/source metadata.
- [ ] Cache has bounded size/TTL.
- [ ] Events without keys continue through unchanged.

**Verification:**

- [ ] Unit tests for duplicate, expiry, missing-key behavior.
- [ ] `cargo test events`

**Dependencies:** Tasks 1-2  
**Likely files:** `src/events/dedup.rs`, `src/events/mod.rs`, `src/events/queue.rs`  
**Scope:** M

---

### Task 4: Tiered queue and backpressure policies

**Description:** Replace single queue behavior with explicit critical/normal/lossy tiers and configurable overflow policies.

**Acceptance criteria:**

- [ ] Critical events are never silently evicted.
- [ ] Lossy events can be dropped according to policy with metrics/logging.
- [ ] Existing severity ordering semantics are preserved within compatible tiers.
- [ ] Push API returns structured rejection details.

**Verification:**

- [ ] Unit tests for all policies.
- [ ] Stress test bounded queue behavior.
- [ ] `cargo test events::queue`

**Dependencies:** Tasks 1-3  
**Likely files:** `src/events/queue.rs`, `src/events/types.rs`  
**Scope:** M

---

### Task 5: Double-buffered batch drain

**Description:** Add OpenClaw-style snapshot drain for agent-safe delivery boundaries.

**Acceptance criteria:**

- [ ] `drain_batch(max_events, max_duration)` exists.
- [ ] Events enqueued during drain are not included in current snapshot.
- [ ] Priority ordering is deterministic in each batch.
- [ ] Existing `pop`/`drain` behavior remains or is shimmed.

**Verification:**

- [ ] Re-entrant enqueue tests.
- [ ] Batch ordering tests.
- [ ] `cargo test events::queue`

**Dependencies:** Task 4  
**Likely files:** `src/events/queue.rs`  
**Scope:** M

---

### Task 6: Durable event store decision + implementation

**Description:** Implement local durable storage for durable events, replay, ack state, and DLQ foundation.

**Acceptance criteria:**

- [ ] Backend decision recorded in this plan/spec.
- [ ] Durable events survive process restart.
- [ ] Delivered/acked events are removed or marked complete.
- [ ] Failed events can move to DLQ.
- [ ] Store files/directories have secure permissions.

**Verification:**

- [ ] Integration test with temp dir simulating restart.
- [ ] Permission test on Unix.
- [ ] `cargo test events`

**Dependencies:** Tasks 1-5  
**Likely files:** `src/events/store.rs`, `src/events/queue.rs`, `src/events/registry.rs`  
**Scope:** L — split if needed after backend decision

---

### Task 7: Ack / retry / DLQ semantics

**Description:** Add explicit delivery acknowledgements, retry metadata, timeouts, and DLQ operations.

**Acceptance criteria:**

- [ ] Event delivery can be marked `Delivered`, `Acked`, `Failed`, `DeadLettered`.
- [ ] Retry count and last error are stored.
- [ ] Ack timeout behavior is documented and tested.
- [ ] DLQ replay/drop APIs exist at core layer.

**Verification:**

- [ ] Unit tests for ack transitions.
- [ ] Integration tests for retry-to-DLQ.

**Dependencies:** Task 6  
**Likely files:** `src/events/store.rs`, `src/events/types.rs`, `src/events/registry.rs`  
**Scope:** M

---

### Task 8: Hook manifest discovery

**Description:** Add `~/.synaps-cli/hooks/` discovery and manifest parsing.

**Acceptance criteria:**

- [ ] Hooks are discovered from filesystem.
- [ ] Manifest validates phases, filters, timeout, command.
- [ ] Invalid hooks are reported but do not crash SynapsCLI.
- [ ] Tests use temp hook directories.

**Verification:**

- [ ] Unit tests for manifest parse/validation.
- [ ] `cargo test events::hooks`

**Dependencies:** Tasks 1-2  
**Likely files:** `src/events/hooks.rs`, `src/events/mod.rs`, docs  
**Scope:** M

---

### Task 9: Hook execution sandbox

**Description:** Execute shell hooks safely with timeout, env control, redaction, and output limits.

**Acceptance criteria:**

- [ ] Hook receives event JSON on stdin or env-documented path.
- [ ] Timeout kills hook.
- [ ] stdout/stderr captured with max size.
- [ ] Sensitive fields are redacted in logs.

**Verification:**

- [ ] Tests for success, timeout, too-large output, nonzero exit.
- [ ] Manual test with sample hook.

**Dependencies:** Task 8  
**Likely files:** `src/events/hooks.rs`, `src/events/security.rs`  
**Scope:** M

---

### Task 10: `emit_collect` decision hooks

**Description:** Add hook return values and deterministic decision resolution.

**Acceptance criteria:**

- [ ] Hook can return Allow/Drop/Defer/Rewrite/Escalate/Route/RequireApproval.
- [ ] Multiple hook decisions resolve deterministically.
- [ ] Pre-ingest/pre-deliver hooks can veto delivery.
- [ ] Rewrite decisions preserve event id/correlation rules or explicitly regenerate.

**Verification:**

- [ ] Tests for precedence and conflict cases.
- [ ] Integration test: hook drops a matching event.

**Dependencies:** Tasks 8-9  
**Likely files:** `src/events/hooks.rs`, `src/events/types.rs`, `src/events/ingest.rs`, `src/events/socket.rs`  
**Scope:** L — likely split into decision model + integration

---

### Task 11: Trace context and structured observability

**Description:** Add trace metadata propagation, spans, and event bus counters/gauges.

**Acceptance criteria:**

- [ ] Every ingested event gets a trace id if missing.
- [ ] Ingest, queue, hook, deliver, ack/reply are traced.
- [ ] Drop/eviction/duplicate/replay paths emit structured logs.
- [ ] Metrics abstraction is available or explicitly deferred behind tracing logs if dependency rejected.

**Verification:**

- [ ] Tests verify trace ids persist through serialization.
- [ ] Manual run with `RUST_LOG=debug` shows trace chain.

**Dependencies:** Tasks 1-7, 8-10 for hook spans  
**Likely files:** `src/events/*`, maybe `src/core/logging.rs`  
**Scope:** M

---

### Task 12: Ingress integration migration

**Description:** Update file inbox, Unix socket, and registry ingestion to use new schema, idempotency, hooks, durability, tracing, and policies.

**Acceptance criteria:**

- [ ] `synaps send` old format still works.
- [ ] File inbox old JSON still works.
- [ ] Unix socket old JSON still works.
- [ ] Each ingress path assigns seq, ingested_at, trace, idempotency if possible.
- [ ] Pre/post ingest hooks run.

**Verification:**

- [ ] Integration tests for file/socket/CLI ingestion.
- [ ] Manual `synaps send` to running session.

**Dependencies:** Tasks 1-11  
**Likely files:** `src/events/ingest.rs`, `src/events/socket.rs`, `src/events/registry.rs`, command files  
**Scope:** L

---

### Task 13: Session delivery integration

**Description:** Update session/agent loops to use batch drain, ack, DLQ, and pre/post deliver hooks.

**Acceptance criteria:**

- [ ] Running session drains batches at safe boundaries.
- [ ] Delivered durable events are acked or retried according to policy.
- [ ] Pre-deliver hook can route/drop/rewrite.
- [ ] Formatting remains readable and includes key metadata.

**Verification:**

- [ ] Integration test: event delivered to mock session and acked.
- [ ] Integration test: hook veto prevents delivery.
- [ ] Manual daemon wake-on-event test.

**Dependencies:** Tasks 5-12  
**Likely files:** `src/events/format.rs`, session/daemon/chat integration files  
**Scope:** L

---

### Task 14: CLI event operations

**Description:** Add operator commands for inspect/list/replay/drop/hooks/metrics.

**Acceptance criteria:**

- [ ] `synaps events list` shows pending/recent/DLQ.
- [ ] `synaps events inspect <id>` shows envelope + trace + delivery state.
- [ ] `synaps events replay <id>` requeues from durable store/DLQ.
- [ ] `synaps events drop <id>` removes pending/DLQ event.
- [ ] `synaps events hooks list/test` works.

**Verification:**

- [ ] CLI snapshot/help tests if project has them.
- [ ] Manual command run with temp store.

**Dependencies:** Tasks 6-13  
**Likely files:** `src/main.rs`, `src/cmd/events.rs`, `src/events/store.rs`, `src/events/hooks.rs`  
**Scope:** L

---

### Task 15: Security hardening pass

**Description:** Apply focused review to external input, durable storage, hooks, and socket paths.

**Acceptance criteria:**

- [ ] Path traversal tests for session ids, hook names, store ids.
- [ ] Malformed JSON tests for every ingress path.
- [ ] Hook command validation documented.
- [ ] Secrets redacted from event logs and inspect output.
- [ ] Store permissions tested on Unix.

**Verification:**

- [ ] `cargo test security` or targeted tests.
- [ ] Run engineering security review checklist before merge.

**Dependencies:** Tasks 1-14  
**Likely files:** `src/events/security.rs`, `src/events/*`, tests  
**Scope:** M

---

### Task 16: Documentation and migration guide

**Description:** Document event schema, hooks, CLI, durability, and migration from legacy formats.

**Acceptance criteria:**

- [ ] Event schema docs include examples.
- [ ] Hook docs include manifest + shell examples.
- [ ] Delivery semantics documented.
- [ ] Migration guide for legacy event JSON.
- [ ] Operational troubleshooting guide for DLQ/replay.

**Verification:**

- [ ] Docs reviewed against implemented CLI help.
- [ ] Example hook manually tested.

**Dependencies:** Tasks 1-15  
**Likely files:** `docs/events/enhancements.md`, README snippets, command help  
**Scope:** M

---

## Checkpoints

### Checkpoint A — Schema + Queue Core

After Tasks 1-5:

- [ ] `cargo test events`
- [ ] `cargo check`
- [ ] Legacy event JSON still deserializes.
- [ ] Batch drain ordering is deterministic.

### Checkpoint B — Durability + Ack

After Tasks 6-7:

- [ ] Crash/restart durable event test passes.
- [ ] DLQ transition test passes.
- [ ] Manual inspect of durable store works.

### Checkpoint C — Hooks

After Tasks 8-10:

- [ ] Hook discovery works.
- [ ] Hook timeout/nonzero behavior works.
- [ ] Veto/rewrite decisions work.

### Checkpoint D — Full Ingress/Delivery

After Tasks 11-13:

- [ ] File inbox, Unix socket, and CLI send all work.
- [ ] Running daemon receives, traces, and acks durable event.
- [ ] Dropped/duplicate/replayed events are observable.

### Checkpoint E — Operator UX + Security

After Tasks 14-16:

- [ ] CLI event commands work.
- [ ] Security tests pass.
- [ ] Docs match behavior.
- [ ] Release candidate can be built and installed from the worktree.

---

## Testing Strategy

Required test layers:

1. **Unit tests** for type serialization, queue policy, dedup, hook decisions.
2. **Integration tests** using temp dirs for file inbox, durable store, hooks.
3. **Tokio async tests** for socket ingestion, timeout, concurrent enqueue/drain.
4. **Crash/restart simulation** for durable events.
5. **Security tests** for traversal, malformed JSON, hook timeout/output cap.
6. **Manual smoke tests**:
   - `cargo check`
   - `cargo test`
   - `cargo install --path . --force --bin synaps`
   - `synaps send ...` to running daemon/session
   - `synaps events list/inspect/replay/drop`

---

## Worktree Plan

Implementation must not happen in the primary checkout.

Recommended worktree:

```bash
cd /home/jr/Projects/Maha-Media/SynapsCLI
git checkout nightly
git status
git worktree add /home/jr/Projects/Maha-Media/.worktrees/SynapsCLI-event-bus-enhancements \
  -b feat/event-bus-enhancements nightly
cd /home/jr/Projects/Maha-Media/.worktrees/SynapsCLI-event-bus-enhancements
```

All commits for this effort land on `feat/event-bus-enhancements`, then merge back into `nightly`.

---

## Convergence Decision

This is security-sensitive and broad: external input, hooks, persistence, replay, CLI commands, and agent delivery semantics.

Convergence mode: **holdout** — approved.

Parameters:

- `threshold: 0.85`
- `max_fix_iterations: 2`
- `max_total_calls: 10`
- Axis weights:
  - correctness: 0.30
  - security: 0.25
  - reliability: 0.20
  - maintainability: 0.15
  - UX/docs: 0.10

---

## Questions Before Coding

1. ✅ Persistence backend: JSONL append-only.
2. ✅ Hook runtime: shell hooks first.
3. ✅ Durable default: High/Critical are durable by default unless explicitly volatile; Medium/Low are volatile unless source requests durability.
4. ✅ CLI shape: new top-level `synaps events ...` command family.
5. ✅ Convergence mode: holdout.

---

## Recommended First Slice

Start with **Tasks 1-5 only** in the first implementation PR/merge:

- Typed schema
- Structured errors/policies
- Idempotency cache
- Tiered queue
- Double-buffered batch drain

This leaves the system buildable and gives later durability/hooks work a stable contract.

Even though all features are in scope for enhancements, implementing them in slices avoids a single unreviewable mega-commit.
