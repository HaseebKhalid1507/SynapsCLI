# SynapsCLI

A terminal-native AI agent runtime built in Rust. Connect to any LLM, execute tools autonomously, dispatch parallel subagents, plug into the MCP ecosystem, and present everything through a polished TUI — in under 7,300 lines.

<p align="center">
  <img src="https://img.shields.io/badge/language-Rust-orange" alt="Rust" />
  <img src="https://img.shields.io/badge/lines-7.3K-blue" alt="7.3K lines" />
  <img src="https://img.shields.io/badge/tools-8_built--in-green" alt="8 tools" />
  <img src="https://img.shields.io/badge/MCP-supported-purple" alt="MCP" />
  <img src="https://img.shields.io/badge/license-MIT-lightgrey" alt="MIT" />
</p>

---

## Why SynapsCLI?

Most agent runtimes are 100K+ lines of TypeScript. SynapsCLI does the same in **7,300 lines of Rust** — instant startup, single binary, no runtime dependencies. It connects to the MCP ecosystem, runs parallel subagents, streams markdown with syntax highlighting, and stays out of your way.

```
8 built-in tools + unlimited MCP tools
Parallel subagent dispatch with live TUI panel  
Type while the agent streams (steering)
Smart scroll, abort with context preservation
OAuth + API key auth with auto-refresh
~95% prompt cache hit rates
```

## Quick Start

```bash
# Build
git clone https://github.com/HaseebKhalid1507/SynapsCLI.git
cd SynapsCLI
cargo build --release

# Authenticate (OAuth — Claude Pro/Max/Team/Enterprise)
./target/release/synaps-cli login

# Or use an API key
mkdir -p ~/.synaps-cli
echo '{"anthropic":{"type":"api_key","key":"sk-ant-..."}}' > ~/.synaps-cli/auth.json

# Run
./target/release/synaps-cli chatui
```

## Features

### Core Runtime
- **Streaming SSE** with extended thinking display
- **Agentic tool loop** — autonomous multi-step execution with cancellation
- **Parallel tool execution** — multiple tool calls run concurrently via JoinSet
- **Prompt caching** — automatic cache breakpoints for ~95% hit rates
- **Configurable thinking** — `low` / `medium` / `high` / `xhigh` or raw token count
- **Model-aware limits** — auto-detects max_tokens per model

### Tool System

