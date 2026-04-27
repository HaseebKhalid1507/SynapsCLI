# Spec: Dynamic Model Catalog + Abstract Model Routing Handlers

Status: Draft for review
Branch target: `feat/login-enhancements` via a new implementation worktree
Date: 2026-04-27

## Assumptions

1. SynapsCLI remains a Rust CLI/TUI with model routing implemented in `src/runtime/openai/*`, chat UI model browser in `src/chatui/models/*`, and core model capability helpers in `src/core/models.rs`.
2. The current expanded `/model` browser works but is still backed by static provider/model lists plus a generic OpenAI-compatible `/models` fetch.
3. We should prefer official provider model-list endpoints at expansion time, with static fallbacks only when a provider does not expose metadata or the request fails.
4. “OOP handlers” in Rust means trait-based provider/model catalog abstractions, not classical inheritance.
5. No new dependency should be added without explicit approval; use existing `reqwest`, `serde`, and `serde_json` first.
6. Dynamic catalog data may be network-fetched on demand and may be cached in memory during a TUI session; persistent caching is optional unless added to the approved plan.

Correct these before implementation if any are wrong.

## 1. Objective

Build a provider-abstracted model catalog/routing layer so `/model` expansion shows the actual models exposed by each configured API provider, with normalized metadata for labels, context windows, pricing where available, and thinking/reasoning support/modes. Replace hard-coded model routing/thinking logic with extensible handler traits so each provider owns:

- model listing endpoint and auth behavior
- response parsing into a normalized catalog model
- runtime routing/config resolution
- thinking/reasoning capability mapping
- UI display metadata/fallbacks

### Users

- TUI users choosing models via `/model` expand mode.
- CLI users setting `model = provider/model` in config.
- Maintainers adding or updating providers without touching unrelated UI/routing code.

### Success criteria

- Expanding OpenRouter loads the provider’s live model list and shows useful metadata such as context length and reasoning/thinking capability where the API exposes it.
- Expanding other configured OpenAI-compatible providers uses their official `/models`-style endpoint when available and falls back gracefully when metadata is thin.
- Static hard-coded lists are reduced to fallback/seed data behind provider handlers, not duplicated across `chatui/models/mod.rs` and `runtime/openai/registry.rs`.
- Thinking level support is no longer decided only by Anthropic string heuristics in `core/models.rs`; it is represented by a normalized capability model and provider-specific request adapters.
- Existing behavior for Claude/OAuth, OpenAI Codex OAuth, local models, `/thinking`, `/model <id>`, compaction, and streaming remains working.

## 2. Commands

Planning/read-only commands:

```bash
cd /home/jr/Projects/Maha-Media/SynapsCLI
git status --short
git log --oneline --decorate -20
```

Implementation worktree setup after approval:

```bash
cd /home/jr/Projects/Maha-Media/SynapsCLI
git fetch origin --prune
git worktree add -b feat/dynamic-model-catalog ../.worktrees/SynapsCLI-dynamic-model-catalog feat/login-enhancements
cd ../.worktrees/SynapsCLI-dynamic-model-catalog
git branch --show-current
git status --short
```

Targeted tests while developing:

```bash
cargo test runtime::openai::registry
cargo test runtime::openai::catalog
cargo test core::models
cargo test chatui::models
cargo test runtime::api
```

Full verification before push/PR:

```bash
cargo test
cargo build --release
```

Known caveat from prior work: several pre-existing parallel test flakes may fail unrelated to this feature; if seen, rerun affected tests serially and document evidence.

## 3. Project Structure

Expected additions/changes:

- `src/runtime/openai/catalog.rs` — normalized catalog types, provider catalog trait, parsers, fetch orchestration.
- `src/runtime/openai/registry.rs` — provider registry becomes metadata/handler factory; static `models` become fallback seed data.
- `src/runtime/openai/mod.rs` — exports catalog APIs and routes through handler metadata.
- `src/core/models.rs` — retains public convenience functions but delegates to capability abstractions where possible.
- `src/runtime/api.rs` and `src/runtime/openai/stream.rs` — consume normalized thinking/reasoning request adapters instead of scattered hard-coded checks.
- `src/chatui/models/mod.rs` / `src/chatui/mod.rs` — render richer `ExpandedModelEntry` metadata and call `fetch_catalog_models` rather than generic parser directly.
- `docs/specs/2026-04-27-model-routing-dynamic-catalog.md` — this living spec.

Tests should live near the code they validate using existing `#[cfg(test)]` modules.

