# AGENTS.md — SynapsCLI Developer & Agent Guide

This is the onboarding doc for any agent (Claude Code, Cursor, Aider, or SynapsCLI itself) touching this codebase. Read this first. If you only read one file, read this one.

SynapsCLI is a terminal-native AI agent runtime written in Rust. ~21K LOC across 87 `.rs` files. Single crate (`synaps-cli`) producing **one binary** (`synaps`) with subcommands. Talks to Anthropic's API natively, plus any OpenAI-compatible provider (Groq, Cerebras, NVIDIA, local Ollama, etc.) via the built-in provider engine. Streams SSE, dispatches tools, renders a TUI.

---

## Build & Test

```bash
cargo build --release                    # full release build (lto, single codegen unit, strip)
cargo build                              # dev build — faster compile, slower runtime
cargo test --lib                         # most tests
cargo test --lib -- --test-threads=1     # required for PTY tests in src/tools/shell/pty.rs
cargo clippy --all-targets               # linting
```

**Minimum Rust:** 1.80 (edition 2021).
**Config path:** `~/.synaps-cli/config` (plain `key = value`, see `src/core/config.rs`).
**Binary:** `target/release/synaps` — single binary, dispatched via subcommand.

- `synaps` (no args) — interactive TUI (the main product)
- `synaps --continue [NAME_OR_ID]` — resume last session, or resolve a chain bookmark / session alias / partial session ID via `resolve_session()` (chain name → session name → partial ID)
- `synaps chat` — single-shot CLI chat
- `synaps run` — non-interactive one-shot command
- `synaps agent` — headless worker managed by the watcher
- `synaps watcher` — supervisor daemon
- `synaps login` — OAuth flow
- `synaps server` / `synaps client` — WebSocket relay (less-used)

**Test quirks:**
- 7 PTY tests in `src/tools/shell/pty.rs` and `src/tools/shell/{start,send,end}.rs` fail under parallel due to TTY contention. Use `--test-threads=1`. Not a bug.
- Tests use `tempfile` crate. No fixtures checked in.

---

## Project Structure

