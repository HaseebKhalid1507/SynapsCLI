# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased] - 2025-01-XX

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