## 4. Code Style

Prefer small normalized types and provider-specific adapters behind traits/enums. Example style:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    pub id: String,
    pub label: Option<String>,
    pub context_tokens: Option<u64>,
    pub reasoning: ReasoningSupport,
    pub source: CatalogSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningSupport {
    None,
    AnthropicBudget { adaptive: bool },
    EffortLevels { levels: Vec<ReasoningEffort> },
    OpenRouterReasoning { max_tokens: Option<u32>, effort: bool },
    Unknown,
}

pub trait ModelCatalogProvider {
    fn spec(&self) -> &'static ProviderSpec;
    fn fallback_models(&self) -> Vec<CatalogModel>;
    async fn fetch_models(
        &self,
        client: &reqwest::Client,
        keys: &BTreeMap<String, String>,
    ) -> Result<Vec<CatalogModel>, CatalogError>;
}
```

Use explicit parser tests with sample JSON from docs. Keep network tests out of unit tests; mock by testing parser functions.

## 5. Testing Strategy

- Unit test every provider parser with doc-derived/minimal JSON fixtures.
- Unit test capability inference for Anthropic adaptive/budget thinking, OpenRouter reasoning metadata, generic OpenAI-compatible unknowns, and local fallback.
- Unit test routing resolution for `provider/model` and `openai-codex/model` remains unchanged.
- Unit test chat UI sorting/filtering with richer expanded model entries.
- No live network calls in `cargo test`.
- Manual verification after implementation:
  - launch `synaps chat`, open `/model`, expand OpenRouter with a configured key
  - verify model labels/metadata render, search works, selecting a model writes the correct `provider/model`
  - verify unconfigured provider shows a clear configured-key error

## 6. Boundaries

### Always do

- Use TDD: write failing unit tests before implementation.
- Keep implementation in a dedicated worktree after plan approval.
- Prefer official API docs and cite endpoint behavior in comments/spec where provider-specific.
- Validate and sanitize provider response data; ignore empty model ids.
- Preserve backward-compatible config keys and model ids.
- Keep secrets out of logs, tests, snapshots, commits, and error strings.

### Ask first

- Adding crates/dependencies.
- Adding persistent cache files or changing config schema.
- Removing existing providers from the registry.
- Changing default model or default thinking budget.
- Changing public config syntax for `provider.<key>` or `model`.

### Never do

- Commit API keys or tokens.
- Make unit tests hit live provider APIs.
- Remove existing tests to make the suite pass.
- Implement on the primary checkout.
- Hard-code a new second provider list in chat UI.

## Research Work Items

Use multiple research agents with web access before implementation. Each should return official URLs, endpoint paths, response schema, auth requirements, and reasoning/thinking metadata availability.

Initial split:

1. OpenRouter model metadata and reasoning support.
2. Groq model listing and reasoning metadata.
3. NVIDIA NIM model listing and metadata.
4. Anthropic/OpenAI Codex/OAuth model-list options and limitations.

Research results should be summarized in this spec or an adjacent research appendix before code starts.

## Stakes / Convergence Notes

- This touches request construction/routing and can break user API calls across providers.
- It handles API keys but should not alter auth storage.
- Security risk is moderate: external I/O and secret-bearing requests, but not auth protocol design.
- Blast radius is medium/high because `/model`, runtime routing, compaction, and provider streaming share this path.

Human must choose convergence mode before task breakdown: `none`, `informed`, or `holdout`.

## Research Appendix

### OpenRouter

- Endpoint: `GET https://openrouter.ai/api/v1/models`.
- Auth: none required for model list.
- Detail endpoint: use `links.details`, e.g. `GET https://openrouter.ai/api/v1/models/{provider}/{canonical_slug}/endpoints`; no auth required.
- List response includes rich metadata: `id`, `name`, `canonical_slug`, `context_length`, `architecture.input_modalities`, `pricing`, `top_provider.max_completion_tokens`, `supported_parameters`, `knowledge_cutoff`, `expiration_date`.
- Thinking/reasoning support is discoverable from `supported_parameters`:
  - `reasoning` + `include_reasoning` => can request and expose reasoning.
  - `reasoning_effort` => effort-style models.
  - `verbosity` => Anthropic-style verbosity models through OpenRouter.
  - `pricing.internal_reasoning` indicates separate Gemini thinking-token pricing.
