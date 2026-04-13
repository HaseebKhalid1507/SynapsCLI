# SynapsCLI

A terminal-native AI agent runtime built in Rust. One binary, zero runtime dependencies.

<p align="center">
  <img src="https://img.shields.io/badge/rust-~9K_lines-orange" alt="Rust" />
  <img src="https://img.shields.io/badge/binary-6.5MB-blue" alt="6.5MB" />
  <img src="https://img.shields.io/badge/tests-90-brightgreen" alt="90 tests" />
  <img src="https://img.shields.io/badge/MCP-lazy_loading-purple" alt="MCP" />
  <img src="https://img.shields.io/badge/license-MIT-lightgrey" alt="MIT" />
</p>

```
 ╭──────────────────────────────────────────────────────────────────╮
 │  8 built-in tools + entire MCP ecosystem on demand              │
 │  Parallel subagent dispatch with live TUI panel                 │
 │  Skills loaded on demand — zero tokens until needed             │
 │  Type while the agent streams (steering + message queue)        │
 │  Syntax-highlighted diffs, code blocks, and tool output         │
 │  6 built-in themes · context usage bar · cost tracking          │
 │  OAuth + API key auth with auto-refresh mid-stream              │
 │  ~95% prompt cache hit rates                                    │
 ╰──────────────────────────────────────────────────────────────────╯
```

---

## Quick Start

```bash
git clone https://github.com/HaseebKhalid1507/SynapsCLI.git
cd SynapsCLI
cargo build --release

# Auth (pick one)
./target/release/login                     # OAuth (Claude Pro/Max/Team)
export ANTHROPIC_API_KEY="sk-ant-..."      # or API key

# Launch
./target/release/chatui
./target/release/chatui --continue         # resume last session
```

## Why SynapsCLI?

Most AI coding agents are 100K–450K lines of TypeScript/Go. SynapsCLI does the same in **~9,000 lines of Rust**.

| | SynapsCLI | Claude Code | Gemini CLI |
|---|---|---|---|
| **Language** | Rust | TypeScript | Go |
| **Lines** | ~9K | ~100K | ~45K |
| **Binary** | 6.5MB | ~370MB (node) | ~50MB |
| **MCP loading** | Lazy (on demand) | Eager (all at boot) | Eager |
| **Subagents** | Parallel with live panel | Sequential | — |
| **Skills** | On-demand tool | — | — |
| **Steering** | Type mid-stream | — | — |

---

## Features

### Tool System

Open trait — add tools at runtime without recompilation.

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String>;
}
```

**Built-in:** `bash` · `read` · `write` · `edit` · `grep` · `find` · `ls` · `subagent`

**Gateway tools:** `mcp_connect` (lazy MCP activation) · `load_skill` (on-demand skills)

Register custom tools at runtime: `registry.register(Arc::new(my_tool))`

### MCP — Lazy Loading

Connect to 1,800+ servers from the [MCP registry](https://registry.modelcontextprotocol.io/). Tools load **on demand** — zero token overhead until needed.

```json
// ~/.synaps-cli/mcp.json
{
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "ghp_..." }
    },
    "deepwiki": {
      "command": "npx",
      "args": ["-y", "mcp-remote", "https://mcp.deepwiki.com/mcp"]
    }
  }
}
```

**How it works:** At startup, SynapsCLI registers a single `mcp_connect` gateway tool (~200 tokens). When the agent needs a server, it calls `mcp_connect("github")` — the server spawns, tools are discovered via JSON-RPC, and they're registered into the live session. Cached from that point forward.

**Token savings vs eager loading:** 65 tools × 9,500 tokens/msg → 10 tools × 1,200 tokens/msg.

Compatible with Claude Code and Gemini CLI config format.

### Skills

Markdown files that load into context when the agent needs them. Zero cost until activated.

```
~/.synaps-cli/skills/
├── rust.md                          # Rust patterns & idioms
├── code-review.md                   # Multi-axis review checklist
├── test-driven-development.md       # TDD methodology
└── security-review.md               # Security audit guidelines
```

```markdown
---
name: rust
description: Rust development best practices
---

