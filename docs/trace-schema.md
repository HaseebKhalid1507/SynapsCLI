# `synaps-request-trace/1` — schema note

Task 7 of the request-lifecycle-hardening plan (spec §6.1, §6.4). Types live
in `crates/agent-engine/src/runtime/trace/` (`types.rs` for the envelope,
`key.rs` for the digest key + keyed digests). Schema/type level only —
transports populate these records starting with Task 8.

## Envelope (`RequestTrace`)

One record per request attempt. Field order below is the canonical,
deterministic JSON serialization order.

| Field | Type | Notes |
|---|---|---|
| `schema` | string | Always `synaps-request-trace/1`; any other tag is rejected on read. |
| `session_id`, `turn_id`, `request_id` | id string | Validated `TraceId` (see *Validated identifiers*). |
| `attempt` | u32 | 1-based try ordinal for this record: `outcome.retries.len() + 1` (see *Attempt/retries rule*). |
| `model` | string | Exact `provider/model` identity (`QualifiedModelId`); provider is its first segment. |
| `transport` | enum | `anthropic_messages`, `open_ai_chat_completions`, `open_ai_responses`, `gemini_generate_content`, `vertex_generate_content`, `cloud_proxy`, `extension`. |
| `endpoint` | object | Validated `{host, path}` (see *Validated endpoint*) — never query strings, fragments, userinfo, or headers. |
| `anatomy` | object | Counts only: system segments, messages, blocks, tools. |
| `wire` | object? | Exact-wire `byte_len` + keyed `digest`, computed by the transport from the very bytes sent (Task 8). Absent until then. |
| `system_segments` | array | Per segment: `kind`, `byte_len`, keyed `digest`. |
| `messages` | array | Per message: `role` + per-block `kind`/`byte_len`. |
| `tools` | array | `stable_id` (`TraceId`), `wire_name` (`WireName`), `schema_byte_len`, keyed `schema_digest`. |
| `cache` | object | Boundary markers (`location`, `index`, `ttl`) plus optional cache-prefix diagnostics (Task 12, spec §6.6): tools/system stable-prefix and history-tail `byte_len` + keyed digest, and a per-segment previous-turn `delta`. All diagnostic fields are optional/defaulted — pre-Task-12 records still deserialize. |
| `translation_losses` | array | See *Translation losses*. Populated by the provider adapter's `TranslationReport` (Task 9, `runtime/transport/`). |
| `outcome` | object | `TransportOutcome`, below. |

## Validated identifiers

Every free-form-looking string in the envelope is a bounded, safe-alphabet
newtype that serializes as a **plain JSON string** (no wrapper object) and
validates on both construction and deserialization:

- **`TraceId`** — `session_id`, `turn_id`, `request_id`,
  `outcome.provider_request_id`, tool `stable_id`, translation-loss
  `element_id`. Nonempty, ≤ 256 bytes, ASCII from `[A-Za-z0-9._/:\[\]-]`
  only (brackets support positional paths like `messages[3].blocks[1]`).
  Whitespace, control chars, quotes, backslashes, and non-ASCII are rejected.
- **`WireName`** — tool `wire_name`, stricter (intersection of provider
  tool-name grammars): nonempty, ≤ 128 bytes, `[A-Za-z0-9_-]` only.

A hostile or oversized value (e.g. a 10 KiB blob or an embedded newline) in
any of these positions makes the whole record unreadable — rejected at parse.

## Validated endpoint

`EndpointMeta` has private fields and a validated constructor
(`EndpointMeta::new(host, path)`); serde keeps the `{host, path}` shape and
routes reads through the same validation:

- **host**: nonempty, printable ASCII, no `@` (userinfo), `?`/`#`
  (query/fragment), `/`, whitespace, or control chars. A DNS name / IPv4
  literal (`[A-Za-z0-9.-]`) or a bracketed IPv6 literal, each optionally with
  a numeric `:port` (1–5 digits) for local fixtures/proxies. No other colons.
- **path**: nonempty, begins with `/`, ≤ 1024 bytes, ASCII from
  `[A-Za-z0-9/-_.~%:=+,]` — structurally cannot carry a query string,
  fragment, or header-injection payload.

## Translation losses

