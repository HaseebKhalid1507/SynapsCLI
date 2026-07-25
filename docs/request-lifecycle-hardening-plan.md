# Request Lifecycle Hardening — Implementation Plan

## Objective

Implement the five-phase program defined in
[`docs/request-lifecycle-hardening-spec.md`](./request-lifecycle-hardening-spec.md):
(1) immediate correctness and privacy hardening, (2) provider-neutral
observability, (3) authorization-enforced progressive capability disclosure,
(4) bounded side-effect-aware agent execution, and (5) consistent context
management and project memory. Every task below is small, independently
verifiable, leaves the workspace compiling and green, and is proven by
headless red→green tests. The spec is the canonical requirements source; this
plan only sequences and sizes the work.

**Base commit context:** branch `feat/request-lifecycle-hardening`, based on
PR #63 head `d20e03f6b9781e03fa80d24880b5c88354cfe43f` (spec commit `b325dc2`
on top). All work happens in this dedicated worktree:
`/home/jr/Projects/Maha-Media/.worktrees/SynapsCLI-request-lifecycle-hardening`.

## Convergence mode

**Mode: holdout.** This effort modifies privacy-sensitive tracing, filesystem
permission enforcement, and tool/model authorization gating — exactly the
class of change where implementation-authored tests share the implementer's
blind spots. Spec §15 explicitly requires holdout convergence for
architecture- and security-sensitive phases, and §13.4 requires an external
adversarial oracle. Parameters are fixed for the entire run and must not
change mid-run:

| Parameter            | Value |
| -------------------- | ----- |
| Threshold            | 0.8   |
| Axis weights         | security/privacy 0.35, correctness 0.30, spec fidelity 0.20, code quality 0.10, docs 0.05 |
| max_fix_iterations   | 2     |
| max_total_calls      | 10    |

The oracle operates from an isolated worktree with read-only access to this
branch, authors the adversarial harnesses of §13.4 independently, and no
phase is considered converged without a fresh passing oracle verdict.

## Local tooling constraints (encoded in every Verification block)

- `cargo clippy` is **not available locally** (`error: no such command:
  clippy`); clippy runs only in GitHub CI. Local verification uses
  `cargo check`, `cargo test`, targeted `rustfmt --check`, and
  `git diff --check`.
- Full-workspace `cargo fmt --check` fails on pre-existing unrelated diffs in
  `crates/agent-core/src/core/config.rs`. Use per-file
  `rustfmt --edition 2021 --check <touched files>` instead.
- Primary suites: `cargo test -p synaps-engine`, `cargo test -p synaps-core`,
  plus workspace integration tests under `tests/` (e.g.
  `cargo test --test tools_export`).
- `git diff --check` (no trailing whitespace) is enforced repo-wide,
  including on this document.

## Dependency graph

Phase-level (strict, per spec §1/§15):

```text
Phase 1 ──> Phase 2 ──> Phase 3 ──> Phase 4 ──> Phase 5
(privacy)   (measure)   (disclose)  (bound)     (context/memory)
```

Task-level (arrows mean "depends on"):

```text
Phase 1: T1        T2        T3        T4        T5        T6
          \        |        /                              |
           +---(T6 harness depends on T1..T5)--------------+

Phase 2: T7 ──> T8 ──> T9 ──> T10 ──> T12
          \            |        \
           +──> T11    +──> T13 (harness: T7..T12)

Phase 3: T14 ──> T15 ──> T16 ──> T17 ──> T18
                          |        \       \
                 T19 <────+   T20 <─+  T21 <┘   T22 (harness: T14..T21)
                 (MCP)        (ext)   (skills)

Phase 4: T23 ──> T25          T24 ──> T25
         T23,T24 independent; T26 ──> after T25; T27 after T23..T26
         T28 (harness: T23..T27)

Phase 5: T29 ──> T30 ──> T31        T32 ──> T33 ──> T34
                                     (memory chain)
         T35 (journal) independent after T29
         T36 (harness: T29..T35)
```

Cross-phase hard edges: T2 (BoundedText) is used by T9/T26; T7/T8 (trace +
wire digest) are prerequisites for measuring T18 (core-set minimization) and
T23 (budgets, cost dimension); T14–T16 (catalog/gate) are prerequisites for
T24 (effect metadata lives on catalog entries).

## Definition of done per task

Every task, in addition to its own acceptance criteria, is done only when:

1. New behavior was proven by a failing-first test (red) that now passes
   (green); behavior-preserving refactors are proven by existing tests
   staying green with no test weakened or removed (spec §13, §14).
2. `cargo check --workspace` passes; the task's listed test commands pass.
3. `rustfmt --edition 2021 --check` passes on every touched `.rs` file and
   `git diff --check` reports nothing.
4. No raw prompt/history/tool-result/memory content or credential is added
   to any log, trace, commit message, or fixture (spec §14 "Never do").
5. Subagent model-authorization invariants are untouched: exact-model
   grants, session-only, no network before authorization, subagent tool
   unavailable to recursive subagents.
6. The change is committed on `feat/request-lifecycle-hardening` with a
   focused, reviewable diff (target 100–300 lines where feasible, spec §15).

The **global** definition of done is spec §16 verbatim; the final checkpoint
(CP-14) verifies each of its eleven points.

---

## Phase 1 — Immediate correctness and privacy hardening (spec §5)

### Task 1 — Remove raw request payload from generic tracing

- **Description:** Delete the `tracing::trace!("Outgoing API Request
  Payload:\n{}", serde_json::to_string_pretty(&body)…)` call at
  `crates/agent-engine/src/runtime/api.rs:972` and replace it with a
  metadata-only trace line (provider, model, payload bytes, message count,
  tool count, cache-marker count, correlation ID). Audit all other
  `trace!`/`debug!` sites in `runtime/` (`api.rs`, `api_sync.rs`,
  `openai/`, `google_gemini/`, `google_vertex.rs`, `sse.rs`,
  `helpers.rs:63` steering-message log) for message text, system-prompt
  content, tool results/arguments, skill bodies, memory bodies, or
  credential-bearing values; convert each to metadata-only. Do not add a
  raw dev-capture feature in this task (that is spec §5.1's separate
  opt-in, deferred to T12 alongside trace export).
- **Acceptance criteria:**
  - No log statement at any level serializes request bodies, message
    blocks, system prompts, tool results, or tool arguments.
  - Metadata-only replacement logs byte length, counts, and IDs only.
  - A new test builds a request containing a unique sentinel string,
    captures `tracing` output at `TRACE`, and asserts the sentinel is
    absent while metadata fields are present (red before, green after).
  - Request-body byte identity is unchanged (existing golden body tests in
    `runtime/body_golden.rs` stay green).
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine
  grep -rn "to_string_pretty(&body)" crates/agent-engine/src/runtime/ ; test $? -eq 1
  rustfmt --edition 2021 --check crates/agent-engine/src/runtime/api.rs
  git diff --check
  ```

- **Dependencies:** None.
- **Files likely touched:** `crates/agent-engine/src/runtime/api.rs`,
  `crates/agent-engine/src/runtime/helpers.rs`, possibly sibling provider
  modules under `crates/agent-engine/src/runtime/`.
- **Scope:** S

### Task 2 — Shared UTF-8-safe `BoundedText` helper

- **Description:** Add a `BoundedText` type (`text`, `original_bytes`,
  `retained_bytes`, `truncated`) with a byte-budget, char-boundary-safe
  constructor in `crates/agent-core` (exported like the existing
  `truncate_str` at `crates/agent-core/src/lib.rs:41`). Migrate
  `truncate_tool_result` in `crates/agent-engine/src/runtime/helpers.rs:272`
  (currently `chars().take(max_chars)` — a char budget, not a byte budget)
  and other preview/truncation call sites (`truncate_str` users in tool
  output, subagent state, chat previews) to the shared helper. Grep-audit
  for direct byte slicing of arbitrary strings.
- **Acceptance criteria:**
  - One shared utility produces byte-bounded, valid-UTF-8 previews and
    reports exact original/retained bytes and truncation flag.
  - Property test: arbitrary Unicode (multibyte, emoji, CJK, combining
    marks) never panics and never exceeds the byte budget.
  - Tool-history truncation preserves its existing user-visible marker
    format or updates it with tests adjusted intentionally (documented in
    the commit message).
  - No production code path performs direct byte-index slicing of
    arbitrary UTF-8 strings.
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-core
  cargo test -p synaps-engine
  rustfmt --edition 2021 --check crates/agent-core/src/lib.rs \
    crates/agent-engine/src/runtime/helpers.rs
  git diff --check
  ```