When writing Rust code:
- Prefer `thiserror` for library errors...
```

Agent calls `load_skill("rust")` → content returned as tool result → cached for the rest of the conversation. Or auto-load via config: `skills = rust, security-review`.

### Subagents

Parallel dispatch with a live TUI status panel.

```
╭ ◈ 2 agents ─────────────────────────────────────────╮
│  ⠹ spike   ⚙ bash (tool #3)                  4.2s  │
│  ⠹ chrollo thinking...                        2.1s  │
╰──────────────────────────────────────────────────────╯
```

- Named agents from `~/.synaps-cli/agents/<name>.md` or inline prompts
- Concurrent execution via `JoinSet`
- Per-model cost tracking and token forwarding
- Recursive safety — subagents can't spawn subagents
- Zombie prevention via shutdown oneshot channels

### TUI

- **Syntax highlighting** — code blocks, diffs (red/green), bash output, grep results
- **6 themes** — default, neon-rain, amber, phosphor, solarized, blood
- **Animated execution** — braille thinking spinner, bash trace animation, streaming pulse
- **Smart scroll** — viewport stays still when scrolled up, auto-follows at bottom
- **Steering** — type while the agent streams; messages inject between tool rounds
- **Message queue** — queued messages auto-fire on completion
- **Abort with context** — Esc saves partial work; next message includes interrupted context
- **Diff display** — edit tool shows old (red −) / new (green +) with line numbers
- **Context usage bar** — visual context window burn rate in the footer
- **Boot/exit effects** — CRT sweep-in, fade animations

### Slash Commands

| Command | Description |
|---------|-------------|
| `/clear` | Reset conversation |
| `/model [name]` | Show or change model |
| `/theme` | List available themes |
| `/thinking [level]` | Set thinking budget |
| `/sessions` | List saved sessions |
| `/resume [id]` | Resume a session |
| `/system [prompt]` | View or change system prompt |
| `/help` | Show all commands |
| `/quit` | Exit |

All commands support prefix matching — `/m` resolves to `/model`, `/th` to `/thinking`.

---

## Configuration

```
~/.synaps-cli/
├── config                # model, thinking level, auto-loaded skills
├── system.md             # system prompt (auto-loaded if present)
├── auth.json             # OAuth tokens or API key
├── mcp.json              # MCP server definitions
├── theme                 # custom color overrides
├── agents/               # subagent personality files
│   └── <name>.md
├── skills/               # loadable skill files
│   └── <name>.md
└── sessions/             # auto-saved conversation history
    └── <id>.json
```

**Config file** (`~/.synaps-cli/config`):
```ini
model = claude-sonnet-4-20250514
thinking = high          # low | medium | high | xhigh | <number>
skills = rust, security  # auto-load these skills on startup
```

**Profiles** — separate namespaces for different contexts:
```bash
chatui --profile work    # uses ~/.synaps-cli/work/config, auth.json, etc.
```

---

## Architecture

```
src/
├── chatui.rs    (3.5K)  TUI — ratatui, markdown, themes, animations
├── runtime.rs   (1.3K)  API client, streaming, agentic tool loop, steering
├── tools.rs     (1.2K)  Tool trait + 8 implementations + subagent dispatch
├── auth.rs      (750)   OAuth 2.0 PKCE, file-locked token refresh
├── server.rs    (560)   Axum WebSocket server
├── mcp.rs       (520)   MCP client — JSON-RPC, lazy loading, gateway tool
├── client.rs    (280)   WebSocket CLI client
├── skills.rs    (280)   Skill loading, on-demand tool, formatting
├── session.rs   (330)   Session persistence + listing
├── config.rs    (220)   Config parsing, profiles, prompt resolution
├── protocol.rs  (340)   Shared message types (serde-tagged enums)
├── main.rs      (105)   CLI entry point
├── login.rs     (80)    OAuth browser flow
├── error.rs     (90)    Error types + tests
├── logging.rs   (27)    Tracing setup
├── chat.rs      (133)   Simple REPL (non-TUI)
└── lib.rs       (19)    Module exports
                ─────
                ~9,000 lines  ·  90 tests  ·  17 files
```

### Key Design Decisions

**Trait-based tools.** `Arc<dyn Tool>` with runtime registration. MCP, skills, and custom tools plug in without touching core code.

**Lazy everything.** MCP servers don't connect until called. Skills don't load until needed. Tool schemas use `Arc<Vec<Value>>` for zero-copy reads.

**Shared mutable registry.** `Arc<RwLock<ToolRegistry>>` lets `mcp_connect` register tools mid-conversation. Snapshot-before-await pattern prevents deadlocks.

**Subagent isolation.** Each subagent gets its own OS thread + tokio runtime. Solves recursive async `Send` bounds. Parent communicates via oneshot channels.

**Prompt caching.** Historical messages never modified. Cache breakpoints placed every 4+ turns. System prompt and tool schemas marked `ephemeral`. ~95% hit rates.

**Atomic writes.** Write and edit tools use tmp file + rename. No partial writes on crash.

---

## Performance

| Metric | Value |
|--------|-------|
| Binary size | 6.5MB (stripped, LTO) |
| Startup | ~3ms |
| Tool schema reads | Zero-copy (`Arc<Vec<Value>>`) |
| Tests | 90 across 10 modules |
| Build (cold) | ~65s |
| Build (incremental) | ~37s |

```toml
[profile.release]
lto = true
codegen-units = 1
strip = true
panic = "abort"
```

## Cost Tracking

Real-time cost tracking in the TUI footer with per-model pricing:

| Model | Input | Output | Cache Read | Cache Write |
|-------|-------|--------|------------|-------------|
| Opus | $15/MTok | $75/MTok | $1.50/MTok | $18.75/MTok |
| Sonnet | $3/MTok | $15/MTok | $0.30/MTok | $3.75/MTok |
| Haiku | $0.80/MTok | $4/MTok | $0.08/MTok | $1/MTok |

Subagent costs tracked separately with their own model pricing.

---

## License

MIT — see [LICENSE](LICENSE).
