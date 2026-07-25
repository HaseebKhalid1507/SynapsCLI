# AGENTS.md — SynapsCLI Developer & Agent Guide

This is the onboarding doc for any agent (Cursor, Aider, or SynapsCLI itself) touching this codebase. Read this first. If you only read one file, read this one.

SynapsCLI is a terminal-native AI agent runtime written in Rust. ~210K LOC across the workspace (counts drift — verify with `find src crates -name '*.rs' -not -path '*/target/*' | xargs wc -l`). **Cargo workspace** with three library crates + a root binary crate:

- `synaps-core` (`crates/agent-core/`) — config, session, auth, credential broker, models, memory, orchestration policy, prompt manifests, pricing, protocol primitives
- `synaps-engine` (`crates/agent-engine/`) — runtime, provider catalogs, credential routing, tools, memory tools, reactive subagent lifecycle, MCP, skills, events, extensions, sidecar, watcher
- `synaps-tui` (`crates/agent-tui/`) — the ratatui TUI
- `synaps` (`src/main.rs`) — the binary; CLI dispatch

Each crate has `extern crate self as synaps_cli;` and re-exports its dependencies, so internal `synaps_cli::` paths still resolve for contributors navigating older references. Talks to Anthropic, OpenAI Codex, xAI, GitHub Copilot, Google Gemini, Azure OpenAI, Amazon Bedrock, Google Vertex AI, and Kimi natively, plus any OpenAI-compatible provider (Groq, Cerebras, NVIDIA, OpenRouter, local Ollama, etc.) via the built-in provider engine. Streams SSE, dispatches tools, renders a TUI.

---

## Build & Test

```bash
cargo build --release                    # full release build (lto, single codegen unit, strip)
cargo build                              # dev build — faster compile, slower runtime
cargo test --workspace                   # all crates + integration tests
cargo test -p synaps-engine              # single crate
cargo test -p synaps-engine -- --test-threads=1  # required for PTY tests (TTY fd contention)
cargo test --test extensions_e2e         # end-to-end with real extension process
cargo clippy --all-targets -- -D warnings  # linting (CI gate)
```

**Minimum Rust:** 1.80 (edition 2021).
**Config path:** `~/.synaps-cli/config` (plain `key = value`, see `crates/agent-core/src/core/config.rs`).
**Binary:** `target/release/synaps` — single binary, dispatched via subcommand.

- `synaps` (no args) — interactive TUI (the main product)
- `synaps --continue [NAME_OR_ID]` — resume last session, or resolve a chain bookmark / session alias / partial session ID via `resolve_session()` (chain name → session name → partial ID)
- `synaps --no-extensions` — disable the extension system (skips plugin hook registration)
- `synaps chat` — fully-featured headless mode (MCP, extensions, skills, sessions, compaction, event bus)
- `synaps agent` — headless worker managed by the watcher
- `synaps watcher` — supervisor daemon
- `synaps login` — OAuth flow
- `synaps server` — WebSocket API server
- `synaps send` — push events into a running session
- `synaps status` — check account usage
- `synaps auth-broker` — credential broker daemon for multi-machine shared auth

**Test quirks:**
- PTY tests (`crates/agent-engine/src/tools/shell/pty.rs`, `crates/agent-engine/src/tools/shell/session.rs`) have historically been flaky under parallel execution due to TTY fd contention. Recent runs pass in parallel consistently — but if you see `shell::` failures, retry with `--test-threads=1` before suspecting your change.
- `cargo test --workspace` runs everything including integration tests in `tests/` (e.g. `extensions_e2e`, the `extension_provider_*` suite). "Lib green" can still mean broken — always run the full suite.
- Tests use `tempfile` crate. No fixtures checked in.

---

## Project Structure

