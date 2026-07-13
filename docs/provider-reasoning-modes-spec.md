# Provider- and model-specific reasoning modes

## Assumptions

1. The immediate authoritative target is OpenAI Codex over ChatGPT OAuth because its brokered live catalog publishes exact per-model `supported_reasoning_levels` and `default_reasoning_level` values.
2. `xhigh`, `max`, and `ultra` are distinct named modes. They must survive config, runtime, session persistence, status display, and wire serialization without aliases or numeric-bucket collapse.
3. Unsupported modes fail clearly; Synaps never silently downgrades them.
4. Providers without authoritative exact-model metadata retain their current behavior in this slice. We will not infer capabilities from model-name substrings.
5. Legacy numeric budgets remain supported for Anthropic fixed-budget models and old config/session data.

## Objective

Add a typed named reasoning level alongside the existing numeric legacy budget, then use exact Codex catalog metadata to expose, validate, persist, and send provider/model-specific modes.

Current Codex OAuth capability evidence:

| Model | Supported named modes |
|---|---|
| `openai-codex/gpt-5.6-sol` | low, medium, high, xhigh, max, ultra |
| `openai-codex/gpt-5.6-terra` | low, medium, high, xhigh, max, ultra |
| `openai-codex/gpt-5.6-luna` | low, medium, high, xhigh, max |
| `openai-codex/gpt-5.5` | low, medium, high, xhigh |
| `openai-codex/gpt-5.4` | low, medium, high, xhigh |
| `openai-codex/gpt-5.4-mini` | low, medium, high, xhigh |
| `openai-codex/gpt-5.3-codex-spark` | low, medium, high, xhigh |

## Domain contract

Introduce a closed `ReasoningLevel` enum:

```rust
pub enum ReasoningLevel {
    Off,
    Adaptive,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Ultra,
}
```

The enum owns parsing and exact serialization. `max` must never alias to `xhigh`; `ultra` must never map through a numeric budget bucket. Runtime state stores the named level separately from the legacy token budget. Provider request builders receive the named level explicitly.

A model capability contains an ordered exact set of named levels plus an optional default. Capability validation uses provider-qualified identity and catalog metadata. Missing metadata does not authorize `max` or `ultra`.

## User behavior

- `/thinking max` and `/thinking ultra` preserve those exact names.
- On a Codex model that supports the selected mode, the session/config/status and request body use the exact mode.
- On a Codex model that does not support it, the command/settings operation returns a clear error and leaves runtime state unchanged.
- The settings UI shows only the active model's supported Codex modes. For providers without exact metadata, it keeps the conservative existing choices and does not advertise `max`/`ultra`.
- Selecting another model re-evaluates available settings; an already persisted unsupported level is rejected at the request boundary rather than silently changed.

## Compatibility

- Existing config values `adaptive`, `low`, `medium`, `high`, `xhigh`, and numeric budgets continue to load.
- Existing session `thinking_level` strings continue to deserialize.
- Numeric custom budgets remain available to legacy Anthropic request construction.
- No auth or broker boundary changes; catalog fetching remains brokered and pinned.

## Testing strategy

Strict red-before-green unit and boundary tests:

1. Enum parsing/serialization and legacy budget compatibility.
2. Codex catalog parsing of exact supported/default levels.
3. Exact capability validation for sol/terra/luna/5.5 and unsupported combinations.
4. `/thinking` preservation and fail-closed runtime mutation.
5. Config/session round trips for `max` and `ultra`.
6. Codex request-body tests proving exact `reasoning.effort` and rejection before network for unsupported modes.
7. TUI tests proving dynamic options and unchanged state after rejected selection.
8. Existing Anthropic golden-body tests remain byte-identical.

## Commands

```bash
CARGO_BUILD_JOBS=8 cargo test -p synaps-core
CARGO_BUILD_JOBS=8 cargo test -p synaps-engine
CARGO_BUILD_JOBS=8 cargo test -p synaps-tui
CARGO_BUILD_JOBS=8 cargo check --release -p synaps-engine
CARGO_BUILD_JOBS=8 cargo check --release -p synaps-tui
git diff --check
```

Format changed Rust files only with targeted `rustfmt`; never `cargo fmt --all`.

## Boundaries

### Always

- Preserve provider-qualified identities.
- Parse external catalog strings into typed levels at the boundary.
- Reject unsupported exact provider/model/mode combinations without network access.
- Keep secrets broker-owned.

### Deferred

- xAI, OpenRouter, Groq, Anthropic, Gemini, Vertex, Azure, Bedrock, and Copilot `max`/`ultra` enablement until each path has authoritative exact-model capability metadata and wire tests.
- Numeric-budget removal or session-schema migration.

### Never

- Infer support from model-name substrings.
- Treat generic OpenAI API model capabilities as ChatGPT OAuth capabilities.
- Silently clamp or downgrade a requested level.
- Invent unsupported wire values.
