# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added
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
- Context bar pinned at 100% after 2-3 turns (was using cumulative tokens / hardcoded 200K)
- Thinking blocks invisible on Opus 4.7 (display defaulted to "omitted")
- `budget_tokens: 0` sentinel leaked to non-adaptive models → 400 error
- `/settings` cycling capped at xhigh (`apply_setting` silently rejected "adaptive")
- Stale test asserting `thinking_level_for_budget(0) == "low"` (production returns "adaptive")
- Usage log world-readable at `/tmp/` → moved to `~/.cache/` with 0600

### Changed
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