- **Dependencies:** None.
- **Files likely touched:** `crates/agent-core/src/lib.rs` (or new
  `crates/agent-core/src/text.rs`), `crates/agent-engine/src/runtime/helpers.rs`,
  call sites in `crates/agent-engine/src/tools/`, `src/cmd/chat.rs`.
- **Scope:** M

### Checkpoint CP-1 (after T1–T2)

```bash
cargo check --workspace
cargo test -p synaps-engine
cargo test -p synaps-core
git diff --check
git log --oneline -3   # expect two focused commits atop b325dc2
```

Durable artifact: two commits — "privacy: metadata-only request tracing"
and "core: shared UTF-8-safe BoundedText"; workspace green.

### Task 3 — Typed `TurnOutcome` and headless failure preservation

- **Description:** Introduce the spec §5.2 `TurnOutcome` enum (and
  `BudgetDimension` placeholder) in the engine and surface it from the
  stream loop. Fix `src/cmd/chat.rs`, where
  `StreamCompletion::Done | StreamCompletion::Error(_) => { … break
  StreamCompletion::Done; }` collapses errors into success and the command
  always returns `Ok(())`: an unrecovered provider/tool failure must exit
  nonzero (or emit a structured error in machine-output mode) while still
  saving valid partial history. Make history repair track messages appended
  by the active turn instead of heuristically dropping a trailing message
  by role (audit `strip`/repair logic in `runtime/helpers.rs`). Propagate
  the same terminal category + correlation ID through TUI/RPC/server/
  watcher/subagent adapters (thin adapter changes only).
- **Acceptance criteria:**
  - `synaps chat` in headless mode exits nonzero on an unrecovered
    provider failure (integration test via stub failure, e.g. extending
    `tests/chat_stdin.rs`); red before, green after.
  - Partial assistant output and completed tool results survive failure in
    the saved session.
  - All frontends receive the same `TurnOutcome` variant and correlation
    ID for the same fixture (cross-mode contract test, cf.
    `tests/c6_cross_mode_contract.rs`).
  - History repair removes only turn-appended messages; a pre-existing
    trailing user message is never removed.
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine
  cargo test --test chat_stdin
  cargo test --test c6_cross_mode_contract
  rustfmt --edition 2021 --check src/cmd/chat.rs \
    crates/agent-engine/src/runtime/stream.rs
  git diff --check
  ```

- **Dependencies:** None (T2 helpful for error previews but not required).
- **Files likely touched:** `crates/agent-engine/src/runtime/stream.rs`,
  `crates/agent-engine/src/runtime/types.rs`,
  `crates/agent-engine/src/runtime/helpers.rs`, `src/cmd/chat.rs`, thin
  adapter files under `src/cmd/` and `crates/agent-tui/`.
- **Scope:** M

### Task 4 — Private filesystem modes for sensitive state

- **Description:** Add shared private-write helpers (dir `0700`, files and
  temp files `0600`, symlink-safe open, atomic create-with-mode →
  write → rename so no interval is broader than policy) in `agent-core`,
  and apply them to: `core/session.rs::save` (currently
  `tokio::fs::write` to `.tmp` with default umask modes),
  `core/session_index.rs:84`, `memory/store.rs:129` (append with
  `OpenOptions::create`), usage logs, and `runtime/telemetry.rs` (already
  `0o600` on the file at line 307 — extend to its directory and rotation).
  Detect pre-existing broader modes and either repair (chmod) or warn once
  with actionable guidance. Non-Unix platforms keep current behavior with
  the strongest available controls.
- **Acceptance criteria:**
  - Under a permissive test umask (e.g. `0o000`), newly created session,
    index, memory, telemetry, and temp files are exactly `0600` and their
    directories `0700` (failing-first tests).
  - A symlink planted at the target path causes a safe failure, not a
    write through the link.
  - At no point does a fresh file exist with broader-than-policy mode
    (create with mode, not create-then-chmod).
  - Existing broader files are repaired or reported once.
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-core
  cargo test -p synaps-engine
  rustfmt --edition 2021 --check crates/agent-core/src/core/session.rs \
    crates/agent-core/src/core/session_index.rs \
    crates/agent-core/src/memory/store.rs \
    crates/agent-engine/src/runtime/telemetry.rs
  git diff --check
  ```

- **Dependencies:** None.
- **Files likely touched:** new `crates/agent-core/src/core/private_fs.rs`
  (or similar), `crates/agent-core/src/core/session.rs`,
  `crates/agent-core/src/core/session_index.rs`,
  `crates/agent-core/src/memory/store.rs`,
  `crates/agent-engine/src/runtime/telemetry.rs`.
- **Scope:** M

### Checkpoint CP-2 (after T3–T4)

```bash
cargo check --workspace
cargo test --workspace
cargo test --test chat_stdin
git diff --check
```

Durable artifact: commits "engine: typed TurnOutcome across frontends" and
"core: private filesystem modes for sensitive state"; full workspace suite
green.

### Task 5 — Honest cloud tool capability (enforced + advertised text-only)

- **Description:** The cloud broker already rejects tools at invoke time
  (`crates/agent-core/src/core/auth/broker.rs:942` returns
  `BrokerError::Denied("tools are not yet supported…")`), but nothing
  advertises this, and rejection happens inside the invoke path. Add
  capability metadata (e.g. `supports_tools: bool` / typed
  `UnsupportedCapability` error) to cloud model/provider catalog entries
  (`broker.rs`, `cloud.rs`), surface "text-only" in model listings and
  user-facing docs, and make tool-requiring modes fail **before any
  network access or credential use** with the typed error. Full cloud tool
  translation is explicitly out of scope here (spec §5.5 second
  paragraph); this task delivers the enforced + advertised text-only
  contract.