```
src/
└── main.rs               — CLI entry point, subcommand dispatch

crates/agent-core/src/    (crate: synaps-core)
├── lib.rs                — crate root; re-exports config, models, session, auth, etc.
├── core/                 — shared primitives
│   ├── config.rs         — SynapsConfig, load/write, profile resolution
│   ├── models.rs         — KNOWN_MODELS, thinking_level_for_budget, context_window_for_model
│   ├── session.rs        — on-disk session persistence (JSONL), `/saveas` naming, `resolve_session()` (chain → name → partial ID)
│   ├── chain.rs          — named chain bookmarks (`~/.synaps-cli/chains/<name>.json`), auto-advance on `/compact`
│   ├── auth/             — typed credential broker: OAuth PKCE, cloud identity (AWS/Azure/GCP), static keys, token storage (fs4-locked, mode 600)
│   ├── protocol.rs       — WebSocket wire format (server/client)
│   ├── error.rs          — SynapsError type
│   ├── logging.rs        — tracing subscriber setup
│   └── watcher_types.rs  — shared types for watcher IPC
├── memory/               — session memory store (mod.rs, store.rs)
├── orchestration.rs      — worker authorization policy, completion gates, model/role/scope validation
├── prompt/               — YAML prompt manifests, module composition, path confinement
└── pricing.rs            — unified pricing table (all providers + models)

crates/agent-engine/src/  (crate: synaps-engine)
├── lib.rs                — crate root; re-exports Runtime, ToolRegistry, etc.
├── cmd/                  — subcommand handlers (chat, server, agent, login, watcher, send, status, rpc)
├── engine/               — shared headless engine used by chat and agent
│   ├── setup.rs          — boot sequence (config, runtime, extensions, session)
│   ├── commands.rs       — slash-command handling for headless mode
│   ├── stream.rs         — StreamEvent consumer for headless rendering
│   └── session.rs        — ConversationState (messages, history)
├── runtime/              — THE BRAIN
│   ├── mod.rs            — Runtime struct, orchestration loop
│   ├── api.rs            — Anthropic API body construction + SSE parsing
│   ├── api_sync.rs       — non-streaming API path (used by /compact)
│   ├── request.rs        — request body assembly helpers
│   ├── stream.rs         — tool dispatch from streamed tool_use events
│   ├── helpers.rs        — annotate_cache_breakpoint, drain_steering, etc.
│   ├── telemetry.rs      — opt-in per-request API records (TelemetryLevel, JSONL log at ~/.cache/synaps/api-log.jsonl)
│   ├── subagent.rs       — blocking `subagent` tool (spawns a child Runtime)
│   ├── types.rs          — StreamEvent enum (the wire between runtime and UIs)
│   ├── auth.rs           — auth token refresh before request
│   └── openai/           — OpenAI-compatible provider engine
│       ├── mod.rs        — Provider enum, resolve_route(), try_route()
│       ├── registry.rs   — 18 providers (17 specs + local), 50+ models, env+config key resolution
│       ├── types.rs      — ChatMessage, ToolCall, ChatRequest, OaiEvent, ProviderConfig
│       ├── wire.rs       — SSE parser + StreamDecoder (HashMap-based tool call accumulation)
│       ├── translate.rs  — Anthropic↔OpenAI message/tool/event translation
│       ├── stream.rs     — call_oai_stream_inner (streaming path)
│       ├── ping.rs       — /ping health check (parallel, non-blocking)
│       └── catalog/      — per-provider model catalogs (anthropic, codex, xai, copilot, gemini, groq, kimi, nvidia, openrouter)
├── events/               — event bus (per-session unix socket, inbox watcher, queue, registry)
├── tools/                — built-in tools, each impls the Tool trait
│   ├── mod.rs            — Tool trait, ToolContext
│   ├── registry.rs       — ToolRegistry::new() registers all built-ins
│   ├── {bash,read,write,edit,grep,find,ls}.rs  — core filesystem/shell tools
│   ├── subagent/         — reactive lifecycle: start, status, steer, collect, resume, authorize_model, finalize + oneshot
│   ├── memory/           — memory_store, memory_search, memory_fetch, memory_forget, memory_context tools
│   ├── agent.rs          — (legacy — prefer the subagent tools)
│   ├── extension.rs      — extension-provided tool wrapper
│   ├── respond.rs        — event-bus response tool
│   ├── send_channel.rs   — channel send tool
│   ├── watcher_exit.rs   — graceful-exit tool (watcher agents only)
│   ├── secret_prompt.rs  — secure sudo password prompt handling
│   ├── shell/            — stateful PTY shell (start/send/end) — session manager
│   └── util.rs           — strip_ansi, expand_path, NEXT_SUBAGENT_ID
├── mcp/                  — Model Context Protocol client
│   ├── connection.rs     — JSON-RPC over stdio to MCP servers
│   ├── lazy.rs           — lazy server spawn (don't pay until connect_mcp_server called)
│   └── tool.rs           — MCP tools wrapped as Tool impls
├── extensions/           — Extension system (hooks, permissions, JSON-RPC runtime)
│   ├── mod.rs            — crate-level re-exports
│   ├── hooks/mod.rs      — HookBus dispatcher
│   ├── hooks/events.rs   — HookKind, HookEvent, HookResult types
│   ├── permissions.rs    — Permission flags and PermissionSet
│   ├── manifest.rs       — ExtensionManifest from plugin.json
│   ├── manager.rs        — ExtensionManager lifecycle
│   └── runtime/process.rs — JSON-RPC over stdio ProcessExtension
├── sidecar/              — sidecar process support
├── skills/               — skill discovery + command registry
│   ├── loader.rs         — walks .synaps-cli/{plugins,skills} roots
│   ├── manifest.rs       — plugin.json / marketplace.json parsers
│   ├── registry.rs       — CommandRegistry: built-ins + skill names → tab-complete
│   ├── marketplace.rs    — plugin install from marketplace
│   └── tool.rs           — load_skill Tool impl
└── watcher/              — supervisor daemon
    ├── mod.rs            — subsystem entry (invoked by `synaps watcher`)
    ├── supervisor.rs     — per-agent lifecycle, limits, retries
    ├── ipc.rs            — Unix socket protocol (deploy, status, stop)
    └── display.rs        — `watcher status` renderer

crates/agent-tui/src/     (crate: synaps-tui)
└── tui/                  — the TUI (module, entered via default `synaps` subcommand)
    ├── mod.rs            — event loop + apply_setting()
    ├── app.rs            — App state, record_cost(), line cache
    ├── input.rs          — key handling, process_submit()
    ├── draw.rs           — render dispatch
    ├── render.rs         — message rendering
    ├── markdown.rs       — markdown → styled lines
    ├── highlight.rs      — syntect-backed syntax highlighting
    ├── stream_handler.rs — StreamEvent → UI mutation
    ├── commands.rs       — slash-command dispatch (STREAMING_COMMANDS, handle_command)
    ├── theme/            — 17 built-in palettes + user TOML loader
    ├── settings/         — /settings modal (schema, input, draw)
    ├── plugins/          — /plugins modal
    └── gamba.rs          — easter egg. Don't touch.

bench/                    — cache strategy benchmark suite (Python; see bench/README.md)
tests/                    — integration tests (extensions_e2e, extension_provider_*, etc.)
```

The tree above is curated, not exhaustive — `extensions/`, `skills/`, and `tui/` have grown more files than listed. When in doubt, `ls` the directory.

---

## The Request Lifecycle

This is the single most important flow to understand.

1. **User input** → `crates/agent-tui/src/tui/input.rs::process_submit()` builds a user message, pushes it into `App.messages`, kicks off a stream.
2. **Stream kickoff** → `Runtime::run_stream_with_messages()` in `runtime/mod.rs`.
3. **Auth refresh** → `runtime/auth.rs` refreshes the OAuth token if near expiry, before the request is built.
4. **API body build** → `runtime/api.rs::call_api_stream()`. Steps:
   - **Provider routing** → `openai::try_route(model, ...)` checks if the model has a provider prefix (e.g. `groq/llama-3.3-70b`). If yes, routes through `openai/stream.rs` instead of Anthropic. If no, falls through to Anthropic.
   - Clone messages, strip UI-only fields.
   - `HelperMethods::annotate_cache_breakpoint(&mut cleaned_messages)` — see caching section below.
   - Look up thinking config based on model: adaptive (`{type: "adaptive"}` + `output_config.effort`) for Opus 4.7+ / Sonnet 4.7+ / 5.x / Fable 5, else legacy (`{type: "enabled", budget_tokens: N}`). Gated by `model_supports_adaptive_thinking()` in `crates/agent-core/src/core/models.rs`.
   - Serialize tool schemas (`ToolRegistry::schemas_json()`).
   - POST to `https://api.anthropic.com/v1/messages` with `stream: true`.
   - **Retries** — transient errors (429/5xx/529) retry with backoff up to `api_retries` (default 3).