```
src/
├── lib.rs                — crate root; re-exports Runtime, ToolRegistry, config, models, etc.
├── main.rs               — unified CLI entry point, subcommand dispatch
├── cmd_*.rs              — subcommand handlers (run, chat, server, client, agent, login, watcher)
├── core/                 — shared primitives
│   ├── config.rs         — SynapsConfig, load/write, profile resolution
│   ├── models.rs         — KNOWN_MODELS, thinking_level_for_budget, context_window_for_model
│   ├── session.rs        — on-disk session persistence (JSONL), `/saveas` naming, `resolve_session()` (chain → name → partial ID)
│   ├── chain.rs          — named chain bookmarks (`~/.synaps-cli/chains/<name>.json`), auto-advance on `/compact`
│   ├── auth/             — OAuth PKCE flow, token storage (fs4-locked, mode 600)
│   ├── protocol.rs       — WebSocket wire format (server/client)
│   ├── error.rs          — SynapsError type
│   ├── logging.rs        — tracing subscriber setup
│   └── watcher_types.rs  — shared types for watcher IPC
├── runtime/              — THE BRAIN
│   ├── mod.rs            — Runtime struct, orchestration loop
│   ├── api.rs            — Anthropic API body construction + SSE parsing
│   ├── stream.rs         — tool dispatch from streamed tool_use events
│   ├── helpers.rs        — annotate_cache_breakpoint, drain_steering, etc.
│   ├── types.rs          — StreamEvent enum (the wire between runtime and UIs)
│   ├── auth.rs           — auth token refresh before request
│   └── openai/           — OpenAI-compatible provider engine
│       ├── mod.rs        — Provider enum, resolve_route(), try_route()
│       ├── registry.rs   — 17 providers, 55+ models, env+config key resolution
│       ├── types.rs      — ChatMessage, ToolCall, ChatRequest, OaiEvent, ProviderConfig
│       ├── wire.rs       — SSE parser + StreamDecoder (HashMap-based tool call accumulation)
│       ├── translate.rs  — Anthropic↔OpenAI message/tool/event translation
│       ├── stream.rs     — call_oai_stream_inner (streaming path)
│       └── ping.rs       — /ping health check (parallel, non-blocking)
├── tools/                — 10 built-in tools, each impls the Tool trait
│   ├── mod.rs            — Tool trait, ToolContext
│   ├── registry.rs       — ToolRegistry::new() registers all built-ins
│   ├── {bash,read,write,edit,grep,find,ls}.rs  — core filesystem/shell tools
│   ├── subagent.rs       — spawns a child Runtime in an isolated thread
│   ├── agent.rs          — (legacy — prefer subagent.rs)
│   ├── watcher_exit.rs   — graceful-exit tool (watcher agents only)
│   ├── shell/            — stateful PTY shell (start/send/end) — session manager
│   └── util.rs           — strip_ansi, expand_path, NEXT_SUBAGENT_ID
├── chatui/               — the TUI (module, entered via default `synaps` subcommand)
│   ├── mod.rs            — event loop + apply_setting()
│   ├── app.rs            — App state, record_cost(), line cache
│   ├── input.rs          — key handling, process_submit()
│   ├── draw.rs           — render dispatch
│   ├── render.rs         — message rendering
│   ├── markdown.rs       — markdown → styled lines
│   ├── highlight.rs      — syntect-backed syntax highlighting
│   ├── stream_handler.rs — StreamEvent → UI mutation
│   ├── commands.rs       — slash-command dispatch (ALL_COMMANDS, handle_command)
│   ├── theme/            — 17 built-in palettes + user TOML loader
│   ├── settings/         — /settings modal (schema, input, draw)
│   ├── plugins/          — /plugins modal
│   └── gamba.rs          — easter egg. Don't touch.
├── watcher/              — supervisor daemon
│   ├── mod.rs            — subsystem entry (invoked by `synaps watcher`)
│   ├── supervisor.rs     — per-agent lifecycle, limits, retries
│   ├── ipc.rs            — Unix socket protocol (deploy, status, stop)
│   └── display.rs        — `watcher status` renderer
├── mcp/                  — Model Context Protocol client
│   ├── connection.rs     — JSON-RPC over stdio to MCP servers
│   ├── lazy.rs           — lazy server spawn (don't pay until connect_mcp_server called)
│   └── tool.rs           — MCP tools wrapped as Tool impls
└── skills/               — skill discovery + command registry
    ├── loader.rs         — walks .synaps-cli/{plugins,skills} roots
    ├── manifest.rs       — plugin.json / marketplace.json parsers
    ├── registry.rs       — CommandRegistry: built-ins + skill names → tab-complete
    ├── marketplace.rs    — plugin install from marketplace
    └── tool.rs           — load_skill Tool impl
```

---

## The Request Lifecycle

This is the single most important flow to understand.

1. **User input** → `chatui/input.rs::process_submit()` builds a user message, pushes it into `App.messages`, kicks off a stream.
2. **Stream kickoff** → `Runtime::run_stream_with_messages()` in `runtime/mod.rs` (~line 377).
3. **API body build** → `runtime/api.rs::call_api_stream()` (~line 30). Steps:
   - **Provider routing** → `openai::try_route(model, ...)` checks if the model has a provider prefix (e.g. `groq/llama-3.3-70b`). If yes, routes through `openai/stream.rs` instead of Anthropic. If no, falls through to Anthropic.
   - Clone messages, strip UI-only fields.
   - `HelperMethods::annotate_cache_breakpoint(&mut cleaned_messages)` — see caching section below.
   - Look up thinking config based on model: adaptive (`{type: "adaptive"}` + `output_config.effort`) for Opus 4.7+ / Sonnet 4.7+ / 5.x, else legacy (`{type: "enabled", budget_tokens: N}`). Gated by `model_supports_adaptive_thinking()` in `core/models.rs`.
   - Serialize tool schemas (`ToolRegistry::schemas_json()`).
   - POST to `https://api.anthropic.com/v1/messages` with `stream: true`.
4. **SSE parse** → line-by-line in `api.rs` (~line 200+). Emits `StreamEvent`s (TextDelta, ThinkingDelta, ToolUse, Usage, MessageStop, Error).
5. **Tool dispatch** → `runtime/stream.rs` collects `ToolUse` blocks, executes them in parallel via `tokio::spawn`, feeds `tool_result` blocks back into the next turn.
6. **Loop** → steps 3–5 repeat until `stop_reason != "tool_use"` (typically `"end_turn"`).
7. **UI update** → `chatui/stream_handler.rs` consumes `StreamEvent`s and mutates `App`.