- **Acceptance criteria:**
  - Selecting a cloud route in a mode that requires tools returns a typed
    unsupported-capability error before credentials/network (test asserts
    zero transport activity, e.g. via stub/no-network guard).
  - Catalog/model listing marks cloud routes text-only; docs updated.
  - Text-only cloud chat behavior is unchanged (existing cloud tests
    green).
  - The invoke-time guard remains as defense in depth.
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-core
  cargo test --test cloud_login_exit
  rustfmt --edition 2021 --check crates/agent-core/src/core/auth/broker.rs \
    crates/agent-core/src/core/auth/cloud.rs
  git diff --check
  ```

- **Dependencies:** T3 (typed outcome plumbing for the error surface).
- **Files likely touched:** `crates/agent-core/src/core/auth/broker.rs`,
  `crates/agent-core/src/core/auth/cloud.rs`, model-catalog surfaces,
  `docs/` user-facing note.
- **Scope:** S

### Task 6 — Phase 1 automated red→green harness

- **Description:** Add a headless, no-human-in-the-loop harness proving all
  Phase 1 acceptance criteria (spec §5 "Phase 1 acceptance criteria") as a
  workspace integration test suite (e.g. `tests/phase1_privacy.rs`):
  log-sentinel scan at all levels, headless failure exit code (spawning
  the binary with a scripted stdin, as `tests/chat_stdin.rs` does —
  interactive input simulated programmatically), Unicode fuzz against the
  byte budget, umask-000 file-mode assertions, symlink attack, and
  tool-required cloud request performing zero network operations. Each
  scenario must be demonstrably red against `d20e03f` behavior (documented
  in the test header) and green now.
- **Acceptance criteria:**
  - One command runs the entire Phase 1 harness headlessly and passes.
  - Every §5 acceptance bullet maps to at least one named test.
  - No test contacts a real provider (stub servers/fixtures only).
  - Harness is wired into normal `cargo test --workspace`.
- **Verification:**

  ```bash
  cargo test --test phase1_privacy
  cargo test --workspace
  git diff --check
  ```

- **Dependencies:** T1, T2, T3, T4, T5.
- **Files likely touched:** `tests/phase1_privacy.rs`, `tests/fixtures/`.
- **Scope:** M

### Checkpoint CP-3 (after T5–T6) — Phase 1 gate

```bash
cargo check --workspace
cargo test --workspace
cargo test --test phase1_privacy
git diff --check
```

Durable artifact: commit "test: phase 1 privacy/correctness harness";
Phase 1 security review requested (spec §15) and holdout oracle verdict
recorded before Phase 2 begins.

---

## Phase 2 — Provider-neutral observability (spec §6)

### Task 7 — Trace envelope schema and `TransportOutcome` types

- **Description:** Create `crates/agent-engine/src/runtime/trace.rs` with
  the versioned `synaps-request-trace/1` envelope (session/turn/request/
  attempt IDs, provider, qualified model, transport, endpoint host/path,
  request anatomy counts/bytes, wire length + digest fields, system/
  message/tool metadata, cache boundaries, translation losses, retries,
  timing stages, usage with provenance, terminal outcome) and a common
  `TransportOutcome` with **optional** timing/usage metrics (never
  fabricated zeros). Add the installation-scoped random HMAC key (private
  file via T4 helpers) for keyed component digests. Types + serde + schema
  test only; no transport wiring yet.
- **Acceptance criteria:**
  - Envelope serializes deterministically and round-trips; a schema test
    validates required fields and version tag.
  - Metadata-only by construction: no field can hold raw content
    (content-bearing export is a separate explicit type in T12).
  - HMAC key is created `0600`, random per installation, and digests are
    keyed (test: same input, different key → different digest).
  - Unknown metrics are `None`, not `0`.
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine
  rustfmt --edition 2021 --check crates/agent-engine/src/runtime/trace.rs
  git diff --check
  ```

- **Dependencies:** T4 (private key file), T3 (`TurnOutcome` in envelope).
- **Files likely touched:** `crates/agent-engine/src/runtime/trace.rs` (new),
  `crates/agent-engine/src/runtime/mod.rs`, `docs/` trace-schema note.
- **Scope:** M

### Task 8 — Anthropic transport emits traces from exact sent bytes

- **Description:** Wire the Anthropic path (`runtime/api.rs`, which already
  serializes `body_bytes: Bytes` once up-front) to populate the trace from
  those exact bytes: wire length + digest computed from `body_bytes`, not
  from re-serialization. Record timing stages (send start, headers, first
  byte, first model event, stream end), retry classes/delays, provider
  request ID, status/stop reason, and usage into `TransportOutcome`.
