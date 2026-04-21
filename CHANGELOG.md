# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added
- **Event Bus** — universal message ingestion for agent sessions
  - `synaps send` CLI command with atomic file writes
  - inotify inbox watcher via `notify` crate (spawn_blocking, non-blocking)
  - Priority EventQueue with severity ordering (Critical→front, High→after)
  - `tokio::sync::Notify` for instant TUI wake on event push
  - Events auto-trigger model turns when agent is idle
  - Events buffer during streaming via `pending_events`, flush on completion
  - Styled TUI event cards with severity icons (🔴🟠🟡🔵)
  - XML-wrapped event format with prompt injection hardening
  - 256KB file size cap, symlink guard, 0700 permissions
- **Reactive Subagents** — dispatch, poll, steer, collect
  - `subagent_start` — spawn and return handle_id immediately
  - `subagent_status` — non-blocking progress snapshot
  - `subagent_steer` — inject guidance mid-flight via steering channel
  - `subagent_collect` — non-blocking result check
  - `subagent_resume` — restart timed-out agents (stub)
  - `SubagentHandle` with shared `RwLock<SubagentState>`
  - `SubagentRegistry` with cleanup_finished on stream Done
  - Abort cancels all running reactive subagents
  - Thread handles stored for graceful shutdown
  - 13 unit tests for handle + registry
- **Chain Sessions** — `/compact` creates linked child session
  - Old session preserved on disk with `compacted_into` forward link
  - New session starts with `parent_session` back link
  - `/chain` command walks the session lineage
  - System prompt included in compaction summary
  - Configurable compaction model (defaults to Sonnet)
  - ModelPicker for compaction model in settings
  - Non-blocking compaction with spinner animation
  - Compacted summary hidden from TUI display
  - Message queuing during compaction
- **`respond` tool** (stub — returns honest failure until wired)
- **`send_channel` tool** (stub — returns honest failure until wired)
- **`/status` command + `synaps status` subcommand**: check account usage (5-hour, 7-day, Sonnet) with progress bars and reset countdowns. Hits OAuth usage API.
- **`/compact` slash command**: summarize & compact conversation history when context gets long
  - Structured checkpoint format (goals, progress, decisions, file ops, next steps)
  - Iterative compaction — re-compacting merges new work into existing summary
  - File operation tracking (read/write/edit paths preserved across compactions)
  - Custom focus instructions via `/compact <focus>`
  - Uses dedicated low-effort API call (no tools, summarization system prompt)
- **`context_window` setting wired to API**: `200k` (default) omits beta header; `1m` sends `context-1m-2025-08-07` on supported models (Opus 4.6+, Sonnet 4.x); previously was UI-only display cap
- **Claude Code marketplace compatibility**: probe both `.synaps-plugin` and `.claude-plugin` layouts, `${CLAUDE_PLUGIN_ROOT}` substitution in skill bodies
- **Plugins subdir sources and cascade uninstall**: install from subdir-based plugin repos, cascade-remove plugins when their marketplace is deleted
- **Settings → Plugins marketplace overlay**: "Open Plugin Marketplace" action row in Settings, opens plugins modal as nested overlay
- **Hidden binary**: GamblersDen bundled as `hidden` binary alongside `synaps`
- **Single binary architecture**: all 8 binaries consolidated into `synaps`
  - `synaps` (no args) = TUI (was `chatui`)
  - `synaps run` = one-shot prompt (was `cli run`)
  - `synaps chat` = streaming chat (was `chat`)
  - `synaps server` = WebSocket API (was `server`)
  - `synaps client` = WS client (was `client`)
  - `synaps agent` = headless worker (was `synaps-agent`)
  - `synaps watcher` = supervisor (was `watcher`)
  - `synaps login` = OAuth (was `login`)
- **Live theme preview in /settings**: scroll themes to preview, Enter confirms, Esc reverts
- **Theme hot-reload**: `/theme <name>` applies instantly without restart (ArcSwap)
- **Settings picker scroll**: theme/model picker scrolls with cursor
- **Adaptive thinking for Opus 4.7+**: `{type: "adaptive", display: "summarized"}` with effort mapping (xhigh/high/medium/low/adaptive)
- **Model-agnostic context window**: bar denominator adapts per-model (1M Opus 4.7, 200K Sonnet/Haiku)
- **Per-turn context tracking**: usage bar shows actual request size, not cumulative cost
- **Effort parameter**: thinking depth on adaptive models controlled via `output_config.effort`
- **"adaptive" thinking option**: new cycler value in `/settings` — lets model decide thinking depth
- **Tab-cycle for slash commands**: `/s` + Tab cycles through sessions → settings → system
- **Streaming command guard**: known slash commands no longer leak into model stream as steering
- **Usage log opt-in**: `SYNAPS_USAGE_LOG=1` writes to `~/.cache/synaps/usage.log` (0600, O_NOFOLLOW)
- **Opus 4.6 in model picker**: was missing from `/settings` model list
- **`settings` + `plugins` in tab-complete**: were missing from `BUILTIN_COMMANDS`