`StreamEvent` (in `runtime/types.rs`) is the wire format between Runtime and any UI. Add new event variants here if you need to surface something new.

---

## Key Patterns

### Adding a New Tool

1. Create `src/tools/my_tool.rs` with a struct implementing the `Tool` trait:
   ```rust
   #[async_trait::async_trait]
   pub trait Tool: Send + Sync {
       fn name(&self) -> &str;
       fn description(&self) -> &str;
       fn parameters(&self) -> serde_json::Value;       // JSON Schema
       async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String>;
   }
   ```
   See `src/tools/mod.rs:64`.
2. Re-export in `src/tools/mod.rs` (`pub use my_tool::MyTool;`).
3. Register in `src/tools/registry.rs::ToolRegistry::new()` — add to the `vec![]`.
4. If it streams output, use `ctx.tx_delta` (UnboundedSender<String>) to push deltas.
5. If it's restricted (e.g. watcher-only), gate on `ctx.watcher_exit_path.is_some()` or similar.

The tool's `parameters()` JSON schema is what the model sees. Be precise — bad schemas lead to malformed tool calls.

### Adding a New Setting (the 5-site sync — KNOWN PAIN POINT)

Adding a setting requires touching 5 files. Miss one and you get silent failures.

1. **`src/chatui/settings/schema.rs`** — add a `SettingDef` to `ALL_SETTINGS`. Pick `EditorKind::Cycler(&[...])`, `Text { numeric }`, `ModelPicker`, or `ThemePicker`.
2. **`src/chatui/mod.rs::apply_setting()`** — add a match arm that mutates `Runtime` (e.g. `runtime.set_foo(v)`).
3. **`src/core/config.rs::load_config()`** — add a branch to parse the key from the config file.
4. **`src/chatui/commands.rs`** — if it has a slash command (e.g. `/foo`), add to `ALL_COMMANDS` and handle in `handle_command`.
5. **`src/skills/mod.rs`** — add to `BUILTIN_COMMANDS` (for tab-complete via `CommandRegistry`).

The `every_setting_key_is_known_to_load_config` test in `schema.rs` catches step 3 omissions. The other sites are not tested. Be careful.

**Tech debt:** `ALL_COMMANDS` (commands.rs:13) and `BUILTIN_COMMANDS` (skills/mod.rs:49) are duplicated lists. They must be kept in sync manually. Should be unified.

### Adding a New Model

1. `src/core/models.rs::KNOWN_MODELS` — add `(id, description)` tuple (Anthropic models only).
2. For OpenAI-compatible provider models: add to `src/runtime/openai/registry.rs` in the provider's `models` array as `(model_id, label, tier)`.
3. If it supports adaptive thinking: update `model_supports_adaptive_thinking()` (~line 26).
4. If context window differs: update `context_window_for_model()` (~line 94).
5. Pricing: update the match in `src/chatui/app.rs::record_cost()` (~line 256). Default falls back to Sonnet pricing.
6. There are existing tests in `core/models.rs` — extend them.

### Adding a New Provider

All OpenAI-compatible providers live in `src/runtime/openai/registry.rs`.

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

1. Add a `Theme::my_theme()` method in `src/chatui/theme/palettes.rs` returning a populated `Theme` struct (all ~30 color fields).
2. Register in `src/chatui/theme/mod.rs::Theme::builtin()` (~line 110) — add a `match` arm.
3. Add the theme name to the list returned by `src/chatui/settings/mod.rs::theme_options()`.
4. Test via `/settings → Appearance → Theme` or config `theme = my-theme`. Requires chatui restart to apply.

### Adding a New Slash Command

1. Add name to `BUILTIN_COMMANDS` (skills/mod.rs:49).
2. If it should work during streaming, add to `STREAMING_COMMANDS` (commands.rs:20).
3. Add a match arm in `handle_command()` (commands.rs).
4. If it needs async work or opens a modal, extend `CommandAction` enum and handle in `mod.rs` event loop.

### Plugin Agent Resolution

Agents from installed plugins can be dispatched via `plugin:agent` namespaced syntax:

```
subagent(agent: "dev-tools:sage", task: "...")
```