- **Acceptance criteria:**
  - Trace wire digest equals digest of the bytes handed to the transport
    (test intercepts the stub server's received body and compares).
  - Success, failure, retry, and cancellation fixtures each yield one
    schema-valid trace record.
  - Golden body fixtures unchanged (`body_golden.rs` green) — tracing does
    not alter request construction.
  - Timing buckets populated from independent stub delays.
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine
  cargo test -p synaps-engine -- --test-threads=1
  rustfmt --edition 2021 --check crates/agent-engine/src/runtime/api.rs
  git diff --check
  ```

- **Dependencies:** T7.
- **Files likely touched:** `crates/agent-engine/src/runtime/api.rs`,
  `crates/agent-engine/src/runtime/api_sync.rs`,
  `crates/agent-engine/src/runtime/trace.rs`.
- **Scope:** M

### Task 9 — Normalized conversation IR and `TranslationReport`

- **Description:** Add a provider-neutral IR (ordered system segments;
  blocks: text, reasoning metadata, tool call, tool result with error
  state, media, unknown opaque provider block) plus `TranslationReport`
  listing dropped/merged/renamed/synthesized/downgraded/unsupported
  elements. Implement the Anthropic adapter first as reference (must be
  lossless for current fixtures), producing wire request + report.
- **Acceptance criteria:**
  - Anthropic adapter output is byte-identical to current request bodies
    for all golden fixtures (behavior-preserving; existing tests green).
  - Every non-representable element appears in the report; a fixture with
    a deliberately unsupported block yields an explicit report entry, not
    silent loss.
  - IR fixtures are cross-provider reusable (stored under
    `tests/fixtures/`).
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine
  git diff --check
  ```

- **Dependencies:** T7; uses T2 for any bounded previews in reports.
- **Files likely touched:** new `crates/agent-engine/src/runtime/transport.rs`
  (IR + report per spec §11), `crates/agent-engine/src/runtime/request.rs`,
  `tests/fixtures/`.
- **Scope:** L (split internally: IR types+Anthropic adapter commit, then
  fixture corpus commit)

### Checkpoint CP-4 (after T7–T9)

```bash
cargo check --workspace
cargo test -p synaps-engine -- --test-threads=1
cargo test --workspace
git diff --check
```

Durable artifact: three commits (trace schema, Anthropic trace wiring,
normalized IR); golden Anthropic bytes provably unchanged.

### Task 10 — Extend `TransportOutcome` to all providers

- **Description:** Wire OpenAI chat/Responses, Gemini, Vertex, cloud
  broker, and extension-provider transports to return `TransportOutcome`
  and emit trace records via the shared envelope, using each provider's
  local stub fixtures (existing `tests/extension_provider_*`,
  `google_gemini_runtime_e2e`, `xai_runtime_e2e` patterns). Per-provider
  translation via the T9 IR may land incrementally, but each transport
  must at least report outcome/timing/usage honestly (`None` where
  unknown).
- **Acceptance criteria:**
  - All supported providers emit one schema-valid trace for success,
    failure, retry, and cancellation stub fixtures.
  - No provider fabricates zero metrics.
  - Existing provider e2e/stub tests stay green.
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine
  cargo test --test extension_provider_streaming
  cargo test --test google_gemini_runtime_e2e
  cargo test --test xai_runtime_e2e
  git diff --check
  ```

- **Dependencies:** T8, T9.
- **Files likely touched:** `crates/agent-engine/src/runtime/openai/`,
  `crates/agent-engine/src/runtime/google_gemini/`,
  `crates/agent-engine/src/runtime/google_vertex.rs`,
  `crates/agent-core/src/core/auth/broker.rs`, extension provider glue.
- **Scope:** L (one commit per provider family)

### Task 11 — Non-blocking bounded telemetry/trace writer

- **Description:** Rework persistence in
  `crates/agent-engine/src/runtime/telemetry.rs` (549 lines today) into a
  bounded background writer: bounded queue, dropped-record counter,
  concurrency-safe rotation, one warning per persistent failure class,
  bounded shutdown flush, and zero effect on request correctness. Trace
  records from T8/T10 flow through the same writer.
- **Acceptance criteria:**
  - A blocked/slow/broken storage path (e.g. unwritable dir fixture) does
    not delay or fail a model turn (test with stub transport + broken
    sink).
  - Queue overflow increments the dropped counter and warns once.
  - Shutdown flush is bounded in time (test with a slow sink).
  - Existing telemetry content/rotation tests stay green or are updated
    intentionally.
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine
  rustfmt --edition 2021 --check crates/agent-engine/src/runtime/telemetry.rs
  git diff --check
  ```

- **Dependencies:** T7 (record types); independent of T10.
- **Files likely touched:** `crates/agent-engine/src/runtime/telemetry.rs`,
  runtime shutdown path in `crates/agent-engine/src/runtime/mod.rs`.
- **Scope:** M

### Task 12 — Cache-prefix diagnostics, `/context`, and trace export surfaces

- **Description:** Add cache-prefix diagnostics (tools-/system-prefix and
  history-tail bytes + keyed digests, per-segment previous-turn
  match/change, changed tool IDs/order/schema digests, cache reads/writes
  and estimated reused/recomputed bytes) computed alongside T8's exact
  bytes. Expose user surfaces per spec §6.1: `/context`, `/trace next`,
  `/trace status` (TUI + headless slash command), and
  `synaps trace export <turn-or-request-id> --metadata-only`. Content
  export is the separate explicit path: short-lived runtime opt-in,
  warning, recursive redaction, user-selected private (`0600`) destination
  path, bounded retention — this also satisfies §5.1's "raw development
  capture" clause.
- **Acceptance criteria:**
  - `/context` explains system, tools, history, loaded skills/memories,
    and which cache component changed, without content by default.
  - Persisted digests are keyed with the installation HMAC key.
  - `synaps trace export … --metadata-only` writes a schema-valid file
    with mode `0600`; content export requires the explicit opt-in flag and
    passes a recursive-redaction test seeded with sentinel secrets.
  - Diagnostics correctly flag an intentional tool-order change fixture as
    a prefix change.
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine
  cargo test --test tools_export
  git diff --check
  ```

- **Dependencies:** T8, T11.
- **Files likely touched:** `crates/agent-engine/src/runtime/trace.rs`,
  `crates/agent-tui/src/tui/` (thin views), `src/cmd/` (trace subcommand),
  `src/cmd/chat.rs` (slash commands).
- **Scope:** M

### Task 13 — Phase 2 automated harness (timing/conformance)

- **Description:** Add `tests/phase2_trace_conformance.rs`: stub servers
  that independently delay connection/headers/body and fragment SSE
  (spec §13.3), validating timing buckets; per-provider trace schema
  validation for success/failure/retry/cancel; trace-secret exfiltration
  attempt (sentinel secrets in prompts/headers must never appear in any
  persisted trace); slow-storage non-blocking proof. Fully headless.
- **Acceptance criteria:**
  - Every §6 acceptance bullet maps to a named test; all pass headlessly.
  - Timing tests distinguish header delay from first-byte delay.
  - No external network access in any test.
- **Verification:**

  ```bash
  cargo test --test phase2_trace_conformance
  cargo test --workspace
  git diff --check
  ```

- **Dependencies:** T7–T12.
- **Files likely touched:** `tests/phase2_trace_conformance.rs`,
  `tests/fixtures/`.
- **Scope:** M

### Checkpoint CP-5 (after T10–T11), CP-6 (after T12–T13) — Phase 2 gate

```bash
# CP-5
cargo test -p synaps-engine -- --test-threads=1
cargo test --test extension_provider_streaming
git diff --check
# CP-6 (phase gate)
cargo check --workspace
cargo test --workspace
cargo test --test phase2_trace_conformance
git diff --check
```

Durable artifacts: per-provider trace commits (CP-5); harness commit and a
cross-provider conformance review record (CP-6, spec §15). Oracle verdict
before Phase 3.

---

## Phase 3 — Authorization-enforced progressive capability disclosure (spec §7)

### Task 14 — Typed identities and `ToolCatalog`

- **Description:** Introduce `ToolId`, `CatalogGeneration`, `SchemaDigest`,
  `SessionActivationGrant` types (spec §4.1, §12) in
  `agent-core::orchestration` where shared, and a `ToolCatalog`
  (`crates/agent-engine/src/tools/catalog.rs`) holding all known
  capabilities: stable ID, namespace/source, compact summary/tags, schema
  locator + digest, implementation factory, permission/trust provenance,
  side-effect classification placeholder, generation. Populate it from the
  existing `ToolRegistry::new()` construction paths without changing what
  is exposed yet (registry becomes a projection later).
- **Acceptance criteria:**
  - Catalog insertion performs no process start, network access, schema
    exposure, or execution grant (test asserts no MCP/extension spawn).
  - IDs parse/validate at boundaries; malformed or oversized IDs fail
    closed with typed errors.
  - Generation increments on catalog mutation; digests are deterministic.
  - Existing tool behavior unchanged (`tests/tools_export.rs`,
    `cargo test -p synaps-engine` green).
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine
  cargo test --test tools_export
  git diff --check
  ```

- **Dependencies:** Phase 2 complete (measurement in place).
- **Files likely touched:** `crates/agent-engine/src/tools/catalog.rs` (new),
  `crates/agent-core/src/orchestration/`,
  `crates/agent-engine/src/tools/registry.rs`.
- **Scope:** M

### Task 15 — `DiscoveryIndex` and `SessionToolSet`

- **Description:** Add a bounded `DiscoveryIndex` (compact descriptors
  only, strict count + byte budget on results, never full schemas) and a
  `SessionToolSet` (core set + exact activated tools with grants, schema
  digests, runtime leases) keyed by session. New sessions start with zero
  activations.
- **Acceptance criteria:**
  - Search results respect count and byte budgets under adversarial long
    descriptors (uses T2 `BoundedText`).
  - Search never returns full schemas and never starts a process or
    touches the network.
  - New sessions inherit no activation from prior sessions.
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine
  git diff --check
  ```

- **Dependencies:** T14.
- **Files likely touched:** `crates/agent-engine/src/tools/catalog.rs`,
  `crates/agent-engine/src/tools/activation.rs` (new).
- **Scope:** M

### Task 16 — `ExecutionGate` in the stream loop

- **Description:** Insert the spec §7.1 `ExecutionGate` immediately before
  tool execution in `runtime/stream.rs`: resolve wire name → exact
  `ToolId` (including the sanitized-name reverse mapping in
  `registry.rs::runtime_name_for_api`), verify generation + schema digest,
  require core status or an exact grant, re-check source permission/trust,
  apply side-effect/confirmation policy (conservative default until T24),
  acquire implementation only then, execute through existing hook/output
  policy. Initially every currently-registered tool is "core", so behavior
  is preserved while the gate becomes load-bearing.
- **Acceptance criteria:**
  - A forged known-but-unactivated tool call fails before implementation
    lookup/execution with a typed error (failing-first test with a
    deferred fixture tool).
  - Runtime-name and sanitized-name aliases cannot bypass the gate.
  - Stale generation or schema-digest mismatch invalidates a grant.
  - All existing tool-loop tests green (behavior preserved for core set).
  - Subagent authorization invariants untouched (existing subagent tests
    green, e.g. `tests/subagent_tombstone_harness.rs`).
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine
  cargo test --test subagent_tombstone_harness
  cargo test --test c6_cross_mode_contract
  git diff --check
  ```

- **Dependencies:** T14, T15.
- **Files likely touched:** `crates/agent-engine/src/runtime/stream.rs`,
  `crates/agent-engine/src/tools/activation.rs`,
  `crates/agent-engine/src/tools/registry.rs`.
- **Scope:** L (split: gate types+wiring commit, alias/staleness commit)

### Checkpoint CP-7 (after T14–T16)

```bash
cargo check --workspace
cargo test --workspace
git diff --check
```

Durable artifact: catalog/index/gate commits; adversarial forged-call test
green; no behavioral change for default sessions.

### Task 17 — Discovery/activation tools and deterministic bulk updates

- **Description:** Add model-facing `search_tools`, `activate_tools`,
  `search_skills` tools and adapt `load_skill` to stable IDs; activation
  routes through the gate's grant issuance (host-side authorization
  policy: explicit user requests for a known exact tool authorize that
  exact identity without a redundant prompt, per spec §7.3/PR #63
  behavior; model-initiated activation follows confirmation policy).
  Implement `activate_many` as one stable-order schema-generation update
  (spec §7.7); catalog insertion never rebuilds exposed schemas; existing
  API-safe name mappings and collision safety preserved.
