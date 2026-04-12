# SynapsCLI

A terminal-native AI agent runtime built in Rust. 4.8MB binary, 3ms startup, 8 built-in tools, unlimited MCP tools, parallel subagents, on-demand skills — all in ~7,700 lines.

<p align="center">
  <img src="https://img.shields.io/badge/rust-4.8MB_binary-orange" alt="Rust" />
  <img src="https://img.shields.io/badge/startup-3ms-blue" alt="3ms startup" />
  <img src="https://img.shields.io/badge/MCP-lazy_loading-purple" alt="MCP" />
  <img src="https://img.shields.io/badge/skills-on_demand-green" alt="Skills" />
  <img src="https://img.shields.io/badge/license-MIT-lightgrey" alt="MIT" />
</p>

---

## Why SynapsCLI?

Most agent runtimes are 100K–450K lines of TypeScript. SynapsCLI does the same in **7,700 lines of Rust** — 3ms startup, 4.8MB binary, zero runtime dependencies.

```
8 built-in tools + entire MCP ecosystem on demand
Parallel subagent dispatch with live TUI panel
Skills loaded on demand — zero tokens until needed
Type while the agent streams (steering)
Smart scroll, abort with context preservation
OAuth + API key auth with auto-refresh
~95% prompt cache hit rates
```

## Quick Start

```bash
git clone https://github.com/HaseebKhalid1507/SynapsCLI.git
cd SynapsCLI
cargo build --release       # 4.8MB binary

# Auth (pick one)
./target/release/synaps-cli login                    # OAuth (Claude Pro/Max)
echo '{"anthropic":{"type":"api_key","key":"sk-ant-..."}}' > ~/.synaps-cli/auth.json  # API key

# Run
./target/release/synaps-cli chatui
```

## Features

### Tool System

Open trait-based architecture. Add tools at runtime — no recompilation.

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String>;
}
```

**8 built-in:** `bash` `read` `write` `edit` `grep` `find` `ls` `subagent`

**+ 2 gateway tools:** `mcp_connect` (lazy MCP server activation) · `load_skill` (on-demand skill loading)

**+ unlimited MCP tools** registered at runtime via `registry.register(Arc::new(tool))`

### MCP Integration — Lazy Loading

Connect to the entire [MCP ecosystem](https://registry.modelcontextprotocol.io/) — 1,800+ servers. Tools load **on demand**, not at startup. Zero token overhead until you need them.

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
    },
    "exa": {
      "command": "npx",
      "args": ["-y", "mcp-remote", "https://mcp.exa.ai/mcp"]
    }
  }
}
```

**How it works:**
1. Startup: parse config, register `mcp_connect` gateway tool (~200 tokens)
2. Agent calls `mcp_connect("exa")` when it needs web search
3. Server spawns, tools discovered via JSON-RPC, registered into live session
4. Next API call includes the new tools — cached from that point forward

**vs eager loading:** 65 tools × 9,500 tokens/msg → 10 tools × 1,200 tokens/msg. Saves ~830K tokens over 100 messages.

