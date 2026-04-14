# SynapsCLI

![Rust 1.80+](https://img.shields.io/badge/rust-1.80%2B-orange.svg)
![MIT License](https://img.shields.io/badge/license-MIT-blue.svg)
![~14K lines](https://img.shields.io/badge/lines-~14.4K-green.svg)

Terminal-native AI agent runtime built in Rust. Chat, orchestrate, deploy autonomous agents — one codebase.

## Quick Start

```bash
git clone https://github.com/HaseebKhalid1507/SynapsCLI.git
cd SynapsCLI
cargo build --release
```

**Authenticate:**
```bash
# OAuth (Claude Pro/Max/Team)
./target/release/login

# OR API key
export ANTHROPIC_API_KEY="sk-ant-..."
```

**Launch:**
```bash
./target/release/chatui
```

Type `/help` for available commands. Type `/theme` to browse 18 themes.

---

## Three Modes

### 💬 Interactive Chat (`chatui`)

Full TUI with streaming, syntax highlighting, markdown rendering, and a live subagent panel.

```bash
chatui --theme cyberpunk --model claude-sonnet-4-20250514
```

- **10 built-in tools:** bash, read, write, edit, grep, find, ls, subagent, mcp_connect, load_skill
- **Mid-stream steering:** Type while Claude is responding to redirect
- **Session continuation:** `/continue` to resume previous conversations
- **18 themes:** From minimal to cyberpunk
- **Parallel subagents:** Dispatch named agents and watch them work in real-time

```
╭ ◈ 4 agents ────────────────────────────────────╮
│  ✓ spike    done                         12.3s  │
│  ⠹ chrollo  ⚙ read (tool #5)             8.1s  │
│  ✓ shady    done                          9.7s  │
│  ⠹ zero     thinking...                   4.2s  │
╰─────────────────────────────────────────────────╯
```

### 📡 Headless Chat (`chat`)

Stdin/stdout interface for scripting and piping:

```bash
echo "Explain this error" | cat error.log - | ./target/release/chat
```

### 🤖 Autonomous Agents (`watcher`)

Supervised agents that run 24/7 with heartbeat monitoring, crash recovery, cost limits, and session handoff.

```bash
watcher init scout              # Create agent from template
watcher deploy scout            # Start supervised execution
watcher status                  # Monitor fleet
watcher logs scout -f           # Follow logs
watcher stop scout              # Graceful shutdown
```

**Agent configuration** (`~/.synaps-cli/watcher/scout/config.toml`):
```toml
[agent]
name = "scout"
model = "claude-sonnet-4-20250514"
trigger = "manual"              # manual | always | watch

[trigger]
paths = ["./src"]               # watch mode: directories to monitor
patterns = ["*.rs"]             # watch mode: file filters
debounce_secs = 3               # watch mode: settle time

[limits]
max_session_tokens = 100000
max_session_duration_mins = 60
max_session_cost_usd = 0.50
max_daily_cost_usd = 10.0
max_tool_calls = 200
cooldown_secs = 10
max_retries = 3

[heartbeat]
interval_secs = 30
stale_threshold_secs = 120
```

Agents write handoff state on exit so the next session picks up exactly where they left off.

---

## Tool System

10 built-in tools available to all agents:

| Tool | Purpose |
|------|---------|
| `bash` | Execute shell commands (30s default, 300s max timeout) |
| `read` | Read files with line numbers (validates UTF-8, rejects binary) |
| `write` | Create/overwrite files (atomic write-then-rename) |
| `edit` | Surgical string replacement (exact match, atomic) |
| `grep` | Regex search with context lines |
| `find` | Glob-based file discovery |
| `ls` | Directory listing with metadata |
| `subagent` | Dispatch a specialist agent with its own runtime |
| `mcp_connect` | Connect to MCP servers and load external tools |
| `load_skill` | Load behavioral guidelines from markdown files |

See [AGENTS.md](AGENTS.md) for full tool reference with parameters and behavior.

---

## MCP Integration

Connect to the [Model Context Protocol](https://modelcontextprotocol.io/) ecosystem. Servers spawn lazily on first tool use.

**Configuration** (`~/.synaps-cli/mcp.json`):
```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/files"],
      "env": {}
    },
    "brave-search": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-brave-search"],
      "env": { "BRAVE_API_KEY": "your-key" }
    }
  }
}
```

Tools from MCP servers are prefixed `mcp__{server}__{tool}` and available for the rest of the session.

---

## Configuration

```
~/.synaps-cli/
├── config                # Global settings (model, thinking budget, theme)
├── auth.json             # OAuth tokens (file-locked, permissions 600)
├── system.md             # Default system prompt
├── mcp.json              # MCP server definitions
├── agents/               # Subagent definitions (.md files)
│   └── spike.md
├── skills/               # Behavioral guidelines (.md with frontmatter)
│   └── code-review.md
├── sessions/             # Conversation history (JSON)
├── logs/
│   └── subagents/        # Per-subagent session logs
└── watcher/              # Autonomous agents
    └── scout/
        ├── config.toml   # Agent configuration
        ├── soul.md       # Agent system prompt
        ├── handoff.json  # State from last session
        ├── stats.json    # Cumulative usage stats
        ├── heartbeat     # Timestamp file
        └── logs/         # Per-session JSONL logs
```

Project-local `.synaps-cli/skills/` override global skills.

---

## Architecture

7 binaries from 39 source files (~14,400 lines):

```
src/
├── main.rs                  # chatui entry
├── lib.rs                   # Core types & re-exports
├── chat.rs                  # Headless chat binary
├── agent.rs                 # synaps-agent (headless worker for watcher)
├── watcher.rs               # Watcher supervisor daemon (1,454 lines)
├── watcher_types.rs         # Watcher config & types
├── login.rs                 # OAuth login flow
├── server.rs                # WebSocket server binary
├── client.rs                # WebSocket client binary
├── config.rs                # Configuration & path resolution
├── session.rs               # Session persistence
├── auth.rs                  # OAuth + API key authentication
├── error.rs                 # Error types (thiserror)
├── mcp.rs                   # MCP JSON-RPC client
├── skills.rs                # Skills system
├── logging.rs               # Tracing setup
├── protocol.rs              # Protocol types
├── runtime/                 # Core runtime (6 files)
│   ├── mod.rs               # Runtime struct, orchestration loop
│   ├── api.rs               # API communication, SSE streaming, retry
│   ├── stream.rs            # Stream processing, parallel tool execution
│   ├── auth.rs              # Auth state management
│   ├── helpers.rs           # Shared utilities
│   └── types.rs             # Internal types
├── tools/                   # Tool implementations (10 files)
│   ├── mod.rs               # Tool trait, ToolRegistry, ToolContext
│   ├── bash.rs              # Shell execution with timeout
│   ├── read.rs              # File reading (UTF-8 validated)
│   ├── write.rs             # Atomic file writes
│   ├── edit.rs              # Surgical string replacement
│   ├── grep.rs              # Regex search
│   ├── find.rs              # Glob file discovery
│   ├── ls.rs                # Directory listing
│   ├── subagent.rs          # Agent dispatch with panic handling
│   └── watcher_exit.rs      # Handoff for watcher agents
└── chatui/                  # TUI frontend (5 files)
    ├── main.rs              # Event loop
    ├── app.rs               # Application state
    ├── draw.rs              # Rendering
    ├── markdown.rs          # Markdown renderer (tables, lists, wrapping)
    ├── highlight.rs         # Syntax highlighting (syntect)
    └── theme.rs             # 18 color themes
```

**Binaries:**

| Binary | Purpose |
|--------|---------|
| `chatui` | Interactive TUI with streaming and subagent panel |
| `chat` | Headless chat for scripting |
| `synaps-agent` | Lightweight worker runtime (used by watcher) |
| `watcher` | Supervisor daemon for autonomous agents |
| `login` | OAuth authentication flow |
| `server` | WebSocket server mode |
| `client` | WebSocket client |

---

## License

MIT License. See [LICENSE](LICENSE).

---

**Author:** [Haseeb Khalid](https://github.com/HaseebKhalid1507)