- Request fields: `reasoning: { effort | max_tokens | exclude }`, top-level `include_reasoning`, sometimes top-level `verbosity`.
- Implementation: create an OpenRouter-specific parser/handler instead of using generic `{id,name}` only; cache aggressively in-memory and optionally later persist.

### Groq

- Endpoint: `GET https://api.groq.com/openai/v1/models`; single model: `GET /models/{id}`.
- Auth: `Authorization: Bearer $GROQ_API_KEY`.
- List fields: `id`, `object`, `created`, `owned_by`, `active`, `context_window`, `public_apps`; retrieve adds `max_completion_tokens`.
- Reasoning is **not** exposed in the models response. It must be inferred from provider docs / model-id families.
- Reasoning request parameters: `reasoning_format` (`hidden|raw|parsed`), `include_reasoning`, `reasoning_effort` (`none|default|low|medium|high` depending on family). `reasoning_format` and `include_reasoning` are mutually exclusive.
- Response may include `message.reasoning`; usage may include `completion_tokens_details.reasoning_tokens`.
- Implementation: parse live context/active metadata; merge static/pattern capability rules for reasoning families (`openai/gpt-oss-*`, `qwen/qwen3-*`, etc.). Guard unsupported combinations before send.

### NVIDIA NIM

- Endpoint: `GET https://integrate.api.nvidia.com/v1/models`; single model `GET /v1/models/{org}/{model}`.
- Auth: no auth for model list/detail; `POST /chat/completions` needs `Authorization: Bearer $NVIDIA_API_KEY`.
- List schema is minimal: `id`, `object`, `created`, `owned_by`. No context/capability metadata.
- API has no `thinking`, `reasoning`, `nvext`, or budget fields; schemas use `additionalProperties: false`.
- Thinking is controlled by model choice or system prompt, not API params:
  - Nemotron Ultra/Super: inject `detailed thinking on/off` system prompt.
  - `*-thinking`, `cosmos-reason*`, DeepSeek/Kimi/Magistral variants may emit inline `<think>...</think>`.
- Response has no reasoning metadata; thinking appears inline in `content` and must be split if displayed.
- Implementation: live list + static capability table/heuristics; filter non-chat models; dedupe duplicate ids.

### Anthropic / OpenAI / Codex

- Anthropic endpoint: `GET https://api.anthropic.com/v1/models`, paginated with `limit`, `after_id`, `before_id`; auth is `x-api-key` or `Authorization: Bearer` for OAuth plus `anthropic-version: 2023-06-01`.
- Anthropic model objects may include `max_input_tokens`, `max_tokens`, and `capabilities.thinking`, `capabilities.effort`, `capabilities.context_management`; deserialize as optional and keep existing heuristics as fallback.
- OpenAI public endpoint: `GET https://api.openai.com/v1/models`; auth Bearer; response has `id`, `object`, `created`, `owned_by` only. No context/thinking metadata.
- OpenAI Codex ChatGPT backend: no documented model list; probing `chatgpt.com/backend-api/models` returns 403. Keep a curated static handler list and improve model-not-found errors.
- Codex reasoning is opaque/encrypted (`reasoning.encrypted_content`) and not displayable as thinking blocks.

## Technical Plan

### Components

1. **Normalized catalog model**
   - `CatalogModel`, `ModelCapabilities`, `ReasoningSupport`, `CatalogSource`, `CatalogProviderKind`.
   - Represents id, label, full runtime id, context/output tokens, modalities, pricing summary, reasoning support, source freshness/fallback.

2. **Provider handlers**
   - Trait or enum-dispatched handler per provider family:
     - Anthropic handler
     - OpenRouter handler
     - Groq handler
     - NVIDIA NIM handler
     - Generic OpenAI-compatible handler
     - Codex static handler
     - Local handler
   - Each handler owns list endpoint, auth, parser, fallback model seeds, and capability inference.

3. **Request capability adapter**
   - Maps Synaps thinking level/budget to provider-specific request params:
     - Anthropic: `thinking` + `output_config.effort` or legacy budget fallback.
     - OpenRouter: `reasoning`, `include_reasoning`, `verbosity` as supported.
     - Groq: `reasoning_format` / `reasoning_effort` guards.
     - NVIDIA: system prompt injection or inline `<think>` parsing support.
     - Generic: no reasoning params unless handler marks safe.
   - This can be staged: first catalog/UI capabilities, then request-body adaptation.