Resolution order in `resolve_agent_prompt()` (src/tools/agent.rs):
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
- `inject_prompt` — submits text as user message (field: `prompt`)
- `run_script` — runs a script from the plugin dir (field: `script`)

**Implementation path:**
1. `ManifestKeybind` (skills/keybinds.rs) — serde struct for plugin.json parsing
2. `KeybindRegistry` (skills/keybinds.rs) — collects + resolves conflicts
3. Built during `skills::register()` (skills/mod.rs) alongside command registry
4. Checked in `handle_key()` (chatui/input.rs) before the core match block
5. `parse_key()` / `format_key()` for notation ↔ KeyCombo conversion

**Priority:** Core (Ctrl+C, Esc, etc.) > user config (`keybind.*`) > plugin. Core binds are never overridable. User config always wins over plugins.

**User overrides** in `~/.synaps-cli/config`:
```
keybind.F5 = /compact        # override or add
keybind.F6 = disabled        # block a plugin bind
```

---

## Prompt Caching Strategy

This is non-obvious and critical. See `src/runtime/helpers.rs:34::annotate_cache_breakpoint`.

- **Manual breakpoint placement.** We don't use Anthropic's auto-cache.
- Anthropic allows up to 4 cache markers per request. We reserve 2 for tools + system prompt (placed elsewhere in `api.rs`), leaving **2 for conversational markers**.
- Breakpoints advance every **4 user messages**. The latest eligible user message gets a `cache_control: {type: "ephemeral"}` on its last content block.
- **Historical messages are NEVER modified.** Prefix stability = cache stability. Adding even a single field to an old message invalidates all downstream cache hits.
- Measured: **90% cache hit rate** vs ~53% with auto-cache. Manual wins.

If you touch `annotate_cache_breakpoint`, re-verify hit rates with `/debug cache` or the usage logs.

---

## Thinking Config by Model

Two code paths, gated by `model_supports_adaptive_thinking()`:

**Adaptive (Opus 4.7+, Sonnet 4.7+, 5.x):**
```json
"thinking": {"type": "adaptive", "display": "summarized"}
"output_config": {"effort": "low" | "medium" | "high" | "xhigh"}  // omitted if "adaptive"
```
No `budget_tokens` field — the API rejects it silently on these models (returns no thinking content, error S172).

**Legacy (Opus 4.6, Sonnet 4.6, Haiku, Opus 3.x):**
```json
"thinking": {"type": "enabled", "budget_tokens": N, "display": "summarized"}
```

**The "0 is adaptive" sentinel:** `Runtime::thinking_budget: u32` uses `0` to mean "adaptive (model decides)". Any consumer must handle this. If a user sets `thinking = adaptive` but the model is legacy, `thinking_level_for_budget(0)` returns `"adaptive"` but the legacy path clamps it to `DEFAULT_LEGACY_ADAPTIVE_FALLBACK = 16384` (matches "high"). See `core/models.rs:80` and `runtime/api.rs` (the clamp site — commit 5edcb86).

Mapping (`core/models.rs:68::thinking_level_for_budget`):
- `0` → `"adaptive"`
- `1..=2048` → `"low"`
- `2049..=4096` → `"medium"`
- `4097..=16384` → `"high"`
- `16385..` → `"xhigh"`

---

## Configuration Flow

```
~/.synaps-cli/config (or ~/.synaps-cli/{profile}/config)
  → core/config.rs::load_config()  — parses key = value, env var overrides
  → Runtime::apply_config()         — sets fields on Runtime
  → runtime/api.rs reads from Runtime at request time
  → chatui/mod.rs::apply_setting() — runtime mutation + write_config_value() for live /settings changes
```

`SYNAPS_PROFILE` env var selects a sub-directory under `~/.synaps-cli/` (e.g. `~/.synaps-cli/work/config`). Profile-specific files override root files. See `core/config.rs::resolve_read_path()`.

---

## Common Pitfalls

