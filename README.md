# SynapsCLI

![Rust 1.80+](https://img.shields.io/badge/rust-1.80%2B-orange.svg)
![MIT License](https://img.shields.io/badge/license-MIT-blue.svg)
![~14K lines](https://img.shields.io/badge/lines-~14.4K-green.svg)
![GitHub stars](https://img.shields.io/github/stars/HaseebKhalid1507/SynapsCLI?style=social)

> **A Rust-native AI agent runtime that boots before your Node binary finishes `require()`-ing.**

Chat, orchestrate a crew of named subagents, or leave autonomous workers running 24/7 — all from one static binary. No Node. No Python. No Electron. No excuses.

<!-- screenshot: chatui with subagent panel + cyberpunk theme -->

---

## Why SynapsCLI?

- ⚡ **Sub-100ms cold start.** Single Rust binary, ~14K lines, `cargo build` and you're done.
- 🎭 **Named agents, not anonymous forks.** `subagent(agent: "spike", task: "...")` dispatches a crew member with their own soul. Watch them all work in a live panel.
- 🤖 **Autonomous mode that won't eat your wallet.** `watcher` supervises long-running agents with heartbeats, crash recovery, cost limits, and session handoff.
- 🎨 **18 themes.** cyberpunk, tokyo-night, gruvbox, catppuccin, nord, dracula, and friends. Your terminal deserves this.
- 🧠 **90%+ prompt cache hit rate.** Hand-tuned cache breakpoints beat auto-cache (tested: 90% vs 53%). Built for multi-hour sessions.
- ✍️ **Mid-stream steering.** Type while the model is generating to redirect in real time.
- 🔌 **MCP + plugins + skills.** Model Context Protocol servers spawn lazily. Skills load from markdown. Plugins ship as marketplaces.

---

## Quick Start

```bash
git clone https://github.com/HaseebKhalid1507/SynapsCLI.git
cd SynapsCLI
cargo build --release
```

**Authenticate** — pick one:
```bash
./target/release/login                      # OAuth (Claude Pro/Max/Team)
export ANTHROPIC_API_KEY="sk-ant-..."       # or API key
```

**Launch:**
```bash
./target/release/chatui
```

`/help` for commands. `/theme` to browse the candy store.

---

## Three Modes, One Binary

### 💬 `chatui` — Interactive TUI

Streaming, markdown, syntax highlighting, and a live panel showing every subagent you dispatched.

```
╭ ◈ 4 agents ────────────────────────────────────╮
│  ✓ spike    done                         12.3s  │
│  ⠹ chrollo  ⚙ read (tool #5)              8.1s  │
│  ✓ shady    done                          9.7s  │
│  ⠹ zero     thinking...                   4.2s  │
╰─────────────────────────────────────────────────╯
```

### 📡 `chat` — Headless Pipe

Stdin/stdout. Perfect for scripts, CI, and UNIX pipelines that believe in themselves.

```bash
echo "explain this error" | cat error.log - | ./target/release/chat
```

### 🤖 `watcher` — Autonomous Daemon

```bash
watcher init scout      # scaffold an agent
watcher deploy scout    # run supervised
watcher status          # monitor the fleet
watcher logs scout -f   # tail the brain
```

Minimal agent config (`~/.synaps-cli/watcher/scout/config.toml`):

```toml
[agent]
name = "scout"
model = "claude-sonnet-4-20250514"
trigger = "watch"               # manual | always | watch

[trigger]
paths = ["./src"]
patterns = ["*.rs"]

[limits]
max_session_cost_usd = 0.50
max_daily_cost_usd = 10.0
```

Full schema in [AGENTS.md](AGENTS.md). Agents checkpoint state on exit and resume where they left off.

---

## Built-in Tools

| Tool | Purpose |
|------|---------|
| `bash` | Shell execution (30s default, 300s max) |
| `read` / `write` / `edit` | Atomic, UTF-8 validated file ops |
| `grep` / `find` / `ls` | Regex, glob, directory ops |
| `subagent` | Dispatch a named crew member |
| `mcp_connect` | Load tools from MCP servers |
| `load_skill` | Load behavioral guidelines from markdown |

See [AGENTS.md](AGENTS.md) for parameters and behavior.

---

## Themes

`minimal` · `cyberpunk` · `tokyo-night` · `gruvbox` · `catppuccin` · `nord` · `dracula` · `solarized-dark` · `solarized-light` · `monokai` · `one-dark` · `rose-pine` · `kanagawa` · `ayu-dark` · `ayu-light` · `github-dark` · `github-light` · `terminal`

Switch live with `/theme`.

---

---|---|---|---|---|
| Language | **Rust** | Node | Node | Node | Go |
| Cold start | **<100ms** | ~1–3s | ~1–3s | ~1–3s | ~200ms |
| Named agents | ✅ | ❌ | ❌ | ❌ | partial |
| Autonomous daemon | ✅ | ❌ | ❌ | ❌ | ❌ |
| Themes | **18** | 1 | 1 | few | many |
| MCP | ✅ | ✅ | ❌ | ✅ | ✅ |
| License | MIT | proprietary | Apache-2.0 | Apache-2.0 | FSL |

---

<details>
<summary><b>🧬 Architecture (for the curious)</b></summary>

8 binaries, 43 source files, ~14,400 lines of Rust:

```
src/
├── bin/         # chat · chatui · cli · login · server · client · agent
├── core/        # config, session, auth, protocol, logging
├── runtime/     # orchestration loop, SSE streaming, parallel tool exec
├── tools/       # bash, read, write, edit, grep, find, ls, subagent, ...
├── chatui/      # event loop, rendering, markdown, syntect, themes
├── watcher/     # supervisor, IPC (Unix socket), heartbeats, file watch
├── mcp.rs       # JSON-RPC client, lazy server spawning
└── skills.rs    # markdown-driven behavioral guidelines
```

| Binary | Purpose |
|--------|---------|
| `chatui` | Interactive TUI with streaming + subagent panel |
| `chat` | Headless chat for scripting |
| `watcher` | Supervisor daemon for autonomous agents |
| `synaps-agent` | Worker runtime spawned by watcher |
| `login` | OAuth flow |
| `server` / `client` | WebSocket transport |
| `cli` | Simple run/chat subcommands |

Config lives at `~/.synaps-cli/` — skills, plugins, sessions, agents, mcp.json. Project-local `.synaps-cli/` overrides global.

</details>

---

## License

MIT. See [LICENSE](LICENSE).

## Author

Built by [Haseeb Khalid](https://github.com/HaseebKhalid1507) because every other CLI agent was a 400MB Electron app pretending to be a terminal tool.