5. **SSE parse** → line-by-line in `api.rs`. Before the body is consumed, response headers (request-id, rate-limits) are captured for telemetry when `telemetry != off`. Emits `StreamEvent`s — a two-level enum: `StreamEvent::{Llm, Session, Agent}` wrapping `LlmEvent::{Thinking, Text, ToolUseStart, ToolUseDelta, ToolUse, ToolResult, Usage, ...}` (see `runtime/types.rs` for the full shape).
6. **Tool dispatch** → `runtime/stream.rs` collects `ToolUse` blocks, executes them in parallel via `tokio::spawn`, feeds `tool_result` blocks back into the next turn. Extension hooks (`before_tool_call` / `after_tool_call`) fire around each execution — a slow extension stalls dispatch.
7. **Loop** → steps 4–6 repeat until `stop_reason != "tool_use"` (typically `"end_turn"`).
8. **UI update** → `crates/agent-tui/src/tui/stream_handler.rs` consumes `StreamEvent`s and mutates `App`. Headless mode has its own consumer in `crates/agent-engine/src/engine/stream.rs` — **new event variants must be handled in BOTH** or headless silently drops them.

`StreamEvent` (in `crates/agent-engine/src/runtime/types.rs`) is the wire format between Runtime and any UI. Add new event variants here if you need to surface something new — then update both consumers (`crates/agent-tui/src/tui/stream_handler.rs` AND `crates/agent-engine/src/engine/stream.rs`).

---

## Key Patterns

### Credential Broker & Provider Auth

Typed auth flows live in `crates/agent-core/src/core/auth/`. Three flow families:

- **OAuth PKCE** — Anthropic, OpenAI Codex, xAI, GitHub Copilot, Google Gemini. Browser-based device flow with PKCE challenge, refresh token rotation.
- **Cloud identity** — Azure OpenAI (Azure CLI/managed identity), AWS Bedrock (STS/profile), Google Vertex AI (gcloud ADC). Cloud secrets never leave the broker.
- **Static keys** — Groq, Cerebras, NVIDIA, OpenRouter, Kimi. Env var or config-file discovery, broker-owned.

The broker boundary ensures long-lived tokens (refresh tokens, cloud credentials) are never handed to model runtimes. Short-lived access tokens are minted per-request. `synaps auth-broker` exposes the broker over a socket for multi-machine setups (shared OAuth across a fleet over WireGuard or TLS).

### Memory System

Project-scoped persistent memory in `crates/agent-core/src/memory/`:

- **Store**: JSONL files at `~/.synaps-cli/memory/<project>/`, one record per line.
- **Index**: Lexical substring index for fast search without external dependencies.
- **Sensitivity**: Three classes — `normal` (full access), `sensitive` (redacted in exports/logs), `secret` (never returned via tool calls).
- **Tools**: `memory_store`, `memory_search` (returns descriptors with snippets), `memory_fetch` (full bodies by ID), `memory_forget` (tombstone), `memory_context` (session lifecycle control).

Records have stable IDs, model provenance, tags, timestamps, and optional TTL. The system is designed for agents to accumulate project knowledge across sessions.

### Reactive Subagent Lifecycle

Implementation split across `crates/agent-engine/src/tools/subagent/` (tools) and `crates/agent-engine/src/orchestration.rs` (lifecycle):

- **Fire-and-forget dispatch**: `subagent_start` returns a handle immediately, worker runs in background.
- **Polling + steering**: `subagent_status` (non-blocking poll), `subagent_steer` (inject guidance mid-run).
- **Completion gate**: Only blocks the foreground on `Terminal|Collected` workers (finished but unreconciled). Running workers pass through — this is what enables the reactive pattern where the foreground continues while workers run.
- **Collection + reconciliation**: `subagent_collect` retrieves results; `reconciled=true` attests the foreground has incorporated the result, clearing the gate.
- **Tombstones**: Expired uncollected workers become bounded tombstones that release transport/thread/channel resources while retaining a UTF-8-safe output tail and an idempotent collection path. Reconciled tombstones are pruned on the next reaper pass.

### Adding a New Tool

1. Create `crates/agent-engine/src/tools/my_tool.rs` with a struct implementing the `Tool` trait:
   ```rust
   #[async_trait::async_trait]
   pub trait Tool: Send + Sync {
       fn name(&self) -> &str;
       fn description(&self) -> &str;
       fn parameters(&self) -> serde_json::Value;       // JSON Schema
       async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String>;
   }
   ```
   See `crates/agent-engine/src/tools/mod.rs::Tool`.
2. Re-export in `crates/agent-engine/src/tools/mod.rs` (`pub use my_tool::MyTool;`).
3. Register in `crates/agent-engine/src/tools/registry.rs::ToolRegistry::new()` — add to the `vec![]`.
4. If it streams output, use `ctx.tx_delta` (UnboundedSender<String>) to push deltas.
5. If it's restricted (e.g. watcher-only), gate on `ctx.watcher_exit_path.is_some()` or similar.

The tool's `parameters()` JSON schema is what the model sees. Be precise — bad schemas lead to malformed tool calls.

### Adding a New Setting

The old "5-site sync" pain point was solved by the `define_settings!` macro. Now it's 2–3 sites:

1. **`crates/agent-tui/src/tui/settings/defs.rs`** — add an entry to the `define_settings!` invocation: key, label, category, `EditorKind` (`Cycler(&[...])`, `Text { numeric }`, `ModelPicker`, or `ThemePicker`), help text, and the apply closure (`|runtime, app, value| { ... }`). The macro generates both `ALL_SETTINGS` (schema) and `apply_setting_dispatch()` — they cannot drift.
2. **`crates/agent-core/src/core/config.rs::load_config()`** — add a branch to parse the key from the config file, plus the field on `SynapsConfig` and its default.
3. **`crates/agent-engine/src/skills/mod.rs::BUILTIN_COMMANDS`** — only if it also gets a slash command (e.g. `/foo`); then handle it in `tui/commands.rs::handle_command()` too.

`apply_setting()` itself lives in `crates/agent-tui/src/tui/helpers.rs` and delegates to the generated dispatch — you should rarely need to touch it. Step 2 is the un-tested one: forget the `load_config()` branch and the setting silently won't persist across restarts.

### Adding a New Model

