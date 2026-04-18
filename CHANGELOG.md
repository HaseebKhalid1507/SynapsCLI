# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added
- **Interactive `/settings` menu**: full-screen modal in `chatui` for editing model, thinking, skills, tool limits, and theme — persists to `~/.synaps-cli/config`, applies to running `Runtime` where possible
- **Binary file detection**: Read tool validates UTF-8 and returns clean error for binary files
- **Thread panic handling**: Subagent threads wrapped in `catch_unwind` — panics become visible errors
- **Doc comments**: Public API types (`Runtime`, `ToolRegistry`) and key methods documented
- **Markdown wrapping**: List items and tables now respect terminal width instead of overflowing

### Fixed
- **Zombie processes**: All 3 `child.kill()` sites now followed by `.wait()` to reap
- **Blocking I/O in async**: Session saves, subagent logs, watcher log reads use `tokio::fs`
- **Dead code**: Removed unused `call_api_stream` method
- **Useless assertion**: Removed `usize >= 0` check in MCP

### Refactored
- **Source restructure**: `src/` reorganized into `core/`, `bin/`, `watcher/` modules
- **`core/` module**: config, session, auth, error, logging, protocol, watcher_types grouped as shared infrastructure
- **`watcher/` split**: 1,454-line god object → 4 focused modules (mod, ipc, supervisor, display)
- **`bin/` directory**: 6 binary entry points moved out of src/ root
- **Layering fix**: `apply_config` moved to `Runtime` method — `core/` no longer depends on runtime
- **Dependency cleanup**: Removed redundant `futures-util` crate

### Documentation
- **README overhaul**: Accurate architecture section, real file counts, correct config paths
- **AGENTS.md rewrite**: All tool parameters verified against source code

### Features (prior)
- **18 Built-in Themes**: ocean, rose-pine, nord, dracula, monokai, gruvbox, catppuccin, tokyo-night, sunset, ice, forest, lavender (plus original 6: default, neon-rain, amber, phosphor, solarized-dark, blood)
- **Runtime Theme Loading**: Themes load from `~/.synaps-cli/themes/<name>` files — edit colors without rebuilding
- **`/theme` Command**: Lists all 18 built-in themes plus any custom user themes
- **Variable Subagent Timeouts**: `timeout` parameter on subagent tool (default 300s)
- **Partial Result Recovery**: Timed-out subagents return partial work (tool log + response text) instead of just an error
- **Warning Color**: New `warning_color` theme field for timeout/warning states

### Improvements
- **Theme-Aware UI**: All hardcoded RGB colors in highlight.rs and draw.rs replaced with theme references — boot/quit effects, logo animation, footer, progress bar, bash output highlighting all respect active theme
- **Timeout Visual Feedback**: Timed-out subagents show ⚠ in `warning_color` instead of ✔ in green, both in subagent panel and tool result rendering

### Refactoring
- **chatui Module Split**: Monolithic `chatui.rs` (4200 lines) split into 6 focused modules: main.rs, app.rs, draw.rs, theme.rs, markdown.rs, highlight.rs

### Fixes
- **`/resume` Indentation**: Fixed broken indentation in resume command handler
- **Subagent Fields in `set()`**: Added missing `subagent_*` fields to theme file parser

## [0.2.0] - 2025-01

### Features
- **Skills System**: Markdown skill files that can be injected into system prompt or loaded on-demand via `load_skill` tool
- **Lazy MCP Loading**: MCP servers only spawn when needed via `mcp_connect` gateway tool, preventing token bloat
- **Steering**: Type and send messages while agent is streaming - injected between tool rounds
- **Message Queue**: Queue messages during streaming; auto-fire on completion
- **Subagent Tool**: Parallel execution with real-time TUI panel and animated spinners
- **Abort Context Preservation**: Escape saves partial work, next message gets interrupted context
- **Tool Result Streaming**: Real-time streaming of tool output via ToolResultDelta
- **Smart Scroll**: Viewport stays still when scrolled up; auto-scrolls at bottom

### Performance
- **Release Profile**: LTO, single codegen unit, stripped binary (4.8MB)
- **Zero-copy Tool Schema**: Tool schemas use `Arc<Vec<Value>>` for efficient reads
- **Tool Deduplication**: Prevent schema bloat on registry register()

### Fixes
- **Unicode Safety**: Fixed cursor safety for multiline input and paste operations
- **Paste Size Cap**: Limited paste to 100K characters to prevent memory issues
- **MCP Error Handling**: MCP stderr piped to tracing instead of stdout
- **Tool Registry Race**: Fixed TOCTOU race with snapshot-before-await pattern
- **Subagent Safety**: Disabled recursive subagent spawning, proper cleanup
- **HTTP Timeouts**: Added connect (10s) and request (300s) timeouts

## [0.1.0] - 2024-12

### Features
- **TUI Interface**: Full markdown rendering with syntax highlighting
- **Prompt Caching**: Cache historical messages between tool-loop calls
- **OAuth 2.0 PKCE**: Browser-based authentication with auto-refresh
- **Tool System**: 8 built-in tools: bash, read, write, edit, grep, find, ls, subagent
- **Session Persistence**: Auto-saved sessions with `--continue` support
- **WebSocket Server**: Axum-based server for multiple clients
- **Cost Tracking**: Real-time token usage and pricing display

### UI/UX
- **Boot/Exit Animations**: CRT-style effects via tachyonfx
- **Streaming Tool Indicators**: Real-time tool execution status
- **Theme Support**: User-configurable theme via `~/.synaps-cli/theme`
- **Table Rendering**: Markdown tables with box-drawing borders
- **Input History**: Arrow key navigation through previous messages

### Infrastructure
- **Structured Logging**: File-based tracing across all entrypoints
- **Profile Support**: Isolated namespaces via `--profile <name>`
- **Configuration**: Typed config with model, thinking level, skills
- **Error Handling**: Granular error types for Auth, Config, Session, Tool, Timeout

### Performance
- **3ms Startup**: Optimized binary with efficient initialization
- **Lazy Syntect**: Syntax highlighting loaded on-demand
- **Token Optimization**: Thinking stripping, tool result truncation
- **Cache Rendering**: Optimized TUI redraws and memory usage

## [0.0.1] - 2024-11

### Initial Release
- **Streaming Chat**: Basic agent runtime with tool support
- **Tool Loop**: Async tool execution with error handling
- **File Operations**: Basic file read/write/edit capabilities
- **Configuration**: System prompt from `~/.agent-runtime/system.md`