- **Acceptance criteria:**
  - Activating one deferred tool adds exactly that schema for that
    session; siblings remain absent.
  - `activate_many` produces one deterministic schema update (byte-stable
    ordering test).
  - Search/activation are credential-free and network-free until an exact
    authorized source requires initialization.
  - Revocation, digest change, or generation change invalidates activation
    (extends T16 tests through the public tool surface).
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine
  cargo test --test tools_export
  git diff --check
  ```

- **Dependencies:** T16.
- **Files likely touched:** `crates/agent-engine/src/tools/` (new tool
  files), `crates/agent-engine/src/tools/registry.rs`,
  `crates/agent-engine/src/tools/activation.rs`.
- **Scope:** M

### Task 18 — Minimize the core exposed set (opt-in flag)

- **Description:** Behind an opt-in config flag (spec §15.3), reduce
  first-request schemas to essential local operations plus discovery/
  authorization gateways; defer specialized subagent lifecycle operations,
  extension tools, and MCP tools. Document the first-request byte budget
  and measure it with T12 diagnostics at 10/100/500/1,000/2,000 dormant
  tools (spec §13.5). Default remains current behavior until quality
  evidence supports flipping (separate future decision, not this task).
- **Acceptance criteria:**
  - With the flag on, the first request includes exactly the configured
    core schemas and stays below the documented byte budget independent of
    dormant tool count (benchmark test at the five catalog sizes).
  - Dormant built-in, extension, MCP, and skill bodies are absent from the
    first request.
  - With the flag off, request bytes are identical to before (golden
    fixtures green).
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine
  cargo test --test tools_export
  git diff --check
  ```

- **Dependencies:** T17; T12 (measurement).
- **Files likely touched:** `crates/agent-engine/src/tools/registry.rs`,
  config surfaces in `crates/agent-core/src/core/config.rs` (mind the
  pre-existing fmt diffs — do not reformat unrelated code),
  `docs/` budget note.
- **Scope:** M

### Task 19 — MCP per-exact-tool activation with leases

- **Description:** Rework `crates/agent-engine/src/mcp/lazy.rs` (currently
  a single `McpConnectTool`) so that before exact selection only local
  config and safe cached descriptors are read (no process, no network);
  after authorization, start only the selected server, initialize/list
  once, validate returned names/schemas against expected digests, activate
  only requested tools, hold a session lease, invalidate on config
  fingerprint or generation change, terminate on session end/revocation/
  idle.
- **Acceptance criteria:**
  - Search over MCP descriptors starts zero processes (process-spawn spy
    test).
  - Selecting one MCP tool starts exactly one server and grants no
    siblings.
  - Config fingerprint change invalidates the lease and grants.
  - Session end terminates leased processes (no leaked children).
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine
  git diff --check
  ```

- **Dependencies:** T16, T17.
- **Files likely touched:** `crates/agent-engine/src/mcp/lazy.rs`,
  `crates/agent-engine/src/mcp/` siblings,
  `crates/agent-engine/src/tools/activation.rs`.
- **Scope:** L (split: descriptor cache commit, lease lifecycle commit)

### Checkpoint CP-8 (after T17–T19)

```bash
cargo check --workspace
cargo test --workspace
git diff --check
```

Durable artifact: activation-tool, core-set-flag, and MCP-lease commits;
first-request byte-budget benchmark output recorded in the commit message.

### Task 20 — Capability-driven extension lifecycle

- **Description:** Classify extensions (tool-only, provider, hook/
  lifecycle, UI/sidecar) per spec §7.5 and defer spawn accordingly:
  tool-only stay metadata until exact activation; providers start on
  provider/model selection; hooks start only for authorized subscriptions
  (explicit eager status where unavoidable); sidecars stay user-triggered.
  Manifest validation and permission checks remain before spawn; runtime
  declarations must match manifest identities and schema digests.
- **Acceptance criteria:**
  - Tool-only extension processes do not start at boot or during search
    (spawn spy test); activation of one exact tool starts that extension
    only.
  - A runtime tool name/schema mismatching the manifest is rejected.
  - Existing extension e2e suites stay green
    (`extensions_e2e`, `extension_provider_*`, `extensions_process`).
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine
  cargo test --test extensions_e2e
  cargo test --test extensions_process
  git diff --check
  ```

- **Dependencies:** T16, T17.
- **Files likely touched:** `crates/agent-engine/src/extensions/` (per spec
  §11), `crates/agent-engine/src/tools/extension.rs`.
- **Scope:** L (split by extension class)

### Task 21 — Lazy skill bodies

- **Description:** Change `skills/loader.rs::load_all` and the skill
  registry so boot reads only bounded metadata/frontmatter, provenance,
  path, hash, size; full body read/substitution/validation/context
  insertion happens only on selection (`load_skill`/`search_skills`).
  Large catalogs use the bounded discovery index instead of a linearly
  growing schema description.
- **Acceptance criteria:**
  - Boot with N skills reads no skill bodies (I/O spy or content sentinel
    test); first request contains no dormant skill bodies.
  - Selecting a skill loads exactly that body, validated against its
    recorded hash.
  - Existing skill/plugin suites green (`skills_plugin`,
    `pi_skills_compat`).
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine
  cargo test --test skills_plugin
  cargo test --test pi_skills_compat
  git diff --check
  ```

- **Dependencies:** T15 (index), T17 (`search_skills`).
- **Files likely touched:** `crates/agent-engine/src/skills/loader.rs`,
  `crates/agent-engine/src/skills/registry.rs`,
  `crates/agent-engine/src/skills/tool.rs`.
- **Scope:** M

### Task 22 — Phase 3 adversarial harness

- **Description:** Add `tests/phase3_activation.rs` implementing the
  §13.4 activation scenarios headlessly: forged unactivated names and
  aliases, sibling/provider-wide escalation attempts, process/network
  activity before activation (spawn + socket spies), cross-provider
  logical-tool-set equivalence after translation, and
  no-inherited-activation across sessions. Consent prompts are simulated
  programmatically via the host authorization policy hooks.
- **Acceptance criteria:**
  - Every §7 acceptance bullet maps to a named test; all pass headlessly.
  - Harness runs with the core-set flag both off and on.
  - Zero external network access.
- **Verification:**

  ```bash
  cargo test --test phase3_activation
  cargo test --workspace
  git diff --check
  ```

- **Dependencies:** T14–T21.
- **Files likely touched:** `tests/phase3_activation.rs`, `tests/fixtures/`.
- **Scope:** M

### Checkpoint CP-9 (after T20–T22) — Phase 3 gate

```bash
cargo check --workspace
cargo test --workspace
cargo test --test phase3_activation
git diff --check
```

Durable artifact: extension/skill/harness commits; Phase 3 security review
and oracle verdict recorded before Phase 4.

---

## Phase 4 — Bounded, side-effect-aware agent execution (spec §8)

### Task 23 — `TurnBudget` enforcement in the stream loop

- **Description:** Add the spec §8.1 `TurnBudget` struct with per-role
  defaults (foreground, autonomous/watcher, worker) and enforce it in
  `runtime/stream.rs`: provider rounds, tool calls, elapsed time,
  accumulated tool-result bytes, optional tokens/cost. Exhaustion emits
  valid synthetic `tool_result`s for unresolved calls, finalizes valid
  history, and returns `TurnOutcome::BudgetExceeded { dimension }` (enum
  from T3).
- **Acceptance criteria:**
  - A stub model requesting tools forever stops at exactly the configured
    round/call budget (failing-first test).
  - Every emitted `tool_use` retains a matching valid `tool_result` at
    exhaustion.
  - Existing auto-turn cap behavior in `src/cmd/chat.rs` composes with
    (does not duplicate) the engine budget.
  - Budgets are configurable per role through typed config.
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine
  cargo test --test chat_stdin
  git diff --check
  ```

- **Dependencies:** T3, T16 (gate emits ledger-friendly events later).
- **Files likely touched:** `crates/agent-engine/src/runtime/stream.rs`,
  `crates/agent-engine/src/runtime/types.rs`, config surfaces.
- **Scope:** M