4. **UI expanded model browser**
   - Replace static `DEV_MODEL_SELECTIONS` as the source of truth with handler-derived provider sections and fallback seeds.
   - Expand mode calls `fetch_catalog_models(provider_key)` and displays richer metadata (context, reasoning badge, price badge where available).

5. **Compatibility layer**
   - Keep existing `core::models::*` functions as wrappers/fallbacks during migration.
   - Keep current config syntax: `model = provider/model`, `provider.<key> = ...`, `thinking = ...`.

### Dependency Graph

```text
Normalized catalog types
    ├── Provider parser tests and handlers
    │       ├── Catalog fetch orchestration
    │       │       ├── Chat UI expanded browser
    │       │       └── Settings/provider display
    │       └── Capability lookup cache
    │               └── Runtime request adapters
    └── Static fallback seeds
            └── Backward-compatible model sections
```

### Implementation Order

1. Contract-first normalized catalog types and parser fixtures.
2. OpenRouter handler first (richest metadata; highest user-request priority).
3. Generic OpenAI-compatible handler refactor (preserve current behavior).
4. Groq/NVIDIA capability enrichment.
5. Anthropic live list and Codex static handler.
6. UI metadata rendering and fallback cleanup.
7. Request-body thinking adapter migration.

### Risks / Mitigations

- **Provider schemas drift:** deserialize optional fields, ignore unknowns, test minimal fixtures.
- **Network failures in UI:** surface clear error and keep fallback list selectable.
- **Secrets leakage:** never log auth headers or key values; OpenRouter/NVIDIA list calls should omit auth when not required.
- **Breaking routing:** keep `resolve_route` behavior and model id strings unchanged; add tests before refactor.
- **Over-scoping request adapters:** stage catalog/UI first, then provider-specific request adaptation in small tasks.

### Parallel vs Sequential Work

- Parallelizable after catalog type contract: provider parser tests/handlers for OpenRouter, Groq, NVIDIA, Anthropic.
- Sequential: UI contract changes and runtime request adapter migration depend on normalized types.
- Sequential: removing hard-coded lists should happen only after handlers/fallbacks are working.

### Verification Checkpoints

- After catalog types + OpenRouter: parser tests pass and expand OpenRouter returns rich model list.
- After generic refactor: current providers still list models and `/model provider/id` still routes.
- After capability handlers: unit tests prove reasoning/context badges for OpenRouter/Groq/NVIDIA/Anthropic/Codex.
- Before push: targeted test groups, full build, manual `/model` expansion.

## Task Breakdown

> Do not start these until human approves this spec/plan, chooses convergence mode, and a dedicated worktree is created.

### Task 1: Define normalized catalog contract

**Description:** Add catalog types and conversion helpers without changing UI/runtime behavior.

**Acceptance criteria:**
- [ ] `CatalogModel` can represent id, label, provider, context/output tokens, modalities, pricing, and reasoning support.
- [ ] Existing static provider seeds can convert into `CatalogModel`.
- [ ] Unit tests cover empty ids being rejected/filtered.

**Verification:**
- [ ] `cargo test runtime::openai::catalog`

**Dependencies:** None
**Files likely touched:** `src/runtime/openai/catalog.rs`, `src/runtime/openai/mod.rs`
**Scope:** S

### Task 2: Implement OpenRouter rich catalog handler

**Description:** Parse OpenRouter `/models` into `CatalogModel` with context, pricing, modalities, supported parameters, and reasoning mode.

**Acceptance criteria:**
- [ ] Parser handles doc/live fixture with `supported_parameters` and pricing strings.
- [ ] `reasoning`, `include_reasoning`, `reasoning_effort`, `verbosity`, and `internal_reasoning` map to normalized support.
- [ ] Fetch omits auth and returns clear HTTP/parse errors.

**Verification:**
- [ ] `cargo test runtime::openai::catalog::openrouter`

**Dependencies:** Task 1
**Files likely touched:** `src/runtime/openai/catalog.rs`, maybe `src/runtime/openai/registry.rs`
**Scope:** M

### Task 3: Refactor existing generic provider listing behind handler interface

**Description:** Move current `{base_url}/models` behavior into a generic handler while preserving `fetch_provider_models` compatibility.

**Acceptance criteria:**
- [ ] Existing parser tests still pass.
- [ ] Generic providers still resolve API keys from config/env.
- [ ] `fetch_provider_models` can become a wrapper over catalog handler output or remain as compatibility shim.

**Verification:**
- [ ] `cargo test runtime::openai::registry`
- [ ] `cargo test runtime::openai::catalog`