### Fixed
- Compaction no longer overwrites original session (loads from disk)
- Event content sanitized against prompt injection (XML tags, case-insensitive)
- Atomic inbox writes (.json.tmp → .json rename)
- inotify watcher runs on spawn_blocking (no longer starves tokio runtime)
- Events during streaming buffered and flushed after MessageHistory
- Spurious auto-triggers prevented by event_received guard
- push() calls notify_one() (was missing — events silently queued)
- Session save ordering: new session saved before old session updated
- chars().count() moved outside lock scope
- High-priority event FIFO ordering (was LIFO among Highs)
- Queue-full events left in inbox for retry (not silently dropped)
- push_priority logs evicted event ID
- **Session file corruption**: atomic writes via write-to-tmp then rename
- **Tool input parse errors surfaced to model**: malformed tool_use JSON no longer silently falls through to empty input; model sees `invalid tool input JSON: ...` and can self-correct
- **Custom theme crash**: `unreachable!()` in draw replaced with graceful fallback colors for non-Rgb themes
- ASCII logo alignment: use unicode display width for consistent centering
- 'default' theme missing from settings picker
- Context bar pinned at 100% after 2-3 turns (was using cumulative tokens / hardcoded 200K)
- Thinking blocks invisible on Opus 4.7 (display defaulted to "omitted")
- `budget_tokens: 0` sentinel leaked to non-adaptive models → 400 error
- `/settings` cycling capped at xhigh (`apply_setting` silently rejected "adaptive")
- Stale test asserting `thinking_level_for_budget(0) == "low"` (production returns "adaptive")
- Usage log world-readable at `/tmp/` → moved to `~/.cache/` with 0600

### Changed
- Compaction logic moved from chatui to `core/compaction.rs`
- `SubagentHandle`/`Registry`/`Status` moved to `runtime/subagent.rs`
- `ApiOptions` struct replaces `use_1m_context: bool` threading
- `build_auth_header` + `build_beta_header` extracted as helpers
- `clone_repo` helper deduplicates plugin installer
- All compaction prompts colocated in `core/compaction.rs`
- Tool descriptions guide model choice (subagent vs subagent_start)
- Stub tools unregistered from tool registry (model doesn't see unimplemented tools)
- SubagentState uses RwLock instead of Mutex
- **`define_settings!` macro**: settings schema + apply handler defined once in `settings/defs.rs` via declarative macro — zero drift possible (replaced manual sync + parity tests)
- **Single source of truth for commands**: removed duplicate `ALL_COMMANDS` array; `commands.rs` now sources from `skills::BUILTIN_COMMANDS`
- **`src/cmd_*.rs` → `src/cmd/` module**: subcommand handlers moved to dedicated directory, `cmd_` prefix stripped
- Binary name: `chatui`/`synaps-cli`/`synaps-agent` → `synaps` (single binary with subcommands)
- `src/chatui/main.rs` → `src/chatui/mod.rs` (module, not binary)
- watcher spawns `synaps agent` instead of standalone `synaps-agent`
- `thinking_level_for_budget()` consolidated from 4 copies into single source of truth in `core/models.rs`
- `DEFAULT_LEGACY_ADAPTIVE_FALLBACK` constant replaces magic `16384` in clamp sites
- Dead-code warnings suppressed with explanatory comments (reaper handles, settings help field)
- Auto-cache toggle removed (manual breakpoints won: 90% vs 53%)
- README rewritten from internal documentation to product landing page

### Removed
- `SPEC-WATCHER.md` — internal spec, not needed in repo
- Auto-cache config toggle (`auto_cache = true/false`)

## [0.1.0] — 2026-04-12

### Added
- Interactive TUI (`chatui`) with streaming, markdown, syntax highlighting
- Headless chat (`chat`) for scripting and piping
- Autonomous agent supervision (`watcher`) with heartbeat, cost limits, handoff
- 10 built-in tools: bash, read, write, edit, grep, find, ls, subagent, mcp_connect, load_skill
- Interactive shell sessions: shell_start, shell_send, shell_end
- 18 color themes
- MCP integration with lazy server spawning
- Skills & plugins subsystem
- OAuth + API key authentication
- WebSocket server/client transport
- `/settings` full-screen modal
- `/plugins` management UI
- Session persistence and `/resume`