**Trait-based architecture** — tools implement an open `Tool` trait, not a closed enum. Add tools at runtime without touching core code.

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String>;
}
```

**8 built-in tools:** `bash`, `read`, `write`, `edit`, `grep`, `find`, `ls`, `subagent`

**Runtime registration:** `registry.register(Arc::new(MyTool))` — MCP tools, custom tools, anything implementing the trait.

### MCP Integration

Connect to any [Model Context Protocol](https://modelcontextprotocol.io/) server. Tools are auto-discovered and registered at startup.

```json
// ~/.synaps-cli/mcp.json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/home/user"]
    },
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "ghp_..." }
    },
    "memory": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-memory"]
    }
  }
}
```

Config format is compatible with Claude Code and Gemini CLI — drop in your existing `mcp.json`.

On startup:
```
⚡ 13 MCP tools loaded
```

The agent sees MCP tools as `mcp__<server>__<tool>` (e.g. `mcp__github__create_issue`).

### Multi-Agent Orchestration
- **Subagent dispatch** — spawn named agents or inline prompts as one-shot workers
- **Parallel execution** — multiple subagents run concurrently
- **Real-time TUI panel** — live status with animated spinners per agent
- **Agent files** — `~/.synaps-cli/agents/<name>.md` with YAML frontmatter
- **Recursive safety** — subagents can't spawn subagents (no inception)
- **Zombie prevention** — shutdown signals via oneshot channel
- **Token forwarding** — subagent costs tracked with correct per-model pricing

```
╭ ◈ 2 agents ─────────────────────────────────────────╮
│  ⠹ spike   ⚙ bash (tool #3)                  4.2s  │
│  ⠹ chrollo thinking...                        2.1s  │
╰──────────────────────────────────────────────────────╯
```

### TUI
- **Markdown rendering** — headers, code blocks (syntax highlighted via syntect), tables, lists, blockquotes
- **Smart scroll** — viewport stays still when scrolled up; auto-scrolls at bottom
- **Steering** — type and send messages while the agent is streaming (injected between tool rounds)
- **Message queue** — messages queued during no-tool responses auto-fire on completion
- **Abort context** — Escape saves partial work; next message gets context of what was interrupted
- **Tool elapsed time** — every tool result shows execution duration
- **Subagent panel** — animated braille spinners, per-agent status, elapsed timers
- **Token tracking** — input/output/cache tokens with running cost in footer
- **Input history** — arrow keys cycle through previous messages
- **Mouse scroll** — scroll wheel for message history
- **Boot/exit animations** — CRT effects via tachyonfx

### Infrastructure
- **OAuth 2.0 PKCE** — browser-based auth flow, tokens stored with auto-refresh
- **API key fallback** — direct Anthropic API key support
- **Shared config** — `SynapsConfig` struct, single parse path, typed fields
- **Expanded error types** — `Auth`, `Config`, `Session`, `Tool`, `Timeout`, `Cancelled`
- **HTTP timeouts** — connect (10s) + request (300s), no silent hangs
- **Structured tracing** — tool name, elapsed_ms, model, API request lifecycle
- **Server/client** — Axum WebSocket server; multiple clients share a session
- **Session persistence** — auto-saved, `--continue` to resume, partial ID match
- **Profiles** — `--profile <name>` for separate config/auth/session namespaces
- **Prefix commands** — `/q` resolves to `/quit`, unambiguous prefixes work

## Configuration

### Config file
`~/.synaps-cli/config`:
```
model = claude-sonnet-4-20250514
thinking = high
```

Thinking levels: `low` (2048), `medium` (4096), `high` (16384), `xhigh` (32768), or a raw number.

### System prompt
`~/.synaps-cli/system.md` — auto-loaded if present. Override with `--system "prompt"` or `--system /path/to/file.md`.

### MCP servers
`~/.synaps-cli/mcp.json` — see [MCP Integration](#mcp-integration) above.

### Agents
`~/.synaps-cli/agents/<name>.md`:
```markdown
---
name: spike
description: Executor — clean, efficient, no-nonsense
---

You are Spike — a subagent built for execution.
Get the job done. Be concise.
```

### Theme
`~/.synaps-cli/theme`:
```
bg = #0c0e12
border = #232832
claude_label = #50c8a0
```

## Usage

```bash
# Interactive TUI
synaps-cli chatui

# One-shot
synaps-cli run "explain quicksort"

# With agent personality
synaps-cli run "review this PR" --agent spike

# Resume last session
synaps-cli chatui --continue

# Resume specific session (partial ID match)
synaps-cli chatui --continue abc123

# Custom system prompt
synaps-cli chatui --system "You are a Rust expert"

# Server mode
synaps-cli server --port 3145

# With profile
synaps-cli chatui --profile work
```

## Architecture

```
src/
├── main.rs       CLI entry point (clap)
├── runtime.rs    Anthropic API, SSE streaming, agentic tool loop,
│                 parallel execution, prompt cache management
├── chatui.rs     TUI — ratatui, markdown, subagent panel, smart scroll,
│                 abort context, steering, animations
├── tools.rs      Tool trait + 8 implementations (bash, read, write,
│                 edit, grep, find, ls, subagent)
├── mcp.rs        MCP client — stdio transport, JSON-RPC 2.0,
│                 auto-discovery, dynamic tool registration
├── auth.rs       OAuth 2.0 PKCE, token refresh with flock
├── session.rs    Session persistence (JSON)
├── config.rs     SynapsConfig, profile system, path resolution,
│                 system prompt resolution
├── server.rs     Axum WebSocket server
├── client.rs     WebSocket CLI client
├── protocol.rs   Shared message types (client ↔ server)
├── chat.rs       Simple REPL (non-TUI)
├── login.rs      OAuth browser flow
├── logging.rs    Tracing setup
├── error.rs      RuntimeError (Auth, Config, Session, Tool, Timeout, Cancelled)
└── lib.rs        Library root, re-exports
```

### Key Design Decisions

**Tool trait over enum.** Tools implement an open `trait Tool`, not a closed `enum ToolType`. This enables MCP tools, custom tools, and future extensions without modifying core code. Tools are stored as `Arc<dyn Tool>` in the registry — Clone-friendly, Send+Sync, runtime-registerable.

**MCP via stdio.** MCP servers are spawned as child processes. Communication is JSON-RPC 2.0 over stdin/stdout. Each server connection is `Arc<Mutex<McpConnection>>`, shared across all tools from that server. Config format matches Claude Code / Gemini CLI.

**Parallel tool execution.** Multiple tool calls in one response run concurrently via `tokio::task::JoinSet`. Single tool calls run inline (zero overhead). Results reassembled in original order.

**Subagent isolation.** Each subagent runs on a dedicated `std::thread` with its own `current_thread` tokio runtime. Solves recursive async `Send` bounds. Parent communicates via `oneshot` channels for results and shutdown.

**Prompt caching.** Historical messages are never modified. Cache breakpoints annotated on the last user message every 4+ turns. System prompt and tool schemas get `cache_control: ephemeral`. Achieves ~95% cache hit rates.

**Steering.** Messages typed during streaming inject between tool execution rounds via unbounded channel. Drain happens after every tool batch and before every API call. Fallback queue fires on response completion. Mirrors pi's `steer` / `followUp` system.

**Smart scroll.** Viewport tracking via `scroll_pinned` + `last_line_count` delta compensation. Scrolled-up viewport stays stationary during streaming content growth.

## Cost Tracking

| Model | Input | Output | Cache Read | Cache Write |
|-------|-------|--------|------------|-------------|
| Opus | $15.00/MTok | $75.00/MTok | $1.50/MTok | $18.75/MTok |
| Sonnet | $3.00/MTok | $15.00/MTok | $0.30/MTok | $3.75/MTok |
| Haiku | $0.80/MTok | $4.00/MTok | $0.08/MTok | $1.00/MTok |

Subagent costs tracked with the subagent's model. Running total in TUI footer, persisted with session.

## Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime |
| `reqwest` | HTTP client (streaming, timeouts) |
| `serde` / `serde_json` | Serialization |
| `clap` | CLI argument parsing |
| `axum` | HTTP/WebSocket server |
| `ratatui` | Terminal UI framework |
| `crossterm` | Terminal backend + input |
| `tachyonfx` | Terminal animations |
| `syntect` | Syntax highlighting |
| `async-trait` | Async trait support |
| `chrono` | Timestamps |
| `uuid` | Session IDs |
| `tokio-util` | CancellationToken |
| `tracing` | Structured logging |
| `fs4` | Cross-process file locking |
| `sha2` | PKCE OAuth |

## License

MIT