1. `crates/agent-core/src/core/models.rs::KNOWN_MODELS` — add `(id, description)` tuple (Anthropic models only).
2. For OpenAI-compatible provider models: add to the provider's catalog file in `crates/agent-engine/src/runtime/openai/catalog/` (e.g. `groq.rs`, `kimi.rs`). Each provider has its own catalog with model rows specifying ID, label, tier, and capability flags.
3. If it supports adaptive thinking: update `model_supports_adaptive_thinking()`.
4. If it supports 1M context opt-in: update `model_supports_1m()`. If context window differs: `context_window_for_model()`.
5. Pricing: see `crates/agent-core/src/pricing.rs` for the unified pricing table. `record_cost()` in `crates/agent-tui/src/tui/app.rs` uses it. Default falls back to Sonnet pricing.
6. There are existing tests in `core/models.rs` — extend them.

Worked example: `claude-fable-5` (commits e824fdc, 497c1ef, 59c6be3) touched all of: KNOWN_MODELS, `model_supports_adaptive_thinking()`, `model_supports_1m()`, and pricing ($10/$50 per MTok).

### Adding a New Provider

All OpenAI-compatible providers live in `crates/agent-engine/src/runtime/openai/registry.rs`.

1. Add a `ProviderSpec` entry to the `providers()` function:
   ```rust
   ProviderSpec {
       key: "myprovider",                              // used in provider/model shorthand
       name: "My Provider",                            // display name in settings
       base_url: "https://api.myprovider.com/v1",     // OpenAI-compat chat/completions
       env_vars: &["MYPROVIDER_API_KEY"],              // env var fallback(s)
       default_model: "some-model-id",                 // used by resolve_provider()
       models: &[
           ("model-id", "Display Name", "S"),          // (api_id, label, tier)
       ],
   }
   ```
2. That's it for most providers. The router, settings UI, model picker, and `/ping` all pick it up automatically.

**Special cases:**
- If the provider rejects `stream_options`: add a URL check in `stream.rs` (see Google gate).
- If auth isn't `Bearer`: needs a new code path in `stream.rs` (currently only Bearer supported).
- For local providers: use `local` key with dynamic URL from `provider.local.url` config.

**Provider key resolution order:** `provider.<key>` in config → env var → absent.

**The translation layer** (`translate.rs`) handles Anthropic↔OpenAI format differences:
- `tools_to_oai()` — converts `input_schema` → `parameters`
- `messages_to_oai()` — flattens content blocks, maps tool_result/tool_use
- `oai_event_to_llm()` — maps OaiEvent → StreamEvent (provider-agnostic)
- `tool_calls_to_content_blocks()` — converts back to Anthropic shape for the agent loop

The agent loop (`runtime/stream.rs`) is **provider-blind** — both paths return identical Anthropic-shaped `Value`s.

### Adding a New Theme

1. Add a `Theme::my_theme()` method in `crates/agent-tui/src/tui/theme/palettes/` (one file per palette) returning a populated `Theme` struct (all ~30 color fields).
2. Register in `crates/agent-tui/src/tui/theme/mod.rs::Theme::builtin()` — add a `match` arm.
3. Add the theme name to the list returned by `crates/agent-tui/src/tui/settings/mod.rs::theme_options()`.
4. Test via `/settings → Appearance → Theme` or config `theme = my-theme`. Requires TUI restart to apply.

### Adding a New Slash Command

1. Add name to `BUILTIN_COMMANDS` (`crates/agent-engine/src/skills/mod.rs`) — this feeds tab-complete via `CommandRegistry::all_commands()`.
2. If it should work during streaming, add to `STREAMING_COMMANDS` (`crates/agent-tui/src/tui/commands.rs`).
3. Add a match arm in `handle_command()` (tui/commands.rs).
4. If it needs async work or opens a modal, extend `CommandAction` enum and handle in `tui/mod.rs` event loop.

### Plugin Agent Resolution

Agents from installed plugins can be dispatched via `plugin:agent` namespaced syntax:

```
subagent(agent: "dev-tools:sage", task: "...")
```

Resolution order in `resolve_agent_prompt()` (`crates/agent-engine/src/tools/agent.rs`):
1. Name contains `/` → file path (read directly)
2. Name contains `:` → `plugin:agent` namespaced lookup → searches `~/.synaps-cli/plugins/<plugin>/skills/*/agents/<agent>.md`
3. Bare name → `~/.synaps-cli/agents/<name>.md`

Safety: both sides of `:` validated as identifiers (no path traversal). Ambiguous matches (agent exists in multiple skills) return an error. I/O errors propagated, not swallowed.

### Adding a Plugin Keybind

Plugins declare keybinds in `plugin.json`:

```json
{
  "keybinds": [
    {
      "key": "F5",
      "action": "slash_command",
      "command": "compact",
      "description": "Quick compact"
    }
  ]
}
```

**Key notation:** `C` = Ctrl, `A` = Alt, `S` = Shift. Combine with `-`: `C-S-s` = Ctrl+Shift+S. Special keys: `F1`–`F12`, `Space`, `Tab`, `Enter`, `Esc`.

**Action types:**
- `slash_command` — runs a `/command` (field: `command`)
- `load_skill` — loads a skill (field: `skill`)
- `inject_prompt` — submits text as user message (field: `prompt`). Note: this lets a plugin put words in the user's mouth — treat third-party plugins declaring `inject_prompt` binds with suspicion.
- `run_script` — **NOT IMPLEMENTED.** The match arm in `skills/keybinds.rs` is a no-op TODO. If you implement it: validate the script path stays inside the plugin dir and never pass manifest strings to a shell unescaped.

**Implementation path:**
1. `ManifestKeybind` (`crates/agent-engine/src/skills/keybinds.rs`) — serde struct for plugin.json parsing
2. `KeybindRegistry` (`crates/agent-engine/src/skills/keybinds.rs`) — collects + resolves conflicts
3. Built during `skills::register()` (`crates/agent-engine/src/skills/mod.rs`) alongside command registry
4. Checked in `handle_key()` (`crates/agent-tui/src/tui/input.rs`) before the core match block
5. `parse_key()` / `format_key()` for notation ↔ KeyCombo conversion

**Priority:** Core (Ctrl+C, Esc, etc.) > user config (`keybind.*`) > plugin. Core binds are never overridable. User config always wins over plugins.

**User overrides** in `~/.synaps-cli/config`:
```
keybind.F5 = /compact        # override or add
keybind.F6 = disabled        # block a plugin bind
```

---

## Prompt Caching Strategy