1. **5-site sync for settings** (see above). Miss one = silent failure.
2. **`thinking_budget: 0` sentinel.** Always handle the "adaptive" case. Legacy paths must clamp.
3. **Cache breakpoints are prefix-sensitive.** Any mutation to historical messages breaks the cache for all subsequent turns. Don't "fix up" old messages retroactively.
4. **PTY tests fail under parallel.** Use `--test-threads=1`. Not a bug — TTY fd contention.
5. **Binary swap requires process restart.** `cargo build` replaces `target/release/synaps` on disk but the running process keeps the old binary mmap'd. Must exit + relaunch to pick up changes. (Obvious once you know it, confusing the first time.)
6. **Two command lists** (`ALL_COMMANDS` vs `BUILTIN_COMMANDS`). Tech debt. Keep in sync or tab-complete breaks silently.
7. **Subagent has NO subagent.** No recursion. Subagents also lack `connect_mcp_server`, `load_skill`, `watcher_exit`. Enforced by skipping registration in `tools/subagent.rs`.
8. **Theme change requires restart.** The `apply_setting` path flags this with `"saved — restart to apply"`. Not a bug — `Theme` is captured by long-lived render state.
9. **MCP servers are lazy-spawned.** First `connect_mcp_server` pays the spawn cost. Tools are registered dynamically via `ToolContext::tool_register_tx` — this channel breaks the `Arc<ToolRegistry>` circularity.
10. **OAuth tokens are file-locked** via `fs4`. Concurrent chatui + watcher instances are safe, but a crashed process holding the lock will block others until its file is cleaned up.
11. **Provider model IDs contain slashes.** `nvidia/meta/llama-3.3-70b-instruct` — the first slash separates provider from model. `resolve_shorthand` uses `split_once('/')`. Nested slashes in model IDs (NVIDIA, DeepInfra) are preserved correctly.
12. **Anthropic auth is optional.** `get_auth_token()` returns `auth_type: "none"` if no credentials found. The app boots fine. Anthropic API calls fail lazily with a clear message pointing to `synaps login` or `/model groq/...`.
13. **`/compact` doesn't route through providers** — uses `call_api_simple` which is Anthropic-only. Known issue (see `docs/open-provider-issues.md`).
14. **Cost display is Claude-only.** The `$X.XX` in the status bar uses Claude pricing for all models. Non-Claude shows wrong numbers. Known issue.
15. **Config file contains API keys.** Written with `0600` permissions. `ProviderConfig` Debug impl redacts `api_key`. Don't log raw config values.

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

- **One file per tool** in `src/tools/*.rs`. Complex tools get a sub-directory (e.g. `src/tools/shell/`).
- **Chatui separation of concerns:**
  - `input.rs` — key handling
  - `draw.rs`/`render.rs` — rendering
  - `app.rs` — state
  - `commands.rs` — slash commands
  - `stream_handler.rs` — StreamEvent → App mutation
- **Tests** live in `#[cfg(test)] mod tests { ... }` at the bottom of each file.
- **Settings module convention:** `schema.rs` (definitions) → `input.rs` (key handling inside modal) → `draw.rs` (modal rendering) → handled by `main.rs::apply_setting()`.
- **Re-exports** happen at module roots (`tools/mod.rs`, `core/mod.rs`) and at the crate root (`lib.rs`). Prefer using the crate-root re-exports: `synaps_cli::Runtime`, `synaps_cli::config::...`, `synaps_cli::models::...`.

---

## The Runtime Struct

Located at `src/runtime/mod.rs:28`. The single source of truth for a session.

Owns: `model`, `thinking_budget`, `system_prompt`, `ToolRegistry` (behind `Arc<RwLock>`), HTTP client, limits (`max_tool_output`, `bash_timeout`, `bash_max_timeout`, `subagent_timeout`, `api_retries`).

Key entry points:
- `run_single(&self, prompt)` → `Result<String>` — one-shot, no streaming. Used by `cli` and `chat` binaries.
- `run_stream(&self, prompt, cancel)` → stream of `StreamEvent` — fire-and-forget (synthesizes messages).
- `run_stream_with_messages(...)` → stream with caller-supplied message history. **Used by chatui.**

Config: `Runtime::apply_config(&SynapsConfig)` at startup; setters (`set_model`, `set_thinking_budget`, etc.) for live updates.

Runtime is `Clone` (cheap — uses `Arc` internally) so subagents can fork from a parent.

---

## Known Tech Debt

Things an agent should know about, but not necessarily fix in-passing:

- **Command list duplication** (`ALL_COMMANDS` / `BUILTIN_COMMANDS`). Should be unified into one `pub const` consumed by both chatui and skills registry.
- **Settings require 5-site edits.** A macro or derive could collapse this.
- **`src/tools/agent.rs`** is legacy, superseded by `subagent.rs`. Kept for compatibility with older agent definitions. Remove after deprecation window.
- **Theme changes require restart.** `Theme` is captured by long-lived render state; refactor to use `Rc<RefCell<Theme>>` or similar if live-swap becomes important.
- **SPEC-WATCHER.md** — the watcher subsystem (`src/watcher/`, `src/cmd_agent.rs`) is being evaluated for removal from the main repo. Don't invest in deep refactors there without checking with project owner first.
- **`gamba.rs`** — easter egg. Yes, really. Leave it alone.

---

## Watcher Subsystem (brief)

The watcher daemon (`target/release/synaps (watcher subcommand)`) supervises headless `synaps agent` processes. Each agent lives at `~/.synaps-cli/watcher/{name}/` with `config.toml`, `soul.md` (system prompt), `handoff.json` (state from last session), `stats.json`, `heartbeat` (timestamp file), and `logs/`.

Trigger modes:
- `manual` — runs only when deployed via `watcher deploy`
- `always` — auto-restart with cooldown
- `watch` — triggered by file changes (via `notify` crate)

Limits (per-agent, in `config.toml`): `max_session_tokens`, `max_session_duration_mins`, `max_session_cost_usd`, `max_daily_cost_usd`, `max_tool_calls`, `cooldown_secs`, `max_retries`.

When a limit is hit, the agent is prompted to call the `watcher_exit` tool to write a handoff. See `src/tools/watcher_exit.rs` and `src/watcher/supervisor.rs`.

IPC is over a Unix socket (`src/watcher/ipc.rs`). Commands: `deploy`, `status`, `stop`, `logs`.

---

## Tool Reference (for agents running INSIDE SynapsCLI)

This is the runtime tool surface. An LLM agent running in chatui, synaps agent, or as a subagent sees these tools.

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

Excludes `.git`, `node_modules`, `target`. 15s timeout. 50KB output cap.

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
Stateful PTY sessions. Returns a `session_id` from `shell_start`; use with `shell_send` to interact and `shell_end` to clean up. For interactive programs (REPLs, SSH, etc.). See `src/tools/shell/` for the full state machine.

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
| `watcher_exit`* | reason, summary | pending, context | Watcher handoff |

*Watcher agents only. Subagents cannot use `subagent`, `connect_mcp_server`, `load_skill`, `watcher_exit`.

---

## Reactive Subagent Tools

```
subagent_start(agent, task, ...)   → {"handle_id": "sa_1", "status": "running"}
subagent_status(handle_id)         → {"status": "running", "partial_output": "..."}
subagent_steer(handle_id, message) → {"acknowledged": true}
subagent_collect(handle_id)        → {"status": "completed", "output": "full result"}
```

Use `subagent` for simple sequential delegation (blocks until done).
Use `subagent_start` for parallel execution or when you want to continue working while the subagent runs.

---

## Session & Chain Naming

Sessions and compaction lineages can be aliased for easy resume.

- `/saveas <name>` — alias the current session (`[a-z0-9-]{1,40}`, unique, collision-checked). `/saveas` (no arg) clears it. `/sessions` shows `[@name]` on named sessions.
- `/chain name <name>` — bookmark the current session's lineage. Stored at `~/.synaps-cli/chains/<name>.json`. On `/compact`, the pointer auto-advances to the new session.
- `/chain list` — all named chains (`*` marks the active one).
- `/chain unname <name>` — remove a chain bookmark.
- `/chain` (no args) — show lineage + "bookmarked by: @name" if present.

Resolution (`core/session.rs::resolve_session()`) tries **chain name → session name → partial ID** in that order. Used by `synaps --continue <NAME_OR_ID>`, `/resume`, and server `--continue`. The resolution path is surfaced to the user via a system message (e.g. `↳ resolved via chain 'foo'`).

---

## Event Bus

External systems push events into running sessions via `synaps send`:
```bash
synaps send "message" --source cli --severity medium
```

Events appear as styled cards and auto-trigger model turns. During streaming, events buffer and flush after the current response.

---

*Whatever happens, happens.*
