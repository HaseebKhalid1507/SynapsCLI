# Dynamic exact-model reasoning modes: native Anthropic and xAI

Builds on `docs/provider-reasoning-modes-spec.md` (Codex slice). The shared
typed domain remains `agent_core::reasoning::ReasoningLevel`; this spec extends
exact-model capability validation, dynamic option derivation, and wire
construction to the native Anthropic (`anthropic/<id>`, legacy bare `claude-*`)
and xAI (`xai-auth/<id>`, OpenAI Responses wire) routes.

## Objective

Expose, validate, persist, and send provider/model-exact reasoning modes for
native Anthropic and xAI using only source-controlled or live-catalog capability
evidence. No capability may ever be inferred from model-name substrings.
Unsupported or unknown combinations fail closed with a clear message and leave
runtime state and persisted config unchanged. Legacy numeric Anthropic budgets
stay exact and backward-compatible.

## Capability evidence (authoritative tables)

### Anthropic

Live `GET /v1/models` exposes `capabilities.thinking.supported` and
`capabilities.effort.supported`. These fields are authoritative for broad
thinking / adaptive-effort support. When capabilities are absent the model is
`Unknown` (conservative fallback set); when `thinking.supported == false` the
model is `None` (named reasoning rejected).

Exact static fallback descriptors (only ids with explicit source-controlled
evidence in `crates/agent-core/src/core/models.rs` `KNOWN_MODELS` +
adaptive-thinking notes):

| Exact model id | Thinking | Adaptive effort | Named effort set |
|---|---|---|---|
| `claude-opus-4-7` | yes | yes | low, medium, high, xhigh |
| `claude-fable-5` | yes | yes | low, medium, high, xhigh |
| `claude-sonnet-4-6` | yes | no (fixed budget) | — (budget tiers only) |
| `claude-opus-4-6` | yes | no (fixed budget) | — (budget tiers only) |
| `claude-haiku-4-5-20251001` | yes | no (fixed budget) | — (budget tiers only) |

No per-model named-effort sets are invented for other ids.

### xAI (official docs evidence only)

| Exact model id | Reasoning | Named effort set | Default | Can disable |
|---|---|---|---|---|
| `grok-4.5` | yes | low, medium, high | high | no |
| `grok-4.5-latest` | yes | low, medium, high | high | no |
| `grok-4.20-multi-agent-0309` | yes | low, medium, high, xhigh (effort = agent count) | undocumented | no evidence → fail closed |
| `grok-4.3`, `grok-4.3-latest`, `grok-latest`, `grok-4.20-0309-reasoning` | yes (intrinsic) | none documented | provider default | no evidence → fail closed |
| `grok-4.20-0309-non-reasoning` | no | none | — | trivially off |

No other exact xAI ids are assumed to support effort.

## Semantics per level and provider

| Level | Anthropic adaptive (`opus-4-7`, `fable-5`) | Anthropic fixed budget | xAI effort models (4.5/4.5-latest, multi-agent) | xAI reasoning w/o effort | xAI non-reasoning |
|---|---|---|---|---|---|
| Off | omit `thinking` and `output_config` | omit `thinking` | **reject** (reasoning cannot be disabled) | **reject** (no disable evidence) | accept; omit `reasoning` |
| Adaptive | `thinking:{type:"adaptive",display:"summarized"}`, no `output_config.effort` | `enabled` + fallback budget (legacy, unchanged) | omit `reasoning` → documented provider default (high for 4.5) | omit `reasoning` (provider default) | omit `reasoning` |
| Low/Medium/High/XHigh | adaptive thinking + `output_config:{effort:"<exact>"}` | `enabled` + exact legacy `budget_tokens` tier | `reasoning:{effort:"<exact>"}` iff in the exact set, else reject | **reject** (no documented effort) | **reject** |
| XHigh (xAI) | as above | as above | multi-agent only; rejected on 4.5 | reject | reject |
| Max / Ultra | **reject** (Codex-only extended modes) | **reject** | **reject** | **reject** | **reject** |

Additional rules:

- Fixed-budget Anthropic models never receive named effort values
  (`output_config` is only ever emitted on adaptive-effort models).
- Legacy numeric budgets (`/thinking 8192`, config `thinking_budget`) remain
  exact for Anthropic fixed-budget request construction.
- Numeric `/thinking <N>` is validated at mutation time like named levels:
  the budget's derived level (`from_legacy_budget`, never Max/Ultra) runs
  through the same exact-model validator before mutating/persisting. Rejected
  budgets leave state unchanged — never silently downgraded (e.g.
  `/thinking 8192` is rejected on `xai-auth/grok-4.3`, maps to High and is
  accepted on `xai-auth/grok-4.5`; exact budgets are preserved on Anthropic).
- Unknown model / provider metadata fails closed for extended and
  provider-specific modes (Codex Max/Ultra on unknown Codex ids included).
- Provider-qualified identity (`anthropic/<id>`, `xai-auth/<id>`,
  `openai-codex/<id>`) is preserved end-to-end; validation keys on the
  qualified id, never on substrings of a model name.
- Off/named rejection for xAI happens at mutation time (command/settings) and
  again at the request boundary — before any broker credential access.
- Anthropic wire-shape selection (adaptive vs enabled+budget) intentionally
  stays on the existing source-controlled static function so request bytes stay
  deterministic; catalog data drives options/validation, not the byte layout.
  Bodies for unchanged behavior remain byte-identical (golden gate).

## Commands

