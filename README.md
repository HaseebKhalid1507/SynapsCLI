# SynapsCLI

A terminal-native AI agent runtime built in Rust. Streams responses with extended thinking, executes tools autonomously, dispatches parallel subagents with real-time monitoring, and presents everything through a polished TUI.

## Features

### Core Runtime
- **Streaming SSE** — Real-time token streaming with thinking block display
- **Agentic tool loop** — Autonomous multi-step tool execution with cancellation
- **8 built-in tools** — bash, read, write, edit, grep, find, ls, subagent
- **Extended thinking** — Configurable thinking budgets (low/medium/high/xhigh or custom)
- **Prompt caching** — Automatic cache breakpoint annotation for ~95% cache hit rates

### Multi-Agent Orchestration
- **Subagent dispatch** — Spawn named agents or inline system prompts as one-shot workers
- **Parallel execution** — Multiple tool calls in one response run concurrently via JoinSet
- **Real-time TUI panel** — Live status showing each agent's activity with animated spinners
- **Agent resolution** — `~/.synaps-cli/agents/<name>.md` with YAML frontmatter stripping
- **Recursive safety** — Subagents get full tools except the subagent tool (no inception)
- **Zombie prevention** — Shutdown signal via oneshot channel; parent abort cancels child streams
- **Token forwarding** — Subagent costs tracked in parent session with correct per-model pricing
- **Subagent logging** — Every dispatch logged to `~/.synaps-cli/logs/subagents/<timestamp>-<agent>.md`

### TUI
- **Markdown rendering** — Headers, code blocks (syntax highlighted), tables, lists, blockquotes
- **Smart scroll** — Viewport stays stationary when scrolled up during streaming; auto-scrolls at bottom
- **Abort context** — Escape saves partial work; next message gets context of what was interrupted
- **Tool elapsed time** — Every tool result shows execution duration
- **Subagent panel** — Animated braille spinner, per-agent status, elapsed timers, running/done counts
- **Boot/exit animations** — CRT-style effects via tachyonfx
- **Themes** — Customizable via `~/.synaps-cli/theme` key-value file
- **Mouse support** — Scroll wheel for message history
- **Input history** — Arrow keys cycle through previous messages
- **Token tracking** — Input/output/cache-read/cache-write tokens with cost in footer

### Infrastructure
- **OAuth login** — PKCE flow via browser; tokens stored locally with auto-refresh
- **API key fallback** — Direct Anthropic API key support
- **Server/client architecture** — Axum WebSocket server; multiple clients share a session
- **Session persistence** — Auto-saved sessions with `--continue` to resume
- **Model-aware limits** — Opus 128K / Sonnet 64K max_tokens automatically
- **Prefix commands** — `/q` instead of `/quit`, unambiguous prefixes resolve
- **Tab completion** — Tab-complete slash commands
- **Profiles** — `--profile <name>` for separate config/auth/session namespaces

## Quick Start

### Prerequisites

- Rust toolchain (1.70+)
- Either a Claude Pro/Max account **or** an Anthropic API key

### Setup

```bash
# Clone and build
git clone https://github.com/HaseebKhalid1507/SynapsCLI.git
cd SynapsCLI
cargo build --release

# The binary is at target/release/synaps-cli
```

### Authentication

**OAuth (recommended — Claude Pro/Max/Team/Enterprise):**
```bash
target/release/synaps-cli login
# Opens browser → Anthropic OAuth → tokens saved to ~/.synaps-cli/auth.json
```

**API key:**
```bash
mkdir -p ~/.synaps-cli
echo '{"anthropic":{"type":"api_key","key":"sk-ant-..."}}' > ~/.synaps-cli/auth.json
```

### Run

```bash
# Interactive TUI
target/release/synaps-cli chatui

# One-shot command
target/release/synaps-cli run "explain quicksort"

# One-shot with agent personality
target/release/synaps-cli run "review this PR" --agent spike

# Resume a session
target/release/synaps-cli chatui --continue

# Server mode (WebSocket)
target/release/synaps-cli server --port 3145
```

## Agents

Agents are markdown files with optional YAML frontmatter at `~/.synaps-cli/agents/`:

```markdown
---
name: spike
description: Executor — clean, efficient, no-nonsense
model: claude-sonnet-4-20250514
---

You are **Spike** — a subagent channeling Spike Spiegel.
Your specialty: **execution**. You get the job done.
```

### CLI dispatch
```bash
synaps-cli run "fix the nginx config" --agent spike
synaps-cli run "audit this code" --agent shady
synaps-cli run "analyze architecture" -a zero
```