Each entry is `action`
(`dropped`/`merged`/`renamed`/`synthesized`/`downgraded`/`unsupported`),
`element` (`system_segment`/`message_block`/`tool`/`parameter`/`other`), and
optional `element_id`. The `element_id` refers to the element **in the
normalized (pre-translation) request** — a tool stable ID, or a positional
path such as `messages[3].blocks[1]` indexing the provider-neutral IR, never
positions in the provider wire body and never content. The one exception is
`synthesized` entries: a synthesized element has no pre-translation position,
so it carries a symbolic ID in the `system.synthetic[N]` namespace (e.g. the
Anthropic OAuth identity system blocks). Entries are produced by the provider
adapter's `TranslationReport` (`runtime/transport/report.rs`): every dropped,
merged, renamed, synthesized, downgraded, or unsupported element is explicit —
silent semantic loss is a bug.

## `TransportOutcome`

Common terminal shape for every transport (spec §6.4). All metrics are
optional: an unobserved value is **absent** in JSON and `None` in Rust —
never a fabricated zero. Explicit `null` also reads back as `None`.

- `timings`: `send_start_unix_ms` plus ms offsets `headers_ms`,
  `first_byte_ms`, `first_model_event_ms`, `stream_end_ms`;
- `retries[]`: `attempt`, `class`
  (`rate_limited`/`overloaded`/`server_error`/`network`/`timeout`/`auth`/`other`),
  `delay_ms`;
- `provider_request_id` (`TraceId`), `http_status`, `stop_reason`
  (normalized enum);
- `usage`: token counts with `provenance`
  (`provider_reported`/`estimated`) — unreported metrics stay `None`;
- `terminal`: the spec §5.2 `TurnOutcome` (single engine-produced source of
  truth).

### Attempt/retries rule

One `RequestTrace` records one **actual transport attempt** (one HTTP send).
A request that is retried therefore yields one record per attempt, all
sharing the same `request_id`, with strictly increasing `attempt` ordinals
(Task 8 emission rule; see `runtime/trace/emit.rs`). `outcome.retries` lists
the tries that **failed** before this record's attempt, in order; each
entry's `attempt` is the 1-based ordinal of that failed try and `delay_ms`
the backoff before the next try. Every record satisfies
`attempt == retries.len() + 1`; a request that succeeds on the first try has
`attempt = 1` and an empty `retries`. Non-final attempt records carry a
typed `ProviderFailed` terminal describing that attempt's own failure; the
final record carries the request's terminal outcome. A cancellation observed
between attempts (during backoff, no send in flight) emits no extra record —
the preceding failed attempt was already recorded.

## Privacy invariants

- **Bounded + safe metadata only**: every field of every trace type is a
  count, byte length, enum, keyed digest, or bounded validated identifier
  (see above). Free-form text — prompt content, message bodies, tool
  results, headers, query strings, credentials — cannot fit these shapes:
  strings are capped (≤ 256 B IDs, ≤ 1 KiB paths), restricted to a
  no-whitespace/no-quote ASCII alphabet, and validated again on read. (Short
  strings spelled from the ID alphabet remain representable; the enforced
  invariant is bounded + safe, not semantic emptiness.) Content-bearing
  export is a separate explicit type (Task 12).
- Digests are HMAC-SHA256 (`hmac` + `sha2` crates) with per-component domain
  separation, rendered as a validated 64-char lowercase-hex newtype
  (`ComponentDigest`).
- The HMAC key is random per installation, 32 bytes, stored at
  `<synaps base dir>/trace/digest.key` — parent `0700`, file `0600`
  (a pre-existing broader mode is repaired to exactly `0600` via `fchmod` on
  the already-open handle), symlink refused, non-regular files (FIFO /
  device / directory) refused with a typed error before and after open
  (`O_NOFOLLOW | O_NONBLOCK` + `fstat`), reads bounded to 33 bytes with any
  length ≠ 32 treated as corrupt, and atomic `link(2)` publish so concurrent
  first-time creation converges on one key. The key and digest preimages are
  never logged.

## Persistence & shutdown flush (Task 11)

- **Toggle semantics:** the
  `telemetry` config key gates BOTH record kinds through one bounded session
  writer. `basic`/`full` enables legacy telemetry persistence
  (`~/.cache/synaps/api-log.jsonl`) and metadata-only trace persistence
  (`~/.cache/synaps/request-trace.jsonl`); `off` (the default) disables both
  — no writer thread exists and every trace seam is the no-op sink, so the
  request path does zero observability work.
