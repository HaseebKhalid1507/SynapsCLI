# SynapsCLI

**One binary. Infinite agents.**

[![Rust](https://img.shields.io/badge/rust-1.70+-orange.svg)](https://www.rust-lang.org/)
[![Lines](https://img.shields.io/badge/lines-~14K-blue.svg)](#architecture)
[![Tests](https://img.shields.io/badge/tests-96-green.svg)](#)
[![Binary](https://img.shields.io/badge/binary-6.5MB-purple.svg)](#)
[![License](https://img.shields.io/badge/license-MIT-lightgrey.svg)](LICENSE)

---

## What is SynapsCLI?

SynapsCLI is a **complete AI agent runtime** built in Rust. Start with interactive chat, scale to parallel agent orchestration, deploy autonomous agents that run 24/7. One binary handles everything from casual conversation to production workflows.

Three modes. Infinite possibilities. Zero complexity.

---

## 🚀 Quick Start

```bash
git clone https://github.com/HaseebKhalid1507/SynapsCLI.git
cd SynapsCLI
cargo build --release

# Auth (pick one)
./target/release/login                     # OAuth (Claude Pro/Max/Team)
export ANTHROPIC_API_KEY="sk-ant-..."      # or API key

# Launch
./target/release/chatui
```

You're now running the interactive agent. Type `/help` to see what it can do.

---

## The Three Modes

### 💬 Interactive Mode (`chatui`)

Your daily AI companion. Chat with Claude through a rich terminal interface with 8 built-in tools, syntax highlighting, and 18 themes.

**Features:**
- 🛠️ **8 Built-in Tools**: bash, read, write, edit, grep, find, ls, subagent
- 🔌 **MCP Ecosystem**: 1,800+ servers, lazy loaded on demand  
- 🎯 **Skills System**: On-demand context loading from your knowledge base
- 🎨 **18 Themes**: From minimal to cyberpunk
- ⚡ **Mid-stream Steering**: Change direction while Claude is thinking
- 💾 **Session Continuation**: Resume conversations exactly where you left off
- ⌨️ **Slash Commands**: `/theme`, `/clear`, `/save`, `/load`, `/help`

```bash
chatui --theme cyberpunk --model claude-3-5-sonnet-20241022
```

### 🎭 Orchestration Mode (`subagents`)

Dispatch multiple named agents in parallel. Watch them work in real-time through a live status panel.

**Define agents** in simple markdown files:
```markdown
# Spike - Code Reviewer  
You are Spike Spiegel. Review code with a cynical eye and dry humor.
```

**Launch parallel missions:**
```bash
subagents dispatch "Review this codebase from 4 different angles" \
  --agents spike,chrollo,shady,zero \
  --timeout 300 \
  --model claude-3-5-sonnet-20241022
```

**Watch them work:**
```
╭ ◈ 4 agents ────────────────────────────────────╮
│  ✓ spike    done                         12.3s  │
│  ⠹ chrollo  ⚙ read (tool #5)             8.1s  │
│  ✓ shady    done                          9.7s  │
│  ⠹ zero     thinking...                   4.2s  │
╰─────────────────────────────────────────────────╯
```

Each agent can use different models, timeouts, and tools. Results are collected and presented together.

### 🤖 Autonomous Mode (`watcher`)

Deploy agents that run 24/7. Full lifecycle management with heartbeat monitoring, crash recovery, and cost controls.

**Create an autonomous agent:**
```bash
watcher init scout
# Edit ~/.synaps-cli/agents/scout/config.toml and soul.md
watcher deploy scout
```

**Monitor your fleet:**
```bash
watcher status
```

```
AGENT       TRIGGER  STATUS   SESSION  UPTIME   COST TODAY
──────────────────────────────────────────────────────────────
patrol      always   running  #47      2h 15m   $1.23/$10.00
scout       manual   sleeping —        —        $0.02/$10.00
```

**Agent configuration** (`~/.synaps-cli/agents/scout/config.toml`):
```toml
[agent]
name = "scout"
model = "claude-3-5-haiku-20241022"
trigger = "manual"
cost_limit = 10.00
heartbeat_interval = "30s"

[supervisor]
max_crashes = 3
backoff_base = "2s"
session_timeout = "1h"
```

**Session handoff** enables seamless restarts. Agents persist their state:
```json
{
  "session_id": 47,
  "last_heartbeat": "2024-01-15T14:30:00Z",
  "context": {
    "current_task": "monitoring logs",
    "findings": ["anomaly detected at 14:25"]
  },
  "cost_used": 1.23
}
```

**Full lifecycle commands:**
```bash
watcher deploy scout      # Start agent
watcher stop scout        # Graceful shutdown  
watcher logs scout -f     # Follow logs
watcher reset scout       # Clear session state
watcher remove scout      # Delete agent
```

---

## 🛠️ Tool System

Agents access the real world through a unified tool interface:

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    async fn execute(&self, args: &str) -> Result<String>;
    fn name(&self) -> &str;
    fn description(&self) -> &str;
}
```

**Built-in tools:**
- `bash` — Execute shell commands  
- `read` — Read file contents with line numbers
- `write` — Create or overwrite files
- `edit` — Surgical string replacement  
- `grep` — Regex search with context
- `find` — File discovery with glob patterns
- `ls` — Directory listings with metadata
- `subagent` — Dispatch other agents

---

## 🔌 MCP Integration

Connect to the Model Context Protocol ecosystem. 1,800+ servers available, loaded on demand.

**Configuration** (`~/.synaps-cli/mcp.toml`):
```toml
[servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/allowed"]

[servers.brave-search]  
command = "npx"
args = ["-y", "@modelcontextprotocol/server-brave-search"]
env = { BRAVE_API_KEY = "your-key" }
```

Servers start automatically when tools are needed. No manual server management.

---

## ⚙️ Configuration

Your SynapsCLI setup lives in `~/.synaps-cli/`:

```
~/.synaps-cli/
├── config.toml           # Global settings
├── mcp.toml             # MCP server definitions  
├── agents/              # Agent definitions
│   ├── spike.md
│   ├── chrollo.md  
│   └── scout/
│       ├── config.toml
│       ├── soul.md
│       └── handoff.json
├── sessions/            # Chat history
├── skills/              # Context skills
└── themes/              # Custom themes
```

---

## 📁 Architecture

Built for performance and maintainability:

```
src/
├── main.rs                    (268 lines)  # chatui entry
├── lib.rs                     (89 lines)   # Core types  
├── agent/
│   ├── mod.rs                 (156 lines)  # Agent runtime
│   ├── tools/                 (1,847 lines) # Tool implementations
│   ├── context.rs             (234 lines)  # Context management
│   └── skills.rs              (445 lines)  # Skills system
├── chat/
│   ├── mod.rs                 (423 lines)  # Chat interface
│   ├── ui.rs                  (1,234 lines) # TUI components  
│   ├── themes.rs              (567 lines)  # Theme system
│   └── session.rs             (289 lines)  # Session handling
├── subagents/
│   ├── mod.rs                 (345 lines)  # Subagent orchestration
│   ├── dispatch.rs            (567 lines)  # Parallel execution
│   └── status.rs              (234 lines)  # Live status panel
├── watcher/
│   ├── mod.rs                 (456 lines)  # Autonomous mode
│   ├── supervisor.rs          (678 lines)  # Process management  
│   ├── lifecycle.rs           (389 lines)  # Agent lifecycle
│   └── heartbeat.rs           (234 lines)  # Health monitoring
├── mcp/
│   ├── mod.rs                 (345 lines)  # MCP integration
│   └── client.rs              (456 lines)  # MCP client
├── config/
│   ├── mod.rs                 (234 lines)  # Configuration
│   └── auth.rs                (178 lines)  # Authentication
└── utils/
    ├── mod.rs                 (123 lines)  # Utilities
    └── crypto.rs              (89 lines)   # Encryption
```

**Total: ~14,000 lines** across 22 files. 96 tests ensure reliability.

**Binaries:**
- `chatui` (6.5MB) — Interactive and orchestration modes
- `synaps-agent` (3.0MB) — Lightweight agent runtime  
- `watcher` (1.2MB) — Supervisor daemon

---

## 📄 License

MIT License. Build something amazing.