### In-conversation dispatch
The model can call the `subagent` tool during a conversation:
```
dispatch spike with task "count lines in src/"
send 3 agents in parallel to review each module
```

The TUI shows real-time status:
```
╭ ◈ 2 agents ─────────────────────────────────────────╮
│  ⠹ spike   ⚙ bash (tool #3)                  4.2s  │
│  ⠹ chrollo thinking...                        2.1s  │
╰──────────────────────────────────────────────────────╯
```

## Architecture

```
src/
├── main.rs          Entry point, CLI args (clap), --agent/--system flags
├── runtime.rs       Anthropic API client, SSE streaming, agentic tool loop,
│                    parallel execution (JoinSet), prompt cache management
├── chatui.rs        TUI (ratatui), subagent panel, smart scroll, abort context,
│                    markdown rendering, syntax highlighting, animations
├── tools.rs         Tool system: bash, read, write, edit, grep, find, ls, subagent
│                    Agent resolution, subagent streaming, zombie prevention
├── auth.rs          OAuth 2.0 PKCE flow, token refresh with flock
├── session.rs       Session persistence (JSON)
├── config.rs        Profile system, path resolution
├── server.rs        Axum WebSocket server, broadcast stream events
├── client.rs        CLI WebSocket client, ANSI streaming output
├── protocol.rs      Shared message types (client ↔ server)
├── chat.rs          Simple REPL chat (non-TUI)
├── login.rs         OAuth browser flow
├── logging.rs       Tracing setup
├── error.rs         Error types
└── lib.rs           Library root, re-exports
```

### Key Design Decisions

**Parallel tool execution:** When the model returns multiple tool calls in one response, they execute concurrently via `tokio::task::JoinSet`. Single tool calls run inline (zero overhead). Results are reassembled in original order.

**Subagent isolation:** Each subagent runs on a dedicated `std::thread` with its own `current_thread` tokio runtime. This solves the recursive async `Send` bound issue (`run_stream` → `tool.execute` → `run_stream`). Parent communicates via `oneshot` channels for results and shutdown signals.

**Prompt caching:** Historical messages are never modified. Cache breakpoints are annotated on the last user message every 4+ turns. System prompt and tool schemas get `cache_control: ephemeral`. This achieves ~95% cache hit rates on long conversations.

**Smart scroll:** Viewport tracking via `scroll_pinned` flag + `last_line_count` delta compensation. When user scrolls up, `scroll_back` increases by content growth each frame so the viewport stays stationary.

**Abort context:** On Escape, all partial output (thinking, text, tool calls, results) since the last user message is captured and injected into the next API call. The model sees what it was doing before the interrupt. Cache-safe — only the new user message is affected.

## Configuration

### Config file

`~/.synaps-cli/config` (or `~/.synaps-cli/<profile>/config`):
```
model = claude-opus-4-6
thinking = high
```

Thinking levels: `low` (1024), `medium` (4096), `high` (8192), `xhigh` (16384), or a raw number.

### System prompt

`~/.synaps-cli/system.md` — loaded automatically if present.

### Theme

`~/.synaps-cli/theme` — key-value pairs:
```
bg = #0c0e12
border = #232832
claude_label = #50c8a0
subagent_name = #b48cdc
```

## Cost Tracking

| Model | Input (per MTok) | Output (per MTok) |
|-------|-----------------|-------------------|
| Opus | $15.00 | $75.00 |
| Sonnet | $3.00 | $15.00 |
| Haiku | $0.80 | $4.00 |

Cache reads bill at 0.1× input price; cache writes at 1.25×. Subagent costs are tracked with the subagent's model (not the parent's), so pricing is always accurate. Running total shown in the TUI footer and persisted with each session.

## Dependencies

| Crate | Purpose |
|-------|---------|
| tokio | Async runtime |
| reqwest | HTTP client with streaming |
| serde / serde_json | Serialization |
| clap | CLI argument parsing |
| axum | HTTP/WebSocket server + OAuth callback |
| tokio-tungstenite | WebSocket client |
| ratatui | Terminal UI framework |
| crossterm | Terminal backend + input events |
| tachyonfx | Terminal animations |
| syntect | Syntax highlighting |
| chrono | Timestamps |
| uuid | Session ID generation |
| tokio-util | CancellationToken for streaming abort |
| futures | Stream combinators |
| fs4 | Cross-process file locking for auth refresh |
| sha2 / rand / base64 | PKCE OAuth challenge |

## License

MIT