### Task 24 — `ToolEffect` metadata and concurrency policy

- **Description:** Add spec §8.2 `ToolEffect` (ReadOnly / IdempotentWrite /
  NonIdempotent) plus optional concurrency key, cancellation support,
  idempotency-key support, and commit semantics to catalog entries;
  classify built-ins (`read`/`grep`/`find`/`ls` read-only; `write`/`edit`
  keyed by canonical path; `bash` NonIdempotent). Unknown/dynamic
  (MCP/extension) tools default to NonIdempotent, serialized. Scheduler:
  only read-only or proven non-conflicting calls run concurrently; same
  concurrency key runs in model order.
- **Acceptance criteria:**
  - Two writes to the same canonical path execute serially in model order
    (test with deliberate reordering pressure).
  - Independent read-only tools may overlap (observed concurrency test).
  - Unclassified tools serialize by default.
  - Result ordering in `tool_result` blocks always matches model request
    order regardless of completion order.
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine
  cargo test --test tools_export
  git diff --check
  ```

- **Dependencies:** T14 (catalog fields), T16 (gate applies policy).
- **Files likely touched:** `crates/agent-engine/src/tools/catalog.rs`,
  built-in tool files under `crates/agent-engine/src/tools/`,
  `crates/agent-engine/src/runtime/stream.rs`.
- **Scope:** M

### Task 25 — Tool-call ledger and interrupted-side-effect handling

- **Description:** Add the typed ledger
  `planned -> authorized -> started -> committed -> result_recorded`
  per tool call. On cancellation/transport failure after possible side
  effect but before result recording, report
  `TurnOutcome::InterruptedAfterSideEffect { call_id }` / unknown outcome;
  never auto-rerun a NonIdempotent operation with unknown commit status.
- **Acceptance criteria:**
  - Cancellation immediately after a committed non-idempotent stub tool
    yields `InterruptedAfterSideEffect` and no automatic rerun
    (failing-first test).
  - Ledger states are monotonic; invalid transitions are unrepresentable
    or rejected.
  - Retry of idempotent operations remains permitted and tested.
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine
  git diff --check
  ```

- **Dependencies:** T23, T24.
- **Files likely touched:** `crates/agent-engine/src/runtime/stream.rs`,
  `crates/agent-engine/src/tools/activation.rs` (or new ledger module).
- **Scope:** M

### Checkpoint CP-10 (after T23–T25)

```bash
cargo check --workspace
cargo test --workspace
git diff --check
```

Durable artifact: budget, effect/scheduler, and ledger commits; infinite
tool-loop and cancel-after-commit tests green.

### Task 26 — Bounded channels and production-time output limits

- **Description:** Replace unbounded model/tool delta queues on high-volume
  paths with bounded channels plus explicit backpressure/coalescing;
  enforce independent UI-preview and model-history byte budgets (via T2
  `BoundedText`) at production time — never materializing full output
  first — with an optional private (`0600`, via T4) spill-to-disk
  artifact and dropped/coalesced counters. Cancellation closes forwarding
  tasks and releases producers.
- **Acceptance criteria:**
  - A synthetic 1 GiB output stream with a slow consumer keeps RSS bounded
    (harness asserts a memory ceiling).
  - UI and model-history outputs obey independent byte budgets.
  - Dropped/coalesced byte and chunk counts are reported.
  - Cancellation leaves no forwarding tasks or runtime leases alive
    (task-count assertion after cancel).
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine -- --test-threads=1
  git diff --check
  ```

- **Dependencies:** T2, T4, T23.
- **Files likely touched:** `crates/agent-engine/src/runtime/stream.rs`,
  `crates/agent-engine/src/tools/` output plumbing (spec §11 suggests
  `tools/output.rs`), `crates/agent-engine/src/tools/send_channel.rs`.
- **Scope:** L (split: bounded channels commit, output-budget/spill commit)

### Task 27 — Correlated execution events

- **Description:** Extend tool lifecycle events with session/turn/request/
  tool-call IDs, stable `ToolId`, wire name, timing, result size,
  truncation flag, activation grant reference, effect class, and commit
  status; feed the same correlation into the T7 trace envelope.
- **Acceptance criteria:**
  - Every execution event for a fixture turn shares consistent IDs and
    appears in the turn's trace record.
  - Parallel completions preserve model-request order in returned
    `tool_result` blocks (re-asserted at the event layer).
  - No event carries raw content beyond bounded previews.
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine
  git diff --check
  ```

- **Dependencies:** T24, T25; T7.
- **Files likely touched:** `crates/agent-engine/src/runtime/stream.rs`,
  `crates/agent-engine/src/runtime/trace.rs`, event types.
- **Scope:** S

### Task 28 — Phase 4 automated harness

- **Description:** Add `tests/phase4_bounds.rs`: infinite tool loop stops
  at budget; 1 GiB synthetic output with slow consumer under an RSS
  ceiling; same-path write serialization; read-only overlap;
  cancel-after-commit non-duplication; cancellation leak check. All
  headless, stub-only.
- **Acceptance criteria:**
  - Every §8 acceptance bullet maps to a named test; all pass headlessly.
  - Suite runtime is bounded (large-stream test uses synthetic generators,
    not real 1 GiB files on disk).
- **Verification:**

  ```bash
  cargo test --test phase4_bounds
  cargo test --workspace
  git diff --check
  ```

- **Dependencies:** T23–T27.
- **Files likely touched:** `tests/phase4_bounds.rs`, `tests/fixtures/`.
- **Scope:** M

### Checkpoint CP-11 (after T26–T28) — Phase 4 gate

```bash
cargo check --workspace
cargo test --workspace
cargo test --test phase4_bounds
git diff --check
```

Durable artifact: bounded-channel, event, and harness commits; Phase 4
security review and oracle verdict before Phase 5.

---

## Phase 5 — Consistent context management and project memory (spec §9)

### Task 29 — Centralized request-aware context budgeting

- **Description:** Add `crates/agent-engine/src/runtime/context.rs`
  computing effective context from actual system segments, exposed
  schemas, history + framing, loaded skills/memories, thinking reserve,
  next tool-result reserve, output reserve, and provider window with
  safety margin (≥10–15% reserve before dispatch). Use provider
  tokenizers where available, conservative estimators otherwise; replace
  frontend-local `estimate_tokens` threshold logic (e.g. in
  `src/cmd/chat.rs`) with the engine calculation.
- **Acceptance criteria:**
  - Compaction triggers before provider exhaustion across English, code,
    JSON, CJK, emoji, tool-heavy, and skill-heavy fixtures with the
    documented reserve.
  - All frontends consume the same engine budget calculation (no
    per-frontend token math left on the trigger path).
  - Estimators are conservative: never overstate remaining capacity in
    fixture tests.
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine
  cargo test --test chat_stdin
  git diff --check
  ```

- **Dependencies:** Phase 3 (real exposed-schema set), T12 (anatomy data).
- **Files likely touched:** `crates/agent-engine/src/runtime/context.rs`
  (new), `crates/agent-engine/src/runtime/stream.rs`, `src/cmd/chat.rs`,
  TUI/RPC adapters.
- **Scope:** M

### Task 30 — Unified compaction transition with typed provenance

- **Description:** Extract one engine operation applying successful
  compaction for all frontends (spec §9.2): successor-vs-in-place policy,
  session ID/chain advancement, token/cost accounting, prompt provenance,
  pending events/queued messages, hooks, save ordering/rollback, parent
  retention. Replace `src/cmd/chat.rs`'s inline
  `<context-summary>` user-message splice and equivalent TUI/RPC/server/
  watcher/subagent paths. Persist the spec §9.3 typed summary artifact
  (source session + range digest, summary provider/model, time,
  prompt-stack digest, content classes, redaction policy, schema version);
  the old system prompt stays typed metadata, never a plain user message.
- **Acceptance criteria:**
  - Every frontend compacts through the same engine transition and
    produces equivalent logical history (cross-mode test).
  - Summary provenance fields persist and round-trip; sessions saved
    before this change still load (backward-compat test).
  - Wrapper/escaping injection in a summary cannot elevate to system
    policy (adversarial fixture).
  - Rollback on failed save leaves the prior session intact.
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine
  cargo test -p synaps-core
  cargo test --test c6_cross_mode_contract
  git diff --check
  ```