**Dependencies:** Task 1
**Files likely touched:** `src/runtime/openai/registry.rs`, `src/runtime/openai/catalog.rs`
**Scope:** M

### Task 4: Add Groq and NVIDIA capability enrichment

**Description:** Add provider-specific parsers/inference for Groq context/reasoning and NVIDIA static/heuristic capabilities.

**Acceptance criteria:**
- [ ] Groq parser reads `active`, `context_window`, `owned_by` and filters inactive models.
- [ ] Groq reasoning families map to normalized support without sending unsupported params.
- [ ] NVIDIA parser dedupes ids and merges static/heuristic thinking/context metadata.

**Verification:**
- [ ] `cargo test runtime::openai::catalog::groq`
- [ ] `cargo test runtime::openai::catalog::nvidia`

**Dependencies:** Tasks 1, 3
**Files likely touched:** `src/runtime/openai/catalog.rs`, `src/runtime/openai/registry.rs`
**Scope:** M

### Task 5: Add Anthropic live catalog and Codex static handler

**Description:** Support OAuth/API-key Anthropic model list with optional capabilities, and move Codex curated models behind a static handler.

**Acceptance criteria:**
- [ ] Anthropic parser handles paginated response and optional capabilities.
- [ ] OAuth headers/beta headers are represented without exposing tokens.
- [ ] Codex handler clearly marks source as static and no live endpoint.

**Verification:**
- [ ] `cargo test runtime::openai::catalog::anthropic`
- [ ] `cargo test core::auth` if auth helper access changes

**Dependencies:** Task 1
**Files likely touched:** `src/runtime/openai/catalog.rs`, `src/runtime/api.rs`, `src/runtime/openai/mod.rs`
**Scope:** M

### Task 6: Wire expanded `/model` UI to catalog handlers

**Description:** Replace expand-mode generic fetch and static UI model structures with catalog-backed entries and richer badges.

**Acceptance criteria:**
- [ ] Expanding OpenRouter shows live catalog labels and metadata.
- [ ] Expanding configured generic providers still works.
- [ ] Errors and fallback/static states are visible and non-crashing.
- [ ] Selecting any expanded entry still writes `provider/model` exactly.

**Verification:**
- [ ] `cargo test chatui::models`
- [ ] Manual TUI `/model` expand OpenRouter/Groq/NVIDIA/Codex/Local where configured

**Dependencies:** Tasks 2–5
**Files likely touched:** `src/chatui/models/mod.rs`, `src/chatui/mod.rs`, `src/chatui/models/input.rs`
**Scope:** M

### Task 7: Introduce provider-aware thinking request adapter

**Description:** Centralize thinking/reasoning request decisions so hard-coded Anthropic-only logic is no longer the only source of truth.

**Acceptance criteria:**
- [ ] Anthropic request bodies remain byte-shape compatible for existing tested cases.
- [ ] OpenRouter/Groq/NVIDIA adapters only emit provider-supported fields.
- [ ] Existing `core::models` functions delegate/fallback rather than own all decisions.

**Verification:**
- [ ] `cargo test runtime::api`
- [ ] `cargo test runtime::openai`
- [ ] Manual smoke test with one Anthropic and one OpenRouter model

**Dependencies:** Tasks 1–5
**Files likely touched:** `src/runtime/api.rs`, `src/runtime/openai/stream.rs`, `src/core/models.rs`, `src/runtime/openai/catalog.rs`
**Scope:** M

### Checkpoint after Tasks 1–3

- [ ] Catalog contract reviewed.
- [ ] OpenRouter works in tests.
- [ ] Existing provider model list behavior preserved.

### Checkpoint after Tasks 4–6

- [ ] `/model` expand mode is catalog-backed.
- [ ] Hard-coded UI provider list reduced to handler fallback seeds.
- [ ] Manual UI model selection succeeds.

### Final Checkpoint after Task 7

- [ ] Request construction is provider-aware.
- [ ] Full targeted tests pass.
- [ ] Release build succeeds.

## Convergence Policy

```yaml
convergence: informed
threshold: 0.80
axis_weights:
  spec_compliance: 0.35
  code_quality: 0.20
  test_coverage: 0.20
  edge_cases: 0.15
  security: 0.10
max_fix_iterations: 2
max_total_calls: 10
```

Rationale: model routing has medium/high blast radius and external API-key-bearing I/O, but this is not changing auth storage/protocols and will receive human review, so informed convergence gives strong multi-agent review without strict holdout overhead.
