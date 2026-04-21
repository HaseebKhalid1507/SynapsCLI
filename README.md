# SynapsCLI

![Rust 1.80+](https://img.shields.io/badge/rust-1.80%2B-orange.svg)
![MIT License](https://img.shields.io/badge/license-MIT-blue.svg)
![~21K lines](https://img.shields.io/badge/lines-~21.3K-green.svg)
![GitHub stars](https://img.shields.io/github/stars/HaseebKhalid1507/SynapsCLI?style=social)

> **A Rust-native AI agent runtime that boots before your Node binary finishes `require()`-ing.**

Chat, orchestrate a crew of named subagents, or leave autonomous workers running 24/7 — all from **one** static binary. No Node. No Python. No Electron. No excuses.

<!-- screenshot: chatui with subagent panel + cyberpunk theme -->

---

## Why SynapsCLI?

- ⚡ **Sub-100ms cold start.** Single Rust binary, ~21K lines, `cargo build` and you're done.
- 🎭 **Named agents, not anonymous forks.** `subagent(agent: "spike", task: "...")` dispatches a crew member with their own soul. Watch them all work in a live panel.
- 🤖 **Autonomous mode that won't eat your wallet.** `watcher` supervises long-running agents with heartbeats, crash recovery, cost limits, and session handoff.
- 🎨 **18 themes.** cyberpunk, tokyo-night, gruvbox, catppuccin, nord, dracula, and friends. Live preview in `/settings`, hot-reload with `/theme`.
- 🧠 **90%+ prompt cache hit rate.** Hand-tuned cache breakpoints beat auto-cache (tested: 90% vs 53%). Built for multi-hour sessions.
- ✍️ **Mid-stream steering.** Type while the model is generating to redirect in real time.
- 🔌 **MCP + plugins + skills.** Model Context Protocol servers spawn lazily. Skills load from markdown. Plugins ship as marketplaces.

---

## Quick Start

```bash
cargo install synaps-cli
synaps login              # OAuth (Claude Pro/Max/Team)
synaps                    # Launch TUI
```

Or build from source:
```bash
git clone https://github.com/HaseebKhalid1507/SynapsCLI.git
cd SynapsCLI
cargo build --release
./target/release/synaps
```

Or use an API key instead of OAuth:
```bash
export ANTHROPIC_API_KEY="sk-ant-..."
synaps
```

`/help` for commands. `/theme` to browse the candy store.

---

## Usage

One binary, every mode as a subcommand.

### Default — Interactive TUI
```bash
synaps                              # launch TUI
synaps --continue                   # resume last session
synaps --system prompt.md           # custom system prompt
```

Streaming, markdown, syntax highlighting, and a live panel showing every subagent you dispatched.

```
╭ ◈ 4 agents ────────────────────────────────────╮
│  ✓ spike    done                         12.3s  │
│  ⠹ chrollo  ⚙ read (tool #5)              8.1s  │
│  ✓ shady    done                          9.7s  │
│  ⠹ zero     thinking...                   4.2s  │
╰─────────────────────────────────────────────────╯
```

### One-Shot
```bash
synaps run "explain this error"     # single prompt
synaps run "fix it" --agent spike   # with named agent
```

### Headless Chat
```bash
echo "explain this error" | cat error.log - | synaps chat
```

### Server Mode
```bash
synaps server --port 3145
synaps client ws://localhost:3145
```

### Autonomous Agents
```bash
synaps watcher init scout           # scaffold an agent
synaps watcher deploy scout         # run supervised
synaps watcher start                # start supervisor
synaps watcher status               # monitor the fleet
synaps watcher logs scout -f        # tail the brain
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

`minimal` · `cyberpunk` · `tokyo-night` · `gruvbox` · `catppuccin` · `nord` · `dracula` · `solarized-dark` · `solarized-light` · `monokai` · `one-dark` · `rose-pine` · `kanagawa` · `ayu-dark` · `ayu-light` · `github-dark` · `github-light` · `terminal` · `default`

Preview live in `/settings` (scroll to preview, Enter to confirm, Esc to revert). Or hot-swap instantly with `/theme <name>`.

---

<details>
<summary><b>🧬 Architecture (for the curious)</b></summary>

One binary. Subcommands dispatched from `main.rs`.

```
src/
├── main.rs      # unified CLI entry point + subcommand dispatch
├── cmd_*.rs     # subcommand handlers (run, chat, server, client, agent, login, watcher)
├── chatui/      # TUI: event loop, rendering, markdown, themes, settings
├── watcher/     # supervisor daemon, IPC, heartbeats
├── core/        # config, session, auth, protocol, logging
├── runtime/     # orchestration, SSE streaming, parallel tool exec
├── tools/       # bash, read, write, edit, grep, find, ls, subagent, mcp
├── mcp/         # JSON-RPC client, lazy server spawning
└── skills/      # markdown-driven behavioral guidelines + plugin marketplace
```

| Subcommand | Purpose |
|------------|---------|
| *(none)* | Interactive TUI with streaming + subagent panel |
| `run` | One-shot prompt, prints to stdout |
| `chat` | Headless streaming chat for scripting |
| `server` / `client` | WebSocket transport |
| `agent` | Worker runtime spawned by watcher |
| `watcher` | Supervisor daemon for autonomous agents |
| `login` | OAuth flow |

Config lives at `~/.synaps-cli/` — skills, plugins, sessions, agents, mcp.json. Project-local `.synaps-cli/` overrides global.

</details>

---

## License

MIT. See [LICENSE](LICENSE).

## Author

Built by [Haseeb Khalid](https://github.com/HaseebKhalid1507) because every other CLI agent was a 400MB Electron app pretending to be a terminal tool.