- `/thinking <off|adaptive|low|medium|high|xhigh|max|ultra|N>` — validated via
  `Runtime::set_reasoning_level_checked`; on `Err` nothing is mutated or
  persisted.
- Settings modal “Thinking” cycler — options derived per active model from the
  capability cache (live catalog) then exact static descriptors, as the Codex
  slice does; rejection leaves runtime state and persisted config unchanged.
- Config/session/headless/server/`--continue` reuse existing persistence
  (`thinking_level` string + legacy numeric budgets); no format changes.

Verification commands:

```bash
CARGO_BUILD_JOBS=8 cargo test -p synaps-core
CARGO_BUILD_JOBS=8 cargo test -p synaps-engine
CARGO_BUILD_JOBS=8 cargo test -p synaps-tui
CARGO_BUILD_JOBS=8 cargo check -p synaps
CARGO_BUILD_JOBS=8 cargo check --release -p synaps-engine
CARGO_BUILD_JOBS=8 cargo check --release -p synaps-tui
git diff --check
```

## Project structure

```
crates/agent-engine/src/runtime/openai/catalog/
  anthropic.rs        # live parse: thinking.supported=false → None; cache populate;
                      # anthropic_static_capability (exact ids)
  xai.rs              # XaiReasoningCapability per exact id; xai_static_capability
  validation.rs       # NEW: shared mutation-time validator + default level +
                      # dynamic thinking options per provider-qualified id
  codex.rs            # validate_codex_level: unreachable! removed (fail-closed Err)
crates/agent-engine/src/runtime/openai/stream.rs
                      # build_xai_body: pure Responses body builder w/ pre-network
                      # validation; call_xai_responses_stream_inner threads ReasoningLevel
crates/agent-engine/src/runtime/request.rs
                      # RequestBody: effort derived from named ReasoningLevel
crates/agent-engine/src/runtime/mod.rs
                      # set_reasoning_level_checked → shared validator;
                      # set_model applies provider default level when not explicit
crates/agent-tui/src/tui/settings/mod.rs
                      # thinking_options_for_model delegates to shared engine fn
docs/anthropic-xai-reasoning-modes-{spec,plan}.md
```

## Style example

```rust
/// Exact-id capability lookup — never substring-based.
pub fn xai_static_capability(model_id: &str) -> Option<XaiReasoningCapability> {
    use agent_core::reasoning::ReasoningLevel::*;
    match model_id {
        "grok-4.5" | "grok-4.5-latest" => Some(XaiReasoningCapability::Effort {
            supported: &[Low, Medium, High],
            default_level: Some(High),
            can_disable: false,
        }),
        // ...exact ids only; anything else is None (fail closed).
        _ => None,
    }
}
```

## Testing strategy

Strict red-before-green. New tests:

1. **Catalog unit (engine)** — xAI capability table exactness (4.5 set, no
   xhigh; multi-agent xhigh; 4.3 family none; non-reasoning none); Anthropic
   static descriptors; live Anthropic parse mapping `thinking.supported=false`
   → `None` and populating the capability cache under `anthropic/<id>`.
2. **Shared validator (engine)** — per-provider accept/reject matrix above,
   including: unknown Codex id + Max/Ultra rejected at mutation time; xAI Off
   rejected (not omitted) on 4.5; non-reasoning named rejected; unknown xAI id
   fails closed; state unchanged after `set_reasoning_level_checked` Err.
3. **Pure body builders (engine)** — `build_xai_body` produces exact Responses
   wire (`model`, `input`, `stream`, `tools`, `max_output_tokens`,
   `reasoning:{effort}` presence/absence) and returns `Err` pre-network for
   rejected combinations; Anthropic `RequestBody` boundary tests for
   Off/Adaptive/named on adaptive and fixed-budget models; existing golden
   byte-identity gate stays green (unchanged behavior = unchanged bytes).
4. **Headless harness (no live credentials)** — `try_route` on
   `xai-auth/grok-4.5` with Off fails before any credential/network access;
   Responses path (not chat-completions) covered via existing zero-network
   harness patterns; `Runtime::new_headless` model-switch default-level flows.
5. **TUI (synaps-tui)** — dynamic options for `anthropic/<id>` and
   `xai-auth/<id>`; settings dispatch rejection leaves state/config unchanged.

## Boundaries and exclusions

- No changes to auth flows, broker allowlists, or credential storage; the xAI
  route keeps its brokered/OAuth path and Responses wire.
- No live-network tests; catalog parsing is fixture-driven.
- Anthropic adaptive-vs-budget wire-shape selection function is not migrated to
  catalog data in this slice (byte-identity preservation); documented gap.
- OpenRouter/Groq/NVIDIA/Copilot/Gemini reasoning behavior is out of scope.
- `cargo fmt --all` is forbidden; rustfmt only changed files.

## Security

- Fail-closed validation happens before any broker credential fetch or network
  I/O on both Codex and xAI paths.
- Broker boundary unchanged: engine never reads refresh tokens; xAI requests
  cross `proxy_stream` only after the body is fully validated.
- No secret material in fixtures, tests, or error messages.

## Success criteria

1. All matrix cells above enforced by tests (RED evidence recorded, then GREEN).
2. Golden Anthropic body gate byte-identical for unchanged scenarios.
3. `cargo test -p synaps-core/-engine/-tui`, `cargo check -p synaps`, release
   checks for engine + TUI all pass; `git diff --check` clean.
4. No model-name substring inference introduced; provider-qualified identity
   preserved through config, session, status, and wire.
5. Rejected mutations provably leave runtime state and persisted config
   unchanged.
