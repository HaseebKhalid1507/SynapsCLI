# Request Lifecycle Hardening and Progressive Context Architecture

**Status:** Draft implementation specification

**Branch:** `feat/request-lifecycle-hardening`

**Base:** PR #63, `fix/anthropic-oauth-transport-identity` at `d20e03f6b9781e03fa80d24880b5c88354cfe43f`

**Worktree:** `/home/jr/Projects/Maha-Media/.worktrees/SynapsCLI-request-lifecycle-hardening`

**Source review:** [“Most Devs Don't Understand How an LLM Request Works”](https://www.youtube.com/watch?v=AxdGI_P11qM) and a static review of SynapsCLI's request, tool, provider, context, compaction, and memory paths.

## 1. Objective

Strengthen SynapsCLI's complete LLM request lifecycle in five ordered implementation phases:

1. **Immediate correctness and privacy hardening**
2. **Provider-neutral observability**
3. **Authorization-enforced progressive capability disclosure**
4. **Bounded, side-effect-aware agent execution**
5. **Consistent context management and project memory**

The target user is anyone running SynapsCLI interactively, headlessly, through RPC/server mode, as an autonomous watcher agent, or through a subagent. Success means the same logical turn is safe, diagnosable, bounded, and semantically consistent regardless of provider or frontend, while optional tools, skills, and memories consume context only after exact session-scoped selection.

This specification deliberately retains SynapsCLI's existing strengths:

- exact provider-qualified model identities;
- credential-broker boundaries;
- authorization before credentials, network, billing, process startup, or worker state;
- full-history semantics without full-history deep copies;
- deterministic request serialization and cache-prefix stability;
- exact-model and exact-tool grants rather than provider-wide grants;
- lazy MCP process startup;
- bounded tool results in model history;
- typed prompt/orchestration policy;
- no silent persistence of session-only grants.

## 2. Assumptions

1. This is a significant, multi-PR architecture program. The five phases define implementation order; each phase may be delivered in one or more reviewable commits or PRs.
2. The new branch intentionally starts from PR #63 rather than `dev`, so PR #63 must be merged first or the eventual change must retain it as an explicit dependency.
3. Native Anthropic request construction and cache behavior are the current reference implementation, but the end state is provider-neutral rather than Anthropic-specific.
4. Raw conversation content, system prompts, tool results, skill bodies, and memory records are sensitive by default.
5. Provider APIs remain stateless from SynapsCLI's perspective: complete effective context may be serialized on each request even when provider-side prompt caching avoids recomputation.
6. Progressive disclosure is an authorization mechanism as well as a token optimization. Omitting a schema alone must never make a hidden tool executable.
7. Project memory is local-first. Remote embedding or summarization services are never used implicitly.
8. Backward compatibility for persisted sessions, current config files, extension manifests, MCP configuration, and existing tool names is required unless a migration is explicitly documented and tested.
9. Existing request-body byte identity must not change accidentally. Intentional cache-prefix changes require golden-fixture updates, migration notes, and cache benchmark evidence.
10. No dependency will be added merely for convenience. A new storage or tokenization dependency requires a documented trade-off and explicit review before introduction.

## 3. Non-goals

- Reproducing Claude Code's quota probe or every private Claude Code request detail.
- Replacing SynapsCLI's current single-last-message cache strategy merely because another client uses more breakpoints.
- Treating prompt caching as a privacy boundary.
- Automatically loading all memories into the system prompt.
- Granting all tools on an MCP server after one exact tool is selected.
- Granting all tools from an extension after one exact tool is selected.
- Persisting session activation grants as favorites or global trust without a separate explicit configuration action.
- Sending prompts, memories, or traces to an external telemetry service.
- Making raw request content part of normal logs at any log level.
- Rewriting all provider transports in a single unreviewable change.

## 4. Architectural principles and invariants

### 4.1 Parse and authorize at boundaries

External identifiers become typed values before policy checks. Important identities include:

- `QualifiedModelId`
- `ToolId`
- `CatalogGeneration`
- `SchemaDigest`
- `SessionActivationGrant`
- `TurnId`, `RequestId`, and `AttemptId`
- project and memory record IDs

Malformed, oversized, unknown, stale-generation, schema-mismatched, unauthorized, and unroutable values fail closed before credentials, process creation, network access, billing, or execution.

### 4.2 Separate knowledge, exposure, activation, and execution

The following states are distinct:

1. A capability is known locally.
2. Its compact descriptor is searchable.
3. Its full schema is exposed for this session.
4. Its implementation is authorized for execution.
5. Its runtime/process is active.

No transition implies a broader transition. In particular, searching never starts a process, exposing one schema never grants siblings, and a forged model call cannot bypass activation.

### 4.3 Keep stable prefixes stable

Stable request material precedes volatile conversation material. Deterministic ordering, exact serialized bytes, and cache boundaries are compatibility surfaces. Dynamic turn context must not casually rewrite stable system or tool prefixes.

### 4.4 Bound every autonomous dimension

Every turn has explicit limits on rounds, tool calls, elapsed time, retained output, context growth, and—where available—tokens or cost. Every stream and queue has bounded memory behavior. Unknown/mutating tools execute conservatively.

### 4.5 Content is data unless policy makes it authority

Tool output, events, compaction summaries, memories, and skill bodies remain lower-authority data. They must not silently become immutable system policy or erase provenance.

## 5. Phase 1 — Immediate correctness and privacy hardening

### 5.1 Remove raw request content from generic tracing

The existing full Anthropic payload trace must be removed or replaced with metadata-only output. Normal logging at every level must exclude:

- message text and blocks;
- system-prompt content;
- tool-result content;
- tool arguments by default;
- skill bodies;
- memory bodies;
- authorization headers, cookies, credentials, and secret-bearing URLs.

Safe log fields may include provider, model, payload bytes, message count, tool count, cache-marker count, request correlation IDs, and non-reversible keyed component digests.

A raw development capture, if retained, must be a separate explicit feature with a short-lived runtime opt-in, warning, redaction, restrictive destination, and bounded retention. Merely enabling `RUST_LOG=trace` must never persist raw prompts.

### 5.2 Preserve typed terminal failures in all frontends

The shared engine must expose terminal outcomes without collapsing errors into success:

```rust
pub enum TurnOutcome {
    Completed,
    Canceled,
    ProviderFailed { code: String, correlation_id: String },
    ToolFailed { tool_id: String, correlation_id: String },
    BudgetExceeded { dimension: BudgetDimension },
    InterruptedAfterSideEffect { call_id: String },
}
```

Requirements:

- `synaps chat` exits nonzero for an unrecovered failure, or emits a structured error in machine-output mode.
- TUI, RPC, server, watcher, and subagents retain the same terminal category and correlation ID.
- Partial assistant output and valid completed tool results survive failure.
- History repair tracks messages appended by the active turn; it must not heuristically remove an arbitrary trailing user/assistant message by role.

### 5.3 Centralize UTF-8-safe bounded previews

One shared utility must define byte-bounded, valid-UTF-8 preview/truncation behavior. It is used by tool history, logs, TUI/headless previews, subagent state, errors, and traces.

The result reports exact units:

```rust
pub struct BoundedText {
    pub text: String,
    pub original_bytes: usize,
    pub retained_bytes: usize,
    pub truncated: bool,
}
```

No direct byte-index slicing of arbitrary UTF-8 strings is allowed.

### 5.4 Enforce private filesystem modes

On Unix, application-owned sensitive state must be created with:

- directories: `0700`;
- files and temporary files: `0600`;
- symlink-safe opening where applicable;
- atomic create/write/rename behavior;
- no interval in which a new file is broader than policy.

Scope includes sessions, session indexes, memory, telemetry/traces, usage logs, and related temporary files. Existing broader files should be detected and either safely repaired or reported once with actionable guidance.

Cross-platform behavior must preserve current functionality while using the strongest available platform controls.

### 5.5 Make cloud tool capability honest

Cloud broker routes currently send no tool schemas. Until full tool translation is supported, model/provider capability metadata and user-facing documentation must mark those routes as text-only. A mode that requires tools must fail before network access with a typed unsupported-capability error.

When cloud tool support is implemented, it must preserve exact tool IDs, arguments, result pairing, usage, cancellation, and execution authorization rather than merely forwarding provider deltas.

### Phase 1 acceptance criteria

- No raw-content sentinel appears in logs at any logging level.
- Headless provider failure returns a non-success outcome and preserves valid partial history.
- Arbitrary Unicode cannot panic or exceed configured retained-byte limits.
- Sensitive files have exact private modes under a permissive test umask.
- Symlink-target tests fail safely.
- Tool-required cloud requests fail locally and perform zero network operations until the route advertises tool support.

## 6. Phase 2 — Provider-neutral observability

### 6.1 Introduce a versioned request trace

Add a provider-neutral trace envelope, for example `synaps-request-trace/1`, covering:

- session, turn, request, and attempt IDs;
- provider, qualified model, transport, endpoint host/path;
- normalized request anatomy;
- final wire byte length and digest;
- system segments by type, bytes, and keyed digest;
- messages by role/block type and bytes;
- exposed tools by stable ID, wire name, schema bytes, and schema digest;
- cache boundaries, TTL, and stable-prefix digests;
- translation losses or synthetic rewrites;
- retries, timing stages, status, stop reason, and provider request ID;
- usage and cache metrics with source/provenance;
- terminal outcome.

Metadata-only is the default. Content export is explicit, temporary, recursively redacted, and written only to a user-selected private path.

Suggested user surfaces:

```text
/context
/trace next
/trace status
synaps trace export <turn-or-request-id> --metadata-only
```

### 6.2 Trace the exact bytes sent

A trace's wire digest and size must derive from the exact serialized bytes passed to the transport. The trace system must not re-run request construction and risk reporting a different payload.

### 6.3 Add a normalized conversation/request IR

Create a provider-neutral representation for ordered system segments and conversation blocks:

- text;
- reasoning/thinking metadata;
- tool call;
- tool result with error state;
- media/attachments where supported;
- unknown opaque provider block.

Provider adapters return both a wire request and a `TranslationReport`. Dropped, merged, renamed, synthesized, downgraded, or unsupported elements must be explicit. Silent semantic loss is not acceptable.

### 6.4 Unify transport outcomes and telemetry

Every transport returns a common `TransportOutcome` with optional—not fabricated zero—metrics:

- send start;
- headers received;
- first byte;
- first model event;
- stream end;
- retry classes and delays;
- provider IDs;
- input/output/cache usage;
- translation report;
- terminal status.

Telemetry must cover successful, failed, retried, and canceled requests across Anthropic, OpenAI chat/Responses, Gemini, cloud, and extension providers.

### 6.5 Make telemetry I/O non-blocking and observable

Telemetry persistence uses a bounded background writer or blocking pool, with:

- bounded queue;
- dropped-record counter;
- concurrency-safe rotation;
- one warning per persistent failure class;
- bounded shutdown flush;
- no effect on request correctness.

### 6.6 Add cache-prefix diagnostics

Without persisting raw content, expose:

- tools-prefix bytes and keyed digest;
- system-prefix bytes and keyed digest;
- history-tail bytes and keyed digest;
- previous-turn match/change per segment;
- changed tool IDs/order/schema digests;
- cache reads/writes and estimated reused/recomputed bytes.

Persisted hashes use an installation-scoped random HMAC key so short prompts cannot be dictionary-attacked.

### Phase 2 acceptance criteria

- All supported providers emit one schema-valid trace record for success, failure, retry, and cancellation fixtures.
- Trace wire digests match sent bytes.
- Default traces contain no raw content or credentials.
- Translation fixtures either preserve normalized meaning or report each loss/rewrite.
- Timing tests independently delay headers and SSE bytes and validate the correct timing buckets.
- Slow or broken trace storage does not delay or fail a model turn.
- `/context` explains system, tools, history, loaded skills/memories, and changed cache component without exposing content by default.

## 7. Phase 3 — Authorization-enforced progressive capability disclosure

### 7.1 Split the current registry

Introduce explicit components:

#### `ToolCatalog`

All locally known capabilities and factories:

- stable `ToolId`;
- namespace/source (`builtin`, `extension`, `mcp`, plugin);
- compact summary and tags;
- full schema locator and digest;
- implementation locator/factory;
- required permissions/trust provenance;
- side-effect classification;
- catalog generation.

Catalog insertion performs no process startup, network access, schema exposure, or execution grant.

#### `DiscoveryIndex`

Bounded searchable descriptors only. Results have a strict count and byte budget and never include full schemas.

#### `SessionToolSet`

The small core set plus exact activated deferred tools, schema digests, activation grants, and runtime leases for one session.

#### `ExecutionGate`

Immediately before execution:

1. Resolve API/wire name to exact `ToolId`.
2. Verify catalog generation and schema digest.
3. Require core status or an exact session activation grant.
4. Re-evaluate source permission/trust.
5. Apply side-effect and confirmation policy.
6. Acquire/start the implementation only when necessary.
7. Execute through existing hook and output policy.

### 7.2 Add local discovery and exact activation tools

Provide compact tools similar to:

- `search_tools`
- `activate_tools`
- `search_skills`
- existing `load_skill`, adapted to stable IDs where needed

Search and activation are credential-free and network-free until an exact source requires initialization after authorization.

### 7.3 Minimize the core exposed set

Initial schemas should be limited to essential local operations and discovery/authorization gateways. Specialized subagent lifecycle operations, extension tools, and MCP tools should be candidates for deferral, subject to measured tool-selection quality.

Existing explicit user requests must remain ergonomic. If a user asks for a known exact tool/model/capability, the host may authorize that exact local identity without a redundant sudo-style prompt, consistent with PR #63's session-only exact-model behavior.

### 7.4 Activate MCP per exact tool

Before exact selection:

- read local config and safe cached descriptors only;
- start no process;
- make no network call.

After authorization:

- start only the selected server;
- initialize/list tools once;
- validate returned names and schemas;
- activate only exact requested tools;
- retain a session lease;
- do not authorize sibling tools;
- invalidate on config fingerprint/schema generation changes;
- terminate on session end, revocation, or idle policy.

### 7.5 Make extension lifecycle capability-driven

Classify extensions:

- tool-only: metadata until exact activation;
- provider: start when provider/model is selected;
- hook/lifecycle: start only when required by an authorized subscription, with explicit eager status where unavoidable;
- UI/sidecar: user-triggered lifecycle.

Manifest validation and permission checks remain before spawn. Runtime declarations must match manifest-declared identities and expected schema digests.

### 7.6 Lazy-load skill bodies

Boot reads bounded metadata/frontmatter, provenance, source path, hash, and size. Full body read, substitution, validation, and context insertion occur only for a selected skill. Large catalogs use bounded search rather than one linearly growing schema description.

### 7.7 Deterministic bulk updates

Catalog insertion does not rebuild exposed schemas. `activate_many` performs one stable-order schema generation update. Existing API-safe name reverse mappings and collision safety remain intact.

### Phase 3 acceptance criteria

- The first request includes exactly the configured core schemas and stays below a documented byte budget.
- Dormant built-in, extension, MCP, and skill bodies are absent.
- Search starts zero MCP/extension processes and performs zero network access.
- Activating one deferred tool adds exactly that schema for that session.
- A forged known-but-unactivated call fails before implementation lookup/execution.
- Runtime-name and sanitized-name aliases cannot bypass activation.
- New sessions inherit no session activation.
- Selecting one MCP tool starts one server and grants no siblings.
- Permission revocation, schema digest change, or catalog generation change invalidates activation.
- All providers expose the same logical active tool set after translation.

## 8. Phase 4 — Bounded, side-effect-aware agent execution

### 8.1 Introduce `TurnBudget`

Every stream session receives explicit limits:

```rust
pub struct TurnBudget {
    pub max_provider_rounds: u32,
    pub max_tool_calls: u32,
    pub max_elapsed: Duration,
    pub max_accumulated_tool_result_bytes: usize,
    pub max_context_tokens: Option<u64>,
    pub max_cost_usd: Option<f64>,
}
```

Budgets differ by foreground, autonomous, and worker role but share one enforcement mechanism. Exhaustion creates valid synthetic results for unresolved calls, emits final valid history, and returns `TurnOutcome::BudgetExceeded`.

### 8.2 Add tool effect metadata

Tools declare conservative effect metadata:

```rust
pub enum ToolEffect {
    ReadOnly,
    IdempotentWrite,
    NonIdempotent,
}
```

They may also expose:

- concurrency key derived from validated input;
- cancellation support;
- idempotency-key support;
- commit/outcome semantics.

Unknown/dynamic tools default to `NonIdempotent` and serialized execution.

Only read-only or proven-safe non-conflicting calls run concurrently. Mutating calls with the same key run in model order.

### 8.3 Track a tool-call ledger

Maintain typed states:

```text
planned -> authorized -> started -> committed -> result_recorded
```

If cancellation or transport failure occurs after a possible side effect but before result recording, report `InterruptedAfterSideEffect`/unknown outcome; never blindly rerun a non-idempotent operation.

### 8.4 Bound channels and output at production time

Replace unbounded model/tool delta queues on high-volume paths with bounded channels and explicit backpressure/coalescing policy. Distinguish:

- UI preview budget;
- model-history budget;
- optional private spill-to-disk artifact;
- dropped/coalesced byte and chunk counts.

Do not materialize arbitrarily large output before applying limits. Cancellation closes forwarding tasks and releases producers.

### 8.5 Correlate all execution events

Tool lifecycle events include session, turn, request, tool call, stable tool ID, wire name, timing, result size, truncation, activation grant, effect class, and commit status. Parallel completion order must not lose model-request order in returned `tool_result` blocks.

### Phase 4 acceptance criteria

- A model requesting tools forever stops at exactly the configured budget.
- Every emitted `tool_use` retains a matching valid `tool_result`, including cancellation and budget exhaustion.
- Two writes to the same canonical path execute serially in model order.
- Independent read-only tools may overlap.
- A non-idempotent committed operation is never automatically duplicated after interruption.
- A synthetic 1 GiB output stream cannot produce unbounded RSS when the consumer is slow.
- UI and model-history outputs obey independent byte budgets.
- Cancellation leaves no forwarding tasks or runtime leases alive.

## 9. Phase 5 — Consistent context management and project memory

### 9.1 Centralize request-aware context budgeting

The engine—not individual frontends—calculates effective context from:

- actual effective system segments;
- exposed tool schemas;
- conversation history and protocol framing;
- loaded skill and memory content;
- reasoning/thinking reserve;
- next likely tool-result reserve;
- requested output reserve;
- provider context window and safety margin.

Use provider tokenizers where available and conservative estimators otherwise. Target at least 10–15% reserved capacity before dispatch.

### 9.2 Unify compaction state transitions

TUI, headless, RPC, server, watcher, and subagents use one engine operation to apply successful compaction. The operation consistently handles:

- linked successor versus explicit in-place policy;
- session IDs and chain advancement;
- token/cost accounting;
- prompt provenance;
- pending events and queued messages;
- hooks;
- save ordering and rollback;
- parent retention/deletion policy.

### 9.3 Preserve summary provenance and authority

A compaction summary is a typed context artifact, not ordinary user text or immutable system policy. Persist:

- source session and message-range digest;
- summary provider/model;
- creation time;
- prompt-stack digest;
- included/excluded content classes;
- redaction policy;
- summary schema version.

Escaping/wrapper injection cannot elevate content. The old system prompt remains typed session/system metadata and is not embedded as a plain user message.

### 9.4 Make compaction disclosure explicit

Before remote compaction, surface the provider/model and approximate disclosure. Policy controls whether thinking, tool results, paths, event data, and other sensitive categories are included. A local-only mode must perform no HTTP/network construction.

### 9.5 Build project-scoped progressive memory

Add stable record IDs and project scope. Model-facing primitives:

- `memory_search`: bounded descriptors/snippets;
- `memory_fetch`: exact selected IDs;
- `memory_store`: explicit project, provenance, sensitivity, and retention;
- `memory_forget`: tombstone/delete workflow.

No record body is sent in the first request. Search/fetch output is lower-authority data, never system policy. Cross-project reads fail closed.

### 9.6 Add a real local retrieval index

Prefer a staged lexical index such as SQLite FTS5, subject to dependency review, with:

- text, tag, project, and timestamp indexes;
- bounded pagination and result allocation;
- crash-safe append/index update;
- optional local embeddings behind explicit opt-in;
- no implicit remote embeddings.

### 9.7 Add disclosure and retention policy

Events and memories carry policies such as:

- display locally only;
- model-visible after redaction;
- model-visible after explicit consent;
- persist but never transmit;
- never persist;
- expiration/retention class.

Unified retention covers sessions, compaction parents, memory, indexes, traces, and logs with maximum age/disk budget and explicit inspect/export/forget operations.

### 9.8 Improve session persistence scalability

Evaluate an append-only session journal with periodic atomic snapshots to avoid cloning and rewriting very large histories on every save. Preserve current crash-recovery guarantees and backward-compatible loading.

### Phase 5 acceptance criteria

- Every frontend compacts through the same engine state transition and produces equivalent logical history.
- Compaction triggers before provider exhaustion with documented reserve across representative English, code, JSON, CJK, emoji, tool-heavy, and skill-heavy fixtures.
- Local-only compaction performs zero network operations.
- Malicious summary/memory wrapper text cannot escape its typed data boundary or override immutable prompt policy.
- First-turn context includes no memory bodies.
- Search and fetch are project-scoped, bounded, sensitivity-aware, and deletable.
- Retrieval memory is proportional to result limit rather than total matches.
- Retention cannot leave named chains pointing to deleted sessions.
- Large-session save benchmarks show bounded memory and documented recovery behavior.

## 10. Commands and developer workflow

Run from the dedicated worktree only:

```bash
cd /home/jr/Projects/Maha-Media/.worktrees/SynapsCLI-request-lifecycle-hardening

cargo fmt --check
cargo check --workspace
cargo test --workspace
cargo test -p synaps-engine -- --test-threads=1
cargo clippy --all-targets -- -D warnings
cargo build --release
git diff --check
```

Focused suites should be added per phase so developers need not run every live/integration fixture during red-green cycles. Final verification still runs the complete workspace suite and the external/adversarial harnesses described below.

No test may contact a real provider unless explicitly marked as an opt-in live test. Provider tests use local stub servers or pure request fixtures.

## 11. Project structure

Expected areas; exact decomposition may evolve through reviewed plans:

```text
crates/agent-core/src/
  core/session*.rs             # private persistence, typed compaction provenance
  memory/                      # project records, index contracts, retention
  orchestration/               # shared authorization/budget domain types where appropriate

crates/agent-engine/src/
  runtime/trace.rs             # request trace schema and redaction
  runtime/context.rs           # provider-aware context accounting
  runtime/transport.rs         # common transport outcome/translation report
  runtime/stream.rs            # turn budgets and execution loop
  runtime/compaction.rs        # common compaction request + provenance
  tools/catalog.rs             # known capability catalog
  tools/activation.rs          # session grants and execution gate
  tools/registry.rs            # active executable projection/compatibility surface
  tools/output.rs              # bounded UTF-8 output/artifact policy
  mcp/                         # per-tool lazy discovery/runtime leases
  extensions/                  # capability-driven lazy lifecycle
  skills/                      # metadata discovery and lazy bodies

crates/agent-tui/src/tui/
  context/trace views and thin adapters only

src/cmd/
  chat/rpc/server/agent adapters that preserve shared outcomes

tests/
  cross-mode and cross-provider conformance
  filesystem privacy
  deferred activation
  budget/side-effect harnesses
  context/compaction/memory end-to-end fixtures

docs/
  this specification, user-facing configuration, trace schema, migrations
```

Avoid creating parallel implementations in each frontend or provider. Domain logic belongs in core/engine, with frontends as adapters.

## 12. Code style

Encode invariants in types rather than booleans or strings. Representative style:

```rust
pub struct ActivationGrant {
    pub session_id: SessionId,
    pub tool_id: ToolId,
    pub catalog_generation: CatalogGeneration,
    pub schema_digest: SchemaDigest,
}

impl ExecutionGate {
    pub fn authorize(
        &self,
        requested: &ResolvedToolCall,
        session: &SessionToolSet,
    ) -> Result<AuthorizedToolCall, ToolAuthorizationError> {
        let grant = session
            .grant_for(&requested.tool_id)
            .ok_or(ToolAuthorizationError::NotActivated)?;
        grant.verify(requested)?;
        self.source_policy.authorize(requested)?;
        Ok(AuthorizedToolCall::new(requested.clone(), grant.clone()))
    }
}
```

Rules:

- Validate external strings once, at boundaries.
- Use stable IDs and typed enums for lifecycle states.
- Make unknown capability/provider states explicit with `Option` or enums, never invented zeros/default support.
- No `unwrap`/`expect` on external data in production paths.
- Preserve deterministic ordering for serialized schemas and traces.
- Keep raw sensitive values out of `Debug`/`Display` where practical.
- Prefer shared pure builders and fixtures over duplicated transport logic.

## 13. Testing strategy

### 13.1 Unit and property tests

- Input/ID/schema validation and fail-closed behavior.
- Recursive trace redaction.
- UTF-8-safe truncation properties.
- Catalog generation and activation grant invariants.
- Effect/concurrency-key classification.
- Context accounting and summary provenance.
- Memory project boundaries and retention.

### 13.2 Golden and conformance tests

- Preserve Anthropic byte fixtures unless intentionally changed.
- Cross-provider normalized-conversation fixtures.
- Translation reports for every unsupported semantic.
- Stable cache-segment digests across multi-turn append-only histories.
- Identical terminal outcomes/history across TUI, chat, RPC/server, watcher, and subagent adapters.

### 13.3 Local transport fixtures

Stub servers independently delay connection/headers/body, return fragmented streams, retryable failures, partial outputs, malformed blocks, usage, and tool calls. Tests make no external network calls.

### 13.4 Adversarial automated harnesses

Use an external test oracle rather than relying only on implementation-authored tests. Required scenarios include:

- forged unactivated tool names and aliases;
- sibling/provider-wide authorization escalation attempts;
- process/network activity before activation;
- Unicode boundary fuzzing;
- symlink and permissive-umask attacks;
- infinite tool loops;
- 1 GiB synthetic output with slow consumers;
- side effect committed immediately before cancellation;
- prompt injection in summary/memory/skill/event data;
- cross-project memory access;
- trace-secret exfiltration attempts.

### 13.5 Performance benchmarks

Track at minimum:

- initial schema bytes with 10, 100, 500, 1,000, and 2,000 dormant tools;
- catalog insertion versus activation rebuild cost;
- cache-prefix reused/rewritten bytes over realistic multi-turn fixtures;
- request serialization/retry cost;
- output-stream RSS under backpressure;
- memory retrieval at 1K, 10K, 100K, and 1M records;
- session save at 1 MiB, 10 MiB, and 100 MiB histories.

Regressions require an explicit rationale and accepted budget update.

## 14. Boundaries

### Always do

- Work only in the dedicated worktree/branch.
- Write failing tests before behavior changes.
- Preserve exact identity and grant only exact session-scoped capabilities.
- Validate before credentials, network, billing, process startup, or execution.
- Keep trace output metadata-only by default.
- Run focused and full verification before commits.
- Update this spec before changing architecture or scope.
- Ship a non-interactive end-to-end harness for every phase, including simulated consent and failure paths.
- Maintain backward-compatible loading or provide a tested migration.

### Ask first

- Adding a production dependency, database, tokenizer, or cryptography crate.
- Changing a persisted session/memory/config/trace schema without backward compatibility.
- Changing public extension, MCP, RPC, or server protocols.
- Changing CI/release configuration.
- Making an intentional fleet-wide Anthropic request-byte/cache-prefix change.
- Enabling any remote telemetry, remote embeddings, or remote content export.
- Persisting activation grants beyond a session.

### Never do

- Commit or log credentials, raw prompts, raw histories, raw tool results, or raw memory by default.
- Treat schema omission as sufficient execution authorization.
- Start MCP/extension processes or make network calls during local search.
- Grant provider-wide, server-wide, extension-wide, or sibling capability access from one exact request.
- Silently persist session grants to favorites or trust configuration.
- Swallow a terminal error as successful completion.
- Retry a non-idempotent side effect whose commit status is unknown.
- Remove or weaken failing tests to make a phase pass.
- Use the modified primary checkout for implementation.

## 15. Delivery and review strategy

This work touches secrets, authorization, plugins/sidecars, shell/filesystem I/O, network transports, persistence, and autonomous execution. It has high blast radius and requires **holdout convergence** for architecture and security-sensitive phases:

- Builder and adversarial oracle use isolated worktrees/write scopes.
- Security review is mandatory for phases 1, 3, 4, and 5.
- Cross-provider conformance review is mandatory for phase 2.
- No autonomous merge without a fresh passing oracle verdict and full verification evidence.
- Keep commits reviewable—prefer focused changes around 100–300 lines where feasible; split larger migrations behind compatibility layers or feature flags.

Recommended delivery sequence:

1. Phase 1 as focused correctness/privacy changes.
2. Phase 2 trace/transport contracts before progressive disclosure, so changes can be measured.
3. Phase 3 catalog/activation behind an opt-in flag, then default after parity and quality evidence.
4. Phase 4 budget and effect enforcement, initially conservative.
5. Phase 5 context/memory changes with explicit persistence migrations.

## 16. Global definition of done

The program is complete only when:

1. Normal logs never contain raw model-facing content.
2. Sensitive state is application-enforced private on disk.
3. All frontends preserve one typed terminal outcome contract.
4. Every provider produces comparable safe request traces and explicit translation-loss reports.
5. First-turn schemas remain within a documented budget independent of dormant capability count.
6. Deferred activation is exact, session-only, authorization-enforced, and process/network-free before selection.
7. Agent turns, streams, queues, outputs, and side effects are bounded and recoverable.
8. Every frontend uses one request-aware context/compaction policy.
9. Memory is local-first, project-scoped, progressively disclosed, indexed, sensitivity-aware, and deletable.
10. Cache behavior remains measured, deterministic, and regression-tested.
11. Full workspace checks, external adversarial harnesses, security review, and provider conformance all pass.