- **Shutdown flush:** every clean process/runtime exit path that owns a
  `Runtime` (headless chat, TUI teardown, autonomous agent, RPC, server)
  calls `Runtime::shutdown_observability_async` under a short bounded budget
  (`DEFAULT_SHUTDOWN_FLUSH_TIMEOUT`, 2 s). "Flushed" means every queued
  record was appended into OS file buffers (`write(2)` returned) — there is
  deliberately **no `fsync`**: these are best-effort diagnostic logs, and
  surviving a kernel crash is not part of their contract. On timeout the
  worker stays detached and keeps draining; the caller logs a metadata-only
  warning (counter stats, never record content) and the exit proceeds —
  trace loss never changes an exit outcome. With telemetry `off` the flush
  returns `None` and is a true no-op.

## Task 12 — cache-prefix diagnostics, `/context`, trace controls & export

- **Cache diagnostics (spec §6.6):** the `cache` object may carry
  `tools_prefix`, `system_prefix`, and `history_tail` (`byte_len` + keyed
  digest over **canonical component bytes** — see
  `runtime/trace/diagnostics.rs` for the exact canonicalization and its
  documented approximations; the digests are keyed with the installation
  HMAC key and are never re-serialized wire bytes claimed as exact) plus a
  `delta` object: per-segment previous-turn state
  (`unchanged`/`changed`/`new`), `changed_tool_ids`, `tool_order_changed`,
  and `estimated_reused_bytes`/`estimated_recomputed_bytes` (canonical-byte
  estimates, not provider token accounting). The previous-turn snapshot
  lives in the session `TraceContext` (bounded metadata only, one mutex,
  compare-and-update atomic per emitted request); provider cache
  read/write token counts remain in `outcome.usage`. Currently computed on
  the Anthropic Messages path (the only wire that carries cache markers);
  other transports leave the diagnostic fields absent.
- **`/context`:** metadata-only report (system/tool/history counts+bytes,
  latest cache component change and reuse estimates, writer/degradation
  counters). Skills/memories and — outside history-owning surfaces —
  history counts are reported `unavailable` with that provenance, never
  fabricated.
- **`/trace next` / `/trace next content` / `/trace status`:** one-shot
  arm for exactly the next **logical** provider request (works with
  telemetry `off` via an ephemeral writer whose handle the runtime retains
  and drains in the shutdown epilogue; auto-disarms; never enables
  indefinite persistence). The arm is consumed inside
  `RequestTracer::begin`: the first request through the armed context wins,
  all of its retry attempts emit records, and subsequent tool-loop requests
  sharing the same options are disabled. `next content` additionally writes
  a **redacted-at-capture**, bounded (1 MiB) capture bundle
  (`synaps-trace-content-capture/1`, `0600`, under
  `<synaps base dir>/trace/capture/`) of the request **body only** —
  headers/credentials structurally never reach the capture seam. Capture is
  supported wherever this process holds the serialized request body
  pre-send (Anthropic, OpenAI-compatible, Codex, xAI, Gemini, and the
  cloud-invoke broker request); on the extension sidecar path (which owns
  serialization out of process) the arm fails explicitly — a metadata
  warning plus a degraded-records bump visible in `/trace status`, never a
  silent consume. Internal compaction requests never consume the arm.

  **Capture expiry (documented guarantee):** expiry is *logical* — a
  bundle older than 15 minutes can never be exported (the export path
  checks the embedded expiry and deletes stale bundles). Physical deletion
  is *opportunistic*: no background process survives CLI exit, so stale
  bundles are swept on the next trace interaction (new capture,
  `/trace status`, any `synaps trace export` invocation).
- **`synaps trace export <id> --metadata-only --output PATH`:** validates
  every trace-log line as a `RequestTrace`, selects the exact turn/request
  ID, and writes re-serialized records to a fresh private file (`0600`,
  parent `0700`, symlink and pre-existing targets refused). No network.
- **`synaps trace export <id> --include-content --allow-content-export`:**
  fail-closed content export. Requires both flags in one invocation plus an
  existing unexpired capture bundle; re-redacts recursively (defense in
  depth), writes a `synaps-trace-content-export/1` artifact (never a
  request-trace schema), and consumes (deletes) the bundle. Expired or
  malformed bundles are deleted and refused. Prompts are never
  reconstructed from sessions.