- **Dependencies:** T29.
- **Files likely touched:** `crates/agent-engine/src/runtime/compaction.rs`,
  `crates/agent-core/src/core/session.rs`, `src/cmd/chat.rs`, frontend
  adapters.
- **Scope:** L (split: engine transition commit, provenance schema commit)

### Task 31 — Compaction disclosure policy and local-only mode

- **Description:** Before remote compaction, surface provider/model and
  approximate disclosure; add policy controls for thinking, tool results,
  paths, event data, and sensitive categories; implement a local-only mode
  performing no HTTP/network construction (spec §9.4).
- **Acceptance criteria:**
  - Local-only compaction performs zero network operations (socket spy).
  - Policy excludes configured categories from the summarization request
    (sentinel test per category).
  - Disclosure summary (provider, model, approximate bytes/classes) is
    available to every frontend before dispatch.
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-engine
  git diff --check
  ```

- **Dependencies:** T30.
- **Files likely touched:** `crates/agent-engine/src/runtime/compaction.rs`,
  config surfaces, frontend prompts (simulated in tests).
- **Scope:** M

### Checkpoint CP-12 (after T29–T31)

```bash
cargo check --workspace
cargo test --workspace
git diff --check
```

Durable artifact: context/compaction commits; multilingual reserve fixtures
and local-only network-zero proof green.

### Task 32 — Project-scoped progressive memory primitives

- **Description:** Extend `crates/agent-core/src/memory/store.rs` with
  stable record IDs and project scope, and add model-facing tools
  `memory_search` (bounded descriptors/snippets), `memory_fetch` (exact
  IDs), `memory_store` (explicit project, provenance, sensitivity,
  retention), `memory_forget` (tombstone/delete) registered as deferred
  tools through the T14 catalog. No record body in the first request;
  results are lower-authority data; cross-project reads fail closed.
  Backward-compatible loading of existing JSONL records.
- **Acceptance criteria:**
  - First-turn context contains no memory bodies (request-anatomy
    assertion via T12).
  - Cross-project fetch/search fails closed (failing-first test).
  - `memory_forget` tombstones/deletes and subsequent search excludes the
    record.
  - Existing memory records still load (compat fixture).
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-core
  cargo test -p synaps-engine
  cargo test --test extensions_memory
  git diff --check
  ```

- **Dependencies:** T14/T17 (deferred tool registration), T4 (private
  files).
- **Files likely touched:** `crates/agent-core/src/memory/`,
  `crates/agent-engine/src/tools/` (memory tool files).
- **Scope:** L (split: store/schema commit, tools commit)

### Task 33 — Local retrieval index (dependency-gated)

- **Description:** Implement a staged lexical index (preferred: SQLite
  FTS5) with text/tag/project/timestamp indexes, bounded pagination,
  crash-safe append/update, and optional local embeddings behind explicit
  opt-in; no implicit remote embeddings. **Ask-first gate:** adding the
  SQLite dependency requires the documented trade-off and explicit
  operator review (spec §2.10, §14 "Ask first") before any code lands; if
  declined, fall back to an in-repo inverted-index file format meeting the
  same bounds.
- **Acceptance criteria:**
  - Retrieval memory usage is proportional to result limit, not total
    matches (benchmark at 1K/10K/100K records; 1M documented if runtime
    permits).
  - Kill-during-append fixture recovers without corruption.
  - No network access in any index path.
  - Dependency decision recorded in `docs/` before implementation.
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-core
  git diff --check
  ```

- **Dependencies:** T32; explicit operator approval for the dependency.
- **Files likely touched:** `crates/agent-core/src/memory/` (index module),
  `crates/agent-core/Cargo.toml` (if approved), `docs/` trade-off note.
- **Scope:** L (split: index core commit, migration/benchmark commit)

### Task 34 — Disclosure and retention policy

- **Description:** Add per-record/event disclosure classes (local-only,
  model-visible-after-redaction, model-visible-after-consent,
  persist-never-transmit, never-persist) and a unified retention system
  (max age/disk budget; inspect/export/forget operations) spanning
  sessions, compaction parents, memory, indexes, traces, and logs
  (spec §9.7). Retention must never leave named chains pointing to
  deleted sessions.
- **Acceptance criteria:**
  - Each disclosure class is enforced at the model-visibility boundary
    (sentinel tests per class).
  - Retention sweep honors age/disk budgets and preserves chain
    integrity (failing-first dangling-chain test).
  - Inspect/export/forget operations exist headlessly (CLI or engine API)
    and are tested.
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-core
  cargo test -p synaps-engine
  git diff --check
  ```

- **Dependencies:** T32; T12 (traces under retention).
- **Files likely touched:** `crates/agent-core/src/memory/`,
  `crates/agent-core/src/core/session_index.rs`, retention module (new),
  `src/cmd/` surface.
- **Scope:** M

### Task 35 — Session persistence scalability (journal + snapshots)

- **Description:** Evaluate and implement an append-only session journal
  with periodic atomic snapshots to avoid rewriting large histories on
  every `Session::save` (spec §9.8), preserving crash-recovery guarantees
  and backward-compatible loading of current JSON sessions. This changes a
  persisted schema: per spec §14 "Ask first", the format and migration
  plan require operator sign-off before landing; ship behind
  backward-compatible loading either way.
- **Acceptance criteria:**
  - Save benchmarks at 1 MiB / 10 MiB / 100 MiB histories show bounded
    memory and documented recovery behavior.
  - Kill-during-save fixtures recover to a consistent session.
  - Old-format sessions load unchanged; migration is tested and
    documented.
  - Journal/snapshot files use T4 private modes.
- **Verification:**

  ```bash
  cargo check --workspace
  cargo test -p synaps-core
  git diff --check
  ```

- **Dependencies:** T4; independent of T30–T34 (touches the same file —
  coordinate rebases); operator approval for the schema change.
- **Files likely touched:** `crates/agent-core/src/core/session.rs`,
  `crates/agent-core/src/core/session_index.rs`, `docs/` migration note.
- **Scope:** L (split: journal write path, snapshot/recovery, migration)

### Task 36 — Phase 5 automated harness and program-level benchmarks

- **Description:** Add `tests/phase5_context_memory.rs`: cross-frontend
  compaction equivalence, reserve fixtures (English/code/JSON/CJK/emoji/
  tool-heavy/skill-heavy), local-only network-zero, summary/memory
  injection resistance, first-turn no-memory-bodies, cross-project
  fail-closed, retrieval proportionality, retention chain integrity, and
  large-save bounds. Also consolidate the §13.5 benchmark commands into a
  documented script so regressions require explicit budget updates.
- **Acceptance criteria:**
  - Every §9 acceptance bullet maps to a named test; all pass headlessly.
  - Benchmarks emit machine-readable numbers recorded in the commit.
  - Full workspace suite remains under an agreed runtime budget (see risk
    register R5); slow benchmarks are `#[ignore]`-gated with a documented
    invocation.
- **Verification:**

  ```bash
  cargo test --test phase5_context_memory
  cargo test --workspace
  cargo test --test phase5_context_memory -- --ignored   # benchmarks, on demand
  git diff --check
  ```

- **Dependencies:** T29–T35.
- **Files likely touched:** `tests/phase5_context_memory.rs`,
  `tests/fixtures/`, `docs/` benchmark notes.
- **Scope:** M

### Checkpoint CP-13 (after T32–T34), CP-14 (after T35–T36) — program gate