Compatible with Claude Code and Gemini CLI config format. Remote servers work via `mcp-remote` bridge — any URL from the [MCP registry](https://registry.modelcontextprotocol.io/).

TUI displays MCP tools cleanly:
```
⚡  read_pseudocode [byteray]
    session_id: abc123
```

### Skills — On-Demand Loading

Markdown skill files that load into conversation context when the agent needs them. Zero cost until activated.

```
~/.synaps-cli/skills/
├── rust.md
├── code-review.md
├── security-review.md
├── systematic-debugging.md
├── test-driven-development.md
└── verification-before-completion.md
```

```markdown
---
name: rust
description: Rust development best practices
---

When writing Rust code:
- Prefer `thiserror` for library errors...
```

**Two modes:**
- **On-demand** (default): agent calls `load_skill("rust")` → skill content returned as tool result → cached for rest of conversation
- **Auto-load** via config: `skills = rust, security-review` → injected into system prompt at launch

Skills get cached after first load — full price once, then 0.1× on every subsequent message.

### Multi-Agent Orchestration
- **Subagent dispatch** — named agents or inline prompts as one-shot workers
- **Parallel execution** — multiple subagents run concurrently
- **Real-time TUI panel** — live status with animated spinners
- **Agent files** — `~/.synaps-cli/agents/<name>.md` with YAML frontmatter
- **Recursive safety** — subagents can't spawn subagents
- **Zombie prevention** — shutdown signals via oneshot channel
- **Token forwarding** — costs tracked per-model

```
╭ ◈ 2 agents ─────────────────────────────────────────╮
│  ⠹ spike   ⚙ bash (tool #3)                  4.2s  │
│  ⠹ chrollo thinking...                        2.1s  │
╰──────────────────────────────────────────────────────╯
```

### TUI
- **Markdown rendering** — headers, code blocks (syntax highlighted), tables, lists, blockquotes
- **Smart scroll** — viewport stays still when scrolled up; auto-scrolls at bottom
- **Steering** — type and send while the agent streams (injected between tool rounds)
- **Message queue** — queued messages auto-fire on response completion
- **Abort context** — Escape saves partial work; next message gets interrupted context
- **Subagent panel** — animated spinners, per-agent status, elapsed timers
- **Token tracking** — input/output/cache tokens with running cost in footer
- **Input history** — arrow keys cycle through previous messages
- **Boot/exit animations** — CRT effects via tachyonfx

### Infrastructure
- **OAuth 2.0 PKCE** — browser auth with auto-refresh
- **API key fallback** — direct Anthropic API key
- **Typed config** — `SynapsConfig` struct, single parse path
- **Granular errors** — `Auth`, `Config`, `Session`, `Tool`, `Timeout`, `Cancelled`
- **HTTP timeouts** — connect (10s) + request (300s)
- **Structured tracing** — tool name, elapsed_ms, model, request lifecycle
- **WebSocket server** — Axum-based, multiple clients share a session
- **Session persistence** — auto-saved, `--continue` to resume
- **Profiles** — `--profile <name>` for separate namespaces

## Configuration

`~/.synaps-cli/config`:
```ini
model = claude-sonnet-4-20250514
thinking = high
skills = rust, security-review    # auto-load these skills
```

| File | Purpose |
|------|---------|
| `config` | Model, thinking level, auto-loaded skills |
| `system.md` | System prompt (auto-loaded) |
| `mcp.json` | MCP server definitions |
| `auth.json` | OAuth tokens / API key |
| `theme` | TUI color customization |
| `agents/<name>.md` | Subagent personalities |
| `skills/<name>.md` | Loadable skill files |

## Usage

```bash
synaps-cli chatui                          # Interactive TUI
synaps-cli run "explain quicksort"         # One-shot
synaps-cli run "review this" --agent spike # With agent
synaps-cli chatui --continue               # Resume session
synaps-cli chatui --continue abc123        # Resume specific
synaps-cli chatui --system "Be concise"    # Custom prompt
synaps-cli chatui --profile work           # Profile
synaps-cli server --port 3145              # WebSocket server
```

## Architecture

```
src/
├── chatui.rs    (2.4K)  TUI — ratatui, markdown, subagent panel, steering
├── runtime.rs   (1.3K)  API client, SSE streaming, agentic tool loop
├── tools.rs     (940)   Tool trait + 8 built-in implementations
├── auth.rs      (655)   OAuth 2.0 PKCE, token refresh with flock
├── server.rs    (559)   Axum WebSocket server
├── mcp.rs       (515)   MCP client — JSON-RPC 2.0, lazy loading, gateway tool
├── client.rs    (277)   WebSocket CLI client
├── skills.rs    (223)   Skill loading, on-demand tool, config auto-load
├── session.rs   (190)   Session persistence (JSON)
├── config.rs    (160)   Typed config, profiles, system prompt resolution
├── main.rs      (105)   CLI entry (clap)
├── protocol.rs  (126)   Shared message types
├── login.rs     (80)    OAuth browser flow
├── error.rs     (21)    Error types
├── logging.rs   (27)    Tracing setup
├── chat.rs      (133)   Simple REPL (non-TUI)
└── lib.rs       (18)    Module root
                ─────
                7,717 lines total
```

### Design Decisions

**Trait-based tools.** Open `Tool` trait with `Arc<dyn Tool>` storage. Runtime-registerable via `registry.register()`. Enables MCP, skills, and custom tools without modifying core code.

**Lazy everything.** MCP servers don't connect until called. Skills don't load until needed. Tool schemas use `Arc<Vec<Value>>` for zero-copy reads. You only pay for what you use.

**Shared mutable registry.** `Arc<RwLock<ToolRegistry>>` allows `mcp_connect` to register new tools mid-conversation. Snapshot-before-await pattern prevents deadlocks — registry is cloned before long API calls, ensuring the write lock is always available for tool registration.

**Subagent isolation.** Each subagent runs on a dedicated OS thread with its own tokio runtime. Solves recursive async `Send` bounds. Parent communicates via oneshot channels.

**Prompt caching.** Historical messages never modified. Cache breakpoints on last user message every 4+ turns. System prompt and tool schemas marked `ephemeral`. ~95% hit rates.

**Steering.** Messages typed during streaming inject between tool rounds via unbounded channel. Fallback queue fires on completion.

## Performance

| Metric | Value |
|--------|-------|
| Binary | 4.8MB (stripped, LTO, single codegen unit) |
| Startup | 3ms |
| Tool schema | Zero-copy via `Arc<Vec<Value>>` |
| Build (cold) | ~1m 45s |
| Build (incremental) | ~22s |

```toml
[profile.release]
lto = true
codegen-units = 1
strip = true
panic = "abort"
```

## Cost Tracking

| Model | Input | Output | Cache Read | Cache Write |
|-------|-------|--------|------------|-------------|
| Opus | $15.00/MTok | $75.00/MTok | $1.50/MTok | $18.75/MTok |
| Sonnet | $3.00/MTok | $15.00/MTok | $0.30/MTok | $3.75/MTok |
| Haiku | $0.80/MTok | $4.00/MTok | $0.08/MTok | $1.00/MTok |

Subagent costs tracked with the subagent's model. Running total in TUI footer.

## License

MIT