This is non-obvious and cost-critical. See `crates/agent-engine/src/runtime/helpers.rs::annotate_cache_breakpoint`.

- **Manual breakpoint placement.** We don't rely solely on Anthropic's auto-cache.
- Anthropic allows up to 4 cache markers per request. We reserve 2 for tools + system prompt (placed elsewhere in `api.rs`), leaving **2 for conversational markers**.
- Breakpoints advance every **4 user messages**. The latest eligible user message gets a `cache_control: {type: "ephemeral"}` on its last content block.
- **Prefix stability = cache stability.** Content of historical messages must never change — adding, removing, or altering a visible field in an old message invalidates all downstream cache hits. The one sanctioned exception: `annotate_cache_breakpoint` itself prunes stale `cache_control` markers from older messages when more than 2 exist (this is load-bearing — don't "fix" it), and `sanitize_thinking_blocks` normalizes history before send. Beyond those, do not retroactively mutate messages.
- **Measured** (bench/ suite, 21-turn tool-heavy sessions, 2026-06-11):
  - `single-last` (1 marker on latest message): **96–97%** hit rate, lowest cost
  - `sliding-4` (current strategy): **96.7%**
  - `none` (no conversational markers — tools+system still cached): **91.3%**
  - Conclusion: conversational markers buy ~5 points over none. `single-last` matches `sliding-4` with ~10% of the code — a simplification candidate.
- Reproduce: `python3 bench/run.py --strategy <name>` then `python3 bench/compare.py bench/results/run-*.jsonl`. See `bench/README.md`. Needs `ANTHROPIC_API_KEY` (OAuth rate-limits under heavy benching).

If you touch `annotate_cache_breakpoint`, re-verify hit rates with the bench suite or `telemetry = full` logs (`~/.cache/synaps/api-log.jsonl`).

---

## Thinking Config by Model

Two code paths, gated by `model_supports_adaptive_thinking()`:

**Adaptive (Opus 4.7+, Sonnet 4.7+, 5.x, Fable 5):**
```json
"thinking": {"type": "adaptive", "display": "summarized"}
"output_config": {"effort": "low" | "medium" | "high" | "xhigh"}  // omitted if "adaptive"
```
No `budget_tokens` field — the API rejects it silently on these models (returns no thinking content, error S172).

**Legacy (Opus 4.6, Sonnet 4.6, Haiku, Opus 3.x):**
```json
"thinking": {"type": "enabled", "budget_tokens": N, "display": "summarized"}
```

**The "0 is adaptive" sentinel:** `Runtime::thinking_budget: u32` uses `0` to mean "adaptive (model decides)". Any consumer must handle this. If a user sets `thinking = adaptive` but the model is legacy, `thinking_level_for_budget(0)` returns `"adaptive"` but the legacy path clamps it to `DEFAULT_LEGACY_ADAPTIVE_FALLBACK = 16384` (matches "high"). See `crates/agent-core/src/core/models.rs:80` and `crates/agent-engine/src/runtime/api.rs` (the clamp site — commit 5edcb86).

Mapping (`crates/agent-core/src/core/models.rs::thinking_level_for_budget`):
- `0` → `"adaptive"`
- `1..=2048` → `"low"`
- `2049..=4096` → `"medium"`
- `4097..=16384` → `"high"`
- `16385..` → `"xhigh"`

---

## Configuration Flow

```
~/.synaps-cli/config (or ~/.synaps-cli/{profile}/config)
  → crates/agent-core/src/core/config.rs::load_config()  — parses key = value, env var overrides
  → Runtime::apply_config()         — sets fields on Runtime
  → runtime/api.rs reads from Runtime at request time
  → crates/agent-tui/src/tui/helpers.rs::apply_setting() — runtime mutation (via generated apply_setting_dispatch) + write_config_value() for live /settings changes
```

`SYNAPS_PROFILE` env var selects a sub-directory under `~/.synaps-cli/` (e.g. `~/.synaps-cli/work/config`). Profile-specific files override root files. See `crates/agent-core/src/core/config.rs::resolve_read_path()`.

### Config Keys

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `model` | string | — | Default model (e.g. `claude-sonnet-4-6`) |
| `thinking` | string/int | — | Thinking budget: `low`, `medium`, `high`, `xhigh`, `adaptive`, or token count |
| `context_window` | string/int | auto | `200k`, `1m`, or token count |
| `identity` | string | SynapsCLI default | System prompt preamble injected as the first message in the system block. Defines who the agent is. |
| `agent_name` | string | — | Display name for the agent in the TUI |
| `theme` | string | — | TUI color theme (17 built-in + user TOML) |
| `compaction_model` | string | claude-sonnet-4-6 | Model used for `/compact` |
| `max_tool_output` | int | 30000 | Max bytes per tool output |
| `bash_timeout` | int | 30 | Default bash timeout (seconds) |
| `bash_max_timeout` | int | 300 | Max allowed bash timeout |
| `subagent_timeout` | int | 300 | Default subagent timeout (seconds) |
| `api_retries` | int | 3 | Max API retries on transient errors |
| `favorite_models` | comma list | — | Pinned models in model picker |
| `disabled_plugins` | comma list | — | Plugins to skip on boot |
| `disabled_skills` | comma list | — | Skills to skip on boot |
| `disabled_tools` | comma list | — | Built-in tools to remove from the registry at boot (by runtime name, e.g. `bash, ls`) |
| `provider.<name>` | string | — | API key for provider (e.g. `provider.groq = gsk_...`) |
| `provider.<name>.url` | string | — | Custom base URL (e.g. `provider.local.url = http://...`) |
| `keybind.<key>` | string | — | Custom keybind (e.g. `keybind.F5 = /compact`) |
| `telemetry` | string | off | Per-request API telemetry: `off`, `basic` (timing+usage+cost), `full` (adds rate-limit headers + cache diagnostics). JSONL at `~/.cache/synaps/api-log.jsonl` (0600, O_NOFOLLOW) |
| `cache_diagnostics` | bool | false | Opt into the `cache-diagnosis` beta — server-side cache miss reasons (recorded when telemetry is on) |
| `shell.*` / `server.*` / `bridge.*` | — | — | Namespaced config families parsed by `parse_shell_config_key` / `parse_server_config_key` / `parse_bridge_config_key` in `crates/agent-core/src/core/config.rs` — see those functions for the full key list |

---

## Common Pitfalls

1. **Settings need `defs.rs` + `load_config()` + (optionally) `BUILTIN_COMMANDS`** (see recipe above). The `load_config()` branch is untested — forget it and the setting won't persist.
2. **`thinking_budget: 0` sentinel.** Always handle the "adaptive" case. Legacy paths must clamp.
3. **Cache breakpoints are prefix-sensitive.** Any mutation to historical messages breaks the cache for all subsequent turns. Don't "fix up" old messages retroactively (the marker-pruning inside `annotate_cache_breakpoint` is the sanctioned exception).
4. **`cargo test -p <crate>` skips integration tests.** The `tests/` suite (extensions e2e, provider tests) only runs with `cargo test --workspace`. "Single-crate green" can still mean broken.
5. **Binary swap requires process restart.** `cargo build` replaces `target/release/synaps` on disk but the running process keeps the old binary mmap'd. Must exit + relaunch to pick up changes.
6. **New StreamEvents need TWO consumers.** `crates/agent-tui/src/tui/stream_handler.rs` AND `crates/agent-engine/src/engine/stream.rs` — update only the TUI one and headless mode silently drops the event.
7. **Subagent has NO subagent.** No recursion. Subagents also lack `connect_mcp_server`, `load_skill`, `watcher_exit`. Enforced via the restricted registry in `crates/agent-engine/src/tools/registry.rs` (see the second tool list).
8. **Theme change requires restart.** The `apply_setting` path flags this with `"saved — restart to apply"`. Not a bug — `Theme` is captured by long-lived render state.
9. **MCP servers are lazy-spawned.** First `connect_mcp_server` pays the spawn cost. Tools are registered dynamically via `ToolContext::tool_register_tx` — this channel breaks the `Arc<ToolRegistry>` circularity.
10. **OAuth tokens are file-locked** via `fs4`. Concurrent TUI + watcher instances are safe, but a crashed process holding the lock will block others until its file is cleaned up.
11. **Provider model IDs contain slashes.** `nvidia/meta/llama-3.3-70b-instruct` — the first slash separates provider from model. `resolve_shorthand` uses `split_once('/')`. Nested slashes in model IDs (NVIDIA, DeepInfra) are preserved correctly.
12. **Anthropic auth is optional.** `get_auth_token()` returns `auth_type: "none"` if no credentials found. The app boots fine. Anthropic API calls fail lazily with a clear message pointing to `synaps login` or `/model groq/...`.
13. **`/compact` doesn't route through providers** — uses the Anthropic-only sync path (`runtime/api_sync.rs`). Known issue (see `docs/open-provider-issues.md`).
14. **Cost display uses `pricing.rs`.** Unified pricing table in `src/pricing.rs` covers all providers. Still Claude-centric for providers with unknown pricing — those fall back to Sonnet rates.
15. **Config file contains API keys.** `write_config_value` sets `0600`. `ProviderConfig`'s `Debug` impl redacts `api_key` — but **`Serialize` does not**: any JSON dump of provider config leaks the key. Don't log raw config values or serialize provider structs.
16. **Extension hooks run on the tool hot path.** `before_tool_call`/`after_tool_call` fire around every tool execution — a slow or hung extension stalls all tool dispatch.
17. **`panic = "abort"` in release.** No unwinding — `catch_unwind` tricks don't work in release builds.

---

## Dependencies (key ones)

- **`tokio` 1.x** — async runtime. `features = ["full"]`. Everything is async.
- **`reqwest` 0.11** — HTTP client for Anthropic + OpenAI-compatible APIs.
- **`bytes` 1 + `memchr` 2** — zero-copy SSE line parsing (BytesMut::split_to + SIMD newline search).
- **`ratatui` 0.29 + `crossterm` 0.28** — TUI framework.
- **`tachyonfx` 0.9** — TUI visual effects (the gamba easter egg).
- **`serde_json`** — everything JSON (messages, tool schemas, API bodies).
- **`syntect` 5** — syntax highlighting. `default-themes + default-syntaxes + regex-onig`.
- **`portable-pty` 0.9** — PTY for stateful shell tool.
- **`notify` 6.1 + `globset` 0.4** — file-watching for watcher mode.
- **`axum` 0.7 + `tokio-tungstenite`** — WS server/client (auxiliary).
- **`fs4` 0.13** — advisory file locks for auth.json.
- **`toml` 0.8** — watcher per-agent config (note: global config uses plain `key = value`, NOT TOML).

Release profile: `lto = true, codegen-units = 1, strip = true, panic = "abort"`. Slow compile, small binary.

---

## File Layout Conventions

- **One file per tool** in `crates/agent-engine/src/tools/*.rs`. Complex tools get a sub-directory (e.g. `crates/agent-engine/src/tools/shell/`).
- **TUI separation of concerns:**
  - `crates/agent-tui/src/tui/input.rs` — key handling
  - `crates/agent-tui/src/tui/draw.rs`/`crates/agent-tui/src/tui/render.rs` — rendering
  - `crates/agent-tui/src/tui/app.rs` — state
  - `crates/agent-tui/src/tui/commands.rs` — slash commands
  - `crates/agent-tui/src/tui/stream_handler.rs` — StreamEvent → App mutation
- **Tests** live in `#[cfg(test)] mod tests { ... }` at the bottom of each file.
- **Settings module convention:** `schema.rs` (definitions) → `input.rs` (key handling inside modal) → `draw.rs` (modal rendering) → handled by `crates/agent-tui/src/tui/mod.rs::apply_setting()`.
- **Re-exports** happen at module roots (`crates/agent-engine/src/tools/mod.rs`, `crates/agent-core/src/core/mod.rs`) and at each crate root (`lib.rs`). Prefer using the crate-root re-exports: `synaps_cli::Runtime`, `synaps_cli::config::...`, `synaps_cli::models::...`.

### Notable Docs

| File | Purpose |
|------|---------|
| `docs/extensions/README.md` | Extension user guide (install, configure, write your own) |
| `docs/extensions/protocol.md` | JSON-RPC protocol spec for extension authors |
| `docs/open-provider-issues.md` | Known provider-specific bugs and workarounds |

---

## The Runtime Struct

Located in `crates/agent-engine/src/runtime/mod.rs`. The single source of truth for a session.

Owns: `model`, `thinking_budget`, `system_prompt`, `ToolRegistry` (behind `Arc<RwLock>`), HTTP client, telemetry level, limits (`max_tool_output`, `bash_timeout`, `bash_max_timeout`, `subagent_timeout`, `api_retries`).

Key entry points:
- `run_single(&self, prompt)` → `Result<String>` — one-shot, no streaming. Used by headless paths.
- `run_stream(&self, prompt, cancel)` → stream of `StreamEvent` — fire-and-forget (synthesizes messages).
- `run_stream_with_messages(...)` → stream with caller-supplied message history. **Used by TUI.**

Config: `Runtime::apply_config(&SynapsConfig)` at startup; setters (`set_model`, `set_thinking_budget`, etc.) for live updates.

Runtime is `Clone` (cheap — uses `Arc` internally) so subagents can fork from a parent.

---

## Known Tech Debt

Things an agent should know about, but not necessarily fix in-passing:

- **`crates/agent-engine/src/tools/agent.rs`** is legacy, superseded by the `subagent` tools. Kept for compatibility with older agent definitions. Remove after deprecation window.
- **Theme changes require restart.** `Theme` is captured by long-lived render state; refactor to use `Rc<RefCell<Theme>>` or similar if live-swap becomes important.
- **Watcher subsystem** (`crates/agent-engine/src/watcher/`, `crates/agent-engine/src/cmd/agent.rs`) is being evaluated for removal from the main repo. Don't invest in deep refactors there without checking with project owner first.
- **`run_script` keybind action is a stub** (`crates/agent-tui/src/tui/input.rs` — TODO). Needs path validation before implementation.
- **Sliding-4 cache strategy is over-engineered** — bench data shows `single-last` is equivalent (see Prompt Caching Strategy). Simplification pending.
- **`gamba.rs`** — easter egg. Yes, really. Leave it alone.

---

## Watcher Subsystem (brief)

The watcher daemon (`target/release/synaps (watcher subcommand)`) supervises headless `synaps agent` processes. Each agent lives at `~/.synaps-cli/watcher/{name}/` with `config.toml`, `soul.md` (system prompt), `handoff.json` (state from last session), `stats.json`, `heartbeat` (timestamp file), and `logs/`.

Trigger modes:
- `manual` — runs only when deployed via `watcher deploy`
- `always` — auto-restart with cooldown
- `watch` — triggered by file changes (via `notify` crate)

Limits (per-agent, in `config.toml`): `max_session_tokens`, `max_session_duration_mins`, `max_session_cost_usd`, `max_daily_cost_usd`, `max_tool_calls`, `cooldown_secs`, `max_retries`.

When a limit is hit, the agent is prompted to call the `watcher_exit` tool to write a handoff. See `crates/agent-engine/src/tools/watcher_exit.rs` and `crates/agent-engine/src/watcher/supervisor.rs`.

IPC is over a Unix socket (`src/watcher/ipc.rs`). Commands: `deploy`, `status`, `stop`, `logs`.

---

## Tool Reference (for agents running INSIDE SynapsCLI)

This is the runtime tool surface. An LLM agent running in the TUI, synaps agent, or as a subagent sees these tools.

### `bash`
Execute shell commands via `bash -c`.

| Parameter | Type | Req | Default | Notes |
|---|---|---|---|---|
| `command` | string | ✓ | — | |
| `timeout` | integer | | 30 | Seconds, max 300 |

ANSI stripped. Output truncated at 30KB. `kill_on_drop` on timeout. Combined stdout+stderr.

### `read`
Read file with line numbers.

| Parameter | Type | Req | Default | Notes |
|---|---|---|---|---|
| `path` | string | ✓ | — | `~` expands |
| `offset` | integer | | 0 | 0-indexed |
| `limit` | integer | | 500 | |

UTF-8 validated. Binary files error with suggestion to use `bash` + `xxd`.

### `write`
Overwrite or create files. Atomic (temp file + rename). Creates parent dirs. Returns line + byte count.

| `path` (string, req) | `content` (string, req) |

### `edit`
Surgical replacement. `old_string` must match exactly once.

| `path` (string, req) | `old_string` (string, req) | `new_string` (string, req) |

### `grep`
Recursive regex search.

| Parameter | Type | Req | Default | Notes |
|---|---|---|---|---|
| `pattern` | string | ✓ | — | |
| `path` | string | | `.` | |
| `include` | string | | — | Glob filter |
| `context` | integer | | — | Lines before/after |

Excludes `.git`, `node_modules`, `target`. 15s timeout. Output truncated at `max_tool_output` (default 30KB, shared limit).

### `find`
Glob-based file search.

| Parameter | Type | Req | Default | Notes |
|---|---|---|---|---|
| `pattern` | string | ✓ | — | |
| `path` | string | | `.` | |
| `type` | string | | — | `"f"` or `"d"` |

Same excludes as grep. 10s timeout.

### `ls`
`ls -lah` output.

| `path` (string, optional, default `.`) |

### `subagent`
Dispatch a specialist. **Not available to subagents.**

| Parameter | Type | Req | Default | Notes |
|---|---|---|---|---|
| `task` | string | ✓ | — | |
| `agent` | string | * | — | Loads `~/.synaps-cli/agents/{name}.md` |
| `system_prompt` | string | * | — | Inline alternative to `agent` |
| `model` | string | | sonnet | Override |
| `timeout` | integer | | 300 | Seconds |

*Must provide `agent` OR `system_prompt`.

Runs in isolated thread with its own tokio runtime. Core tools only (no subagent/MCP). Logs to `~/.synaps-cli/logs/subagents/`. Output prefixed `[subagent:{name}]`. Returns partial results on timeout.

### `connect_mcp_server`
Connect to an MCP server defined in `~/.synaps-cli/mcp.json`. Tools registered as `ext__{server}__{tool}`. 30s request timeout.

(Renamed from `mcp_connect` — Anthropic's API rejects tool names starting with lowercase `mcp_` due to rate limit pool misrouting, yielding 400s.)

| `server` (string, req) |

### `load_skill`
Load behavioral guidelines. Discovery roots: `.synaps-cli/plugins/`, `.synaps-cli/skills/`, `~/.synaps-cli/plugins/`, `~/.synaps-cli/skills/`. Plugin = dir with `.synaps-plugin/plugin.json`. Collision resolution: built-ins > bare skill names > qualified `plugin:skill`.

| `skill` (string, req) — `name` or `plugin:name` |

### `shell_start` / `shell_send` / `shell_end`
Stateful PTY sessions. Returns a `session_id` from `shell_start`; use with `shell_send` to interact and `shell_end` to clean up. For interactive programs (REPLs, SSH, etc.). See `crates/agent-engine/src/tools/shell/` for the full state machine.

### `watcher_exit`
**Watcher agents only.** Writes `handoff.json`, triggers shutdown.

| Parameter | Type | Req | Default |
|---|---|---|---|
| `reason` | string | ✓ | — |
| `summary` | string | ✓ | — |
| `pending` | array[string] | | `[]` |
| `context` | object | | `{}` |

---

## Quick-Reference Summary

| Tool | Required | Optional | Purpose |
|------|----------|----------|---------|
| `bash` | command | timeout | Shell execution |
| `read` | path | offset, limit | File reading |
| `write` | path, content | — | File creation |
| `edit` | path, old_string, new_string | — | Surgical editing |
| `grep` | pattern | path, include, context | Regex search |
| `find` | pattern | path, type | File discovery |
| `ls` | — | path | Directory listing |
| `subagent` | task | agent, system_prompt, model, timeout | Agent dispatch |
| `connect_mcp_server` | server | — | MCP server connection |
| `load_skill` | skill | — | Behavioral guidelines |
| `shell_start` | — | cwd, env, … | Start PTY session |
| `shell_send` | session_id, input | timeout_ms | Interact with PTY |
| `shell_end` | session_id | — | Close PTY |
| `subagent_start` | task | agent, system_prompt, model, timeout | Reactive dispatch (non-blocking) |
| `subagent_status` | handle_id | — | Poll reactive subagent state |
| `subagent_steer` | handle_id, message | — | Inject guidance mid-run |
| `subagent_collect` | handle_id | — | Collect result (non-blocking poll) |
| `subagent_resume` | handle_id, instructions | — | Resume finished/timed-out subagent with prior context |
| `watcher_exit`* | reason, summary | pending, context | Watcher handoff |

*Watcher agents only. Subagents cannot use `subagent`, `connect_mcp_server`, `load_skill`, `watcher_exit`.

---

## Reactive Subagent Tools

```
subagent_start(agent, task, ...)        → {"handle_id": "sa_1", "status": "running"}
subagent_status(handle_id)              → {"status": "running", "partial_output": "..."}
subagent_steer(handle_id, message)      → {"acknowledged": true}
subagent_collect(handle_id)             → {"status": "completed", "output": "full result"}
subagent_resume(handle_id, instructions) → new handle_id; prior conversation prepended as context
```

Use `subagent` for simple sequential delegation (blocks until done).
Use `subagent_start` for parallel execution or when you want to continue working while the subagent runs.
Use `subagent_resume` on finished/timed-out/failed handles to continue with new instructions — the original handle stays readable.

Implementation: `crates/agent-engine/src/tools/subagent/{oneshot,start,status,steer,collect,resume}.rs`.

---

## Session & Chain Naming

Sessions and compaction lineages can be aliased for easy resume.

- `/saveas <name>` — alias the current session (`[a-z0-9-]{1,40}`, unique, collision-checked). `/saveas` (no arg) clears it. `/sessions` shows `[@name]` on named sessions.
- `/chain name <name>` — bookmark the current session's lineage. Stored at `~/.synaps-cli/chains/<name>.json`. On `/compact`, the pointer auto-advances to the new session.
- `/chain list` — all named chains (`*` marks the active one).
- `/chain unname <name>` — remove a chain bookmark.
- `/chain` (no args) — show lineage + "bookmarked by: @name" if present.

Resolution (`crates/agent-core/src/core/session.rs::resolve_session()`) tries **chain name → session name → partial ID** in that order. Used by `synaps --continue <NAME_OR_ID>`, `/resume`, and server `--continue`. The resolution path is surfaced to the user via a system message (e.g. `↳ resolved via chain 'foo'`).

---

## Event Bus

External systems push events into running sessions via `synaps send`:
```bash
synaps send "message" --source cli --severity medium
```

Events appear as styled cards and auto-trigger model turns. During streaming, events buffer and flush after the current response.

---

## Security Notes

For agents touching this code — what's protected, what isn't, and what to never weaken.

**Protections that exist (don't break them):**
- Auth tokens: `~/.synaps-cli/auth.json` — fs4 advisory locks + mode `0600` (`crates/agent-core/src/core/auth/storage.rs`)
- Config: `write_config_value` writes via temp file + rename, sets `0600` (`crates/agent-core/src/core/config.rs`) — it holds provider API keys
- Telemetry log: `0600` + `O_NOFOLLOW` on the append path (`crates/agent-engine/src/runtime/telemetry.rs`)
- Event registry/sockets: dirs `0700`, session sockets `0600` after bind, inbox refuses symlinks + caps events at 256KB (`crates/agent-engine/src/events/`)
- Plugin agent resolution: identifier validation on both sides of `plugin:agent`, no path traversal (`crates/agent-engine/src/tools/agent.rs`)
- `ProviderConfig` `Debug` redacts `api_key` — but `Serialize` does NOT. Never serialize provider config into logs or output.

**Known soft spots (be aware, warn users, don't make worse):**
- **`bash` is unsandboxed.** Full user privileges, no path restrictions. The model can run anything.
- **The `read` tool can read `~/.synaps-cli/config` and the auth store** — a prompt-injected agent can exfiltrate API keys into context, session logs, or subagent logs (`~/.synaps-cli/logs/subagents/`). There is no denylist.
- **Event bus has no sender authentication.** Filesystem perms (0600/0700) stop other *users*, but any same-UID process can drop JSON into the inbox or hit the session socket and trigger an unattended model turn. Events are untrusted input — prompt injection by design. Treat event content accordingly.
- **Watcher IPC** (`src/watcher/ipc.rs`): socket chmod'd `0600` after bind (small race window); no command-level auth on deploy/stop beyond filesystem perms.
- **Marketplace installs are TOFU** (trust-on-first-use, per `github.com/<owner>` host key — `crates/agent-engine/src/skills/marketplace.rs`). No signature verification — a compromised upstream repo ships straight to users on update.
- **`run_script` keybind is an unimplemented stub** — when implementing, validate the path stays inside the plugin dir.
- **`inject_prompt` keybinds** let plugins submit text as the user. Audit third-party plugin manifests before install.

---

*Whatever happens, happens.*