```bash
# CP-13
cargo check --workspace
cargo test -p synaps-core
cargo test --workspace
git diff --check
# CP-14 — final program verification (spec §10 minus locally-unavailable clippy)
cargo check --workspace
cargo test --workspace
cargo test -p synaps-engine -- --test-threads=1
cargo test --test phase1_privacy
cargo test --test phase2_trace_conformance
cargo test --test phase3_activation
cargo test --test phase4_bounds
cargo test --test phase5_context_memory
cargo build --release
git diff --check
```

Durable artifacts: memory/retention commits (CP-13); final commit series
with all five phase harnesses green, benchmark evidence, and the spec §16
checklist walked point-by-point in the final commit message (CP-14).
Clippy (`-D warnings`) is verified by GitHub CI when the operator chooses
to push.

---

## Task-size summary

| Phase | Tasks | XS | S | M | L | Harness task |
| ----- | ----- | -- | - | - | - | ------------ |
| 1     | T1–T6   | 0 | 2 | 4 | 0 | T6  |
| 2     | T7–T13  | 0 | 0 | 5 | 2 | T13 |
| 3     | T14–T22 | 0 | 0 | 6 | 3 | T22 |
| 4     | T23–T28 | 0 | 1 | 4 | 1 | T28 |
| 5     | T29–T36 | 0 | 0 | 4 | 4 | T36 |
| Total | 36      | 0 | 3 | 23 | 10 | 5 |

All L tasks carry explicit internal split points so no single commit
exceeds review budget; there are no XL tasks.

## Risk register

| # | Risk | Likelihood | Impact | Mitigation |
| - | ---- | ---------- | ------ | ---------- |
| R1 | Privacy regression via trace leakage (new trace/export code reintroduces raw content or secrets) | Medium | Critical | Metadata-only types by construction (T7); sentinel-secret exfiltration tests in T6/T13 run in the full suite forever; content export isolated behind explicit opt-in with recursive redaction (T12); holdout oracle attacks this axis specifically. |
| R2 | Cache-prefix invalidation from request changes (tool set, ordering, schema bytes) silently degrades caching/cost | Medium | High | Golden byte fixtures guard every request-construction change (T1, T9, T18); T12 diagnostics measure prefix reuse before/after; intentional changes require fixture updates + migration note + benchmark evidence (spec §2.9) and operator sign-off (spec §14). |
| R3 | Provider behavioral drift (OpenAI/Gemini/cloud/extension paths diverge from the Anthropic reference in outcomes, translation, or tracing) | Medium | High | Shared IR + per-provider `TranslationReport` makes loss explicit (T9/T10); cross-provider conformance fixtures and identical-terminal-outcome tests (T13, §13.2); cross-provider review gate at CP-6. |
| R4 | `unbounded → bounded` channel changes introduce deadlocks or dropped model deltas | Medium | High | T26 lands early in its phase with backpressure tests, slow-consumer harness, and cancellation leak checks; coalescing counters make drops observable rather than silent. |
| R5 | Test-suite runtime growth (36 tasks each adding suites; benchmarks at 100 MiB/1M-record scale) makes red-green cycles impractical | High | Medium | Focused per-phase suites (spec §10); heavyweight benchmarks `#[ignore]`-gated with documented invocation (T36); synthetic generators instead of on-disk gigabyte fixtures (T28); suite-runtime tracked at each phase checkpoint. |
| R6 | Local tooling gaps: no local `cargo clippy`; workspace `cargo fmt --check` broken by pre-existing `config.rs` diffs | Certain | Medium | All verification blocks use `cargo check` + tests + per-file `rustfmt --check` + `git diff --check`; clippy deferred to GitHub CI on operator-initiated push; tasks touching `config.rs` (T18) must not reformat unrelated regions. |
| R7 | Authorization regression: gate refactor (T16) or activation tools (T17) accidentally widen grants or weaken PR #63 subagent invariants | Low | Critical | Invariants listed in per-task DoD; existing subagent tests are release criteria for T16/T17; adversarial escalation harness (T22); security review mandatory for phases 1, 3, 4, 5. |
| R8 | Persistence schema churn (T30 provenance, T32 memory, T35 journal) breaks old sessions/memories | Medium | High | Backward-compat load tests with real old-format fixtures in every persistence task; ask-first gate before any non-compatible schema change; migrations documented and tested. |
| R9 | Dependency review stalls T33 (SQLite) blocking Phase 5 tail | Medium | Medium | Ask-first request raised at CP-12; documented fallback (bounded in-repo index) keeps T34–T36 unblocked. |
| R10 | Same-file contention: T30/T35 both rework `session.rs`; T16/T26 both rework `stream.rs` | Medium | Medium | Ordering fixed in the dependency graph; later task rebases on the earlier one at the intervening checkpoint. |

## Traceability: spec section → tasks

| Spec section | Requirement | Task(s) |
| ------------ | ----------- | ------- |
| §5.1 | Remove raw request content from tracing; opt-in dev capture | T1, T12 |
| §5.2 | Typed terminal failures in all frontends | T3 |
| §5.3 | Centralized UTF-8-safe bounded previews | T2 |
| §5.4 | Private filesystem modes | T4 |
| §5.5 | Honest cloud tool capability | T5 |
| §5 acceptance | Phase 1 harness | T6 |
| §6.1 | Versioned request trace + user surfaces | T7, T12 |
| §6.2 | Trace exact sent bytes | T8 |
| §6.3 | Normalized IR + TranslationReport | T9, T10 |
| §6.4 | Unified TransportOutcome/telemetry | T7, T10 |
| §6.5 | Non-blocking observable telemetry I/O | T11 |
| §6.6 | Cache-prefix diagnostics, keyed HMAC | T7, T12 |
| §6 acceptance | Phase 2 harness | T13 |
| §7.1 | Catalog / DiscoveryIndex / SessionToolSet / ExecutionGate | T14, T15, T16 |
| §7.2 | Discovery and exact activation tools | T17 |
| §7.3 | Minimized core set, ergonomic exact requests | T17, T18 |
| §7.4 | Per-exact-tool MCP activation | T19 |
| §7.5 | Capability-driven extension lifecycle | T20 |
| §7.6 | Lazy skill bodies | T21 |
| §7.7 | Deterministic bulk updates | T17 |
| §7 acceptance | Phase 3 harness | T22 |
| §8.1 | TurnBudget | T23 |
| §8.2 | Tool effect metadata + concurrency | T24 |
| §8.3 | Tool-call ledger | T25 |
| §8.4 | Bounded channels/output | T26 |
| §8.5 | Correlated execution events | T27 |
| §8 acceptance | Phase 4 harness | T28 |
| §9.1 | Centralized context budgeting | T29 |
| §9.2 | Unified compaction transitions | T30 |
| §9.3 | Summary provenance/authority | T30 |
| §9.4 | Compaction disclosure + local-only | T31 |
| §9.5 | Project-scoped progressive memory | T32 |
| §9.6 | Local retrieval index | T33 |
| §9.7 | Disclosure and retention policy | T34 |
| §9.8 | Session persistence scalability | T35 |
| §9 acceptance | Phase 5 harness + benchmarks | T36 |
| §13.4 | Adversarial harnesses | T6, T13, T22, T28, T36 (+ holdout oracle) |
| §13.5 | Performance benchmarks | T18, T26, T33, T35, T36 |
| §16 | Global definition of done | CP-14 checklist |

## Delivery and review

- The branch **stays local**. No task pushes, opens a PR, or touches any
  remote; push and PR creation happen **only on explicit operator
  request**, at which point GitHub CI supplies the clippy gate.
- Commits stay focused (100–300 lines where feasible); L tasks land as
  their documented internal splits.
- Security review is mandatory before closing phases 1, 3, 4, and 5;
  cross-provider conformance review before closing phase 2; no phase gate
  passes without a fresh holdout oracle verdict (spec §15).
- PR #63 remains an explicit dependency of this branch (spec §2.2); its
  subagent authorization invariants are release criteria, never
  regression candidates.
- Any architecture/scope change discovered mid-implementation updates the
  spec first (spec §14 "Always do"), then this plan.
