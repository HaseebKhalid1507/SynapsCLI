# SynapsCLI

![Rust 1.80+](https://img.shields.io/badge/rust-1.80%2B-orange.svg)
![MIT License](https://img.shields.io/badge/license-MIT-blue.svg)
![~32K lines](https://img.shields.io/badge/lines-~32K-green.svg)
![GitHub stars](https://img.shields.io/github/stars/HaseebKhalid1507/SynapsCLI?style=social)

> **A Rust-native AI agent runtime that boots before your Node binary finishes `require()`-ing.**

One binary, any model. Start with Claude, drop in a free Groq key, point at localhost for private — same subagents, same TUI, same config. No Node. No Python. No Electron. No excuses.

<!-- screenshot: chatui with subagent panel + cyberpunk theme -->

---

## Why SynapsCLI?

- ⚡ **Sub-100ms cold start.** Single Rust binary, ~32K lines, `cargo build` and you're done.
- 🌐 **Any model, any provider.** Claude, Groq, Cerebras, NVIDIA NIM, OpenRouter, or your local Ollama. 17 providers, 55+ models. Set a key, pick a model, go.
- 🎭 **Named agents, not anonymous forks.** `subagent(agent: "spike", task: "...")` dispatches a crew member with their own soul. Watch them all work in a live panel.
- 📡 **Event Bus.** External systems push events into a running session — the agent reacts in real time. `synaps send` from any script, cron job, or monitoring tool.
- 🔄 **Reactive Subagents.** Dispatch, poll, steer, collect. Five tools that turn fire-and-forget into collaborative orchestration.
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

### Other Models

No Claude key? No problem. SynapsCLI works with any OpenAI-compatible provider.

```bash
# Set a free provider key (Groq, Cerebras, NVIDIA — no credit card)
export GROQ_API_KEY="gsk_..."
synaps
/model groq/llama-3.3-70b-versatile
```

Or configure in `~/.synaps-cli/config`:
```
provider.groq = gsk_...
provider.cerebras = csk-...
provider.nvidia = nvapi-...
provider.local.url = http://localhost:11434/v1
model = groq/llama-3.3-70b-versatile
```

17 providers supported. `/ping` to health-check them all. Manage keys in `/settings → Providers`.

| Provider | Free tier | Models |
|----------|-----------|--------|
| Groq | 30 RPM, 14.4K req/day | Llama 3.3 70B, Llama 4 Scout |
| Cerebras | 30 RPM, 1M tokens/day | Qwen3 235B (S+ tier) |
| NVIDIA NIM | ~40 RPM | Qwen3 Coder 480B, Devstral 2, Mistral Large 675B |
| Google AI Studio | 60 RPM | Gemini 2.5 Flash |
| Local (Ollama/vLLM) | Unlimited | Any local model |

Full list: `synaps` → `/settings` → Providers.

`/help` for commands. `/theme` to browse the candy store. `/compact` when context gets long. `/status` to check usage. `/saveas <name>` to alias a session. `/chain name <name>` to bookmark a compaction lineage.

```bash
synaps send "alert" --source monitoring  # inject events from anywhere
```

---

## Usage

One binary, every mode as a subcommand.

### Default — Interactive TUI
```bash
synaps                              # launch TUI
synaps --continue                   # resume last session
synaps --continue my-project        # resume by name (session alias or chain bookmark)
synaps --system prompt.md           # custom system prompt
```

Name a session with `/saveas my-project`, or bookmark a compaction lineage with `/chain name my-project` — then `synaps --continue my-project` picks up where you left off. Resolution tries chain name → session name → partial ID.

Streaming, markdown, syntax highlighting, and a live panel showing every subagent you dispatched.

```
╭ ◈ 4 agents ────────────────────────────────────╮
│  ✓ spike    done                         12.3s  │
│  ⠹ chrollo  ⚙ read (tool #5)              8.1s  │
│  ✓ shady    done                          9.7s  │
│  ⠹ zero     thinking...                   4.2s  │
╰─────────────────────────────────────────────────╯
```

### Usage Status
```bash
synaps status                    # check account usage + reset times
```
Or use `/status` inside the TUI.

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

### Event Bus
```bash
# Any script, cron job, or service can push events into a running session
synaps send "Jellyfin is DOWN" --source uptime-kuma --severity high
synaps send "deploy complete" --source ci --severity low --content-type event
synaps send "check PR #42" --source github --channel reviews
```
Events appear as styled cards in the TUI and auto-trigger the agent to respond. During streaming, events buffer and flush after the current response completes.

---

## Built-in Tools

| Tool | Purpose |
|------|---------|
| `bash` | Shell execution (30s default, 300s max) |
| `read` / `write` / `edit` | Atomic, UTF-8 validated file ops |
| `grep` / `find` / `ls` | Regex, glob, directory ops |
| `subagent` | Dispatch a named crew member |
| `subagent_start` | Dispatch reactive subagent (returns immediately) |
| `subagent_status` | Poll running subagent progress |
| `subagent_steer` | Inject guidance into running subagent |
| `subagent_collect` | Check if subagent is done, get result |
| `shell_start/send/end` | Interactive PTY sessions |
| `connect_mcp_server` | Load tools from MCP servers |
| `load_skill` | Load behavioral guidelines from markdown |

See [AGENTS.md](AGENTS.md) for parameters and behavior.

---

## Context Compaction

Long sessions eat context. `/compact` fixes that.

```
/compact                          # summarize & replace history
/compact focus on the auth module  # with custom focus
```

The LLM produces a structured checkpoint (goals, progress, decisions, file ops, next steps) and the entire message history is replaced with that summary. Iterative — `/compact` again merges new work into the existing summary. Inspired by [pi coding agent](https://github.com/badlogic/pi-mono/tree/main/packages/coding-agent).

- Chain sessions: `/compact` creates a new linked session, preserving the original
- Configurable model: compaction defaults to Sonnet (saves tokens)
- System prompt carried through compaction chains

---

## Themes

`minimal` · `cyberpunk` · `tokyo-night` · `gruvbox` · `catppuccin` · `nord` · `dracula` · `solarized-dark` · `solarized-light` · `monokai` · `one-dark` · `rose-pine` · `kanagawa` · `ayu-dark` · `ayu-light` · `github-dark` · `github-light` · `terminal` · `default`

Preview live in `/settings` (scroll to preview, Enter to confirm, Esc to revert). Or hot-swap instantly with `/theme <name>`.

---

## Configuration

Config lives at `~/.synaps-cli/config`. Simple `key = value` format.

```
# Model
model = claude-opus-4-7
thinking = high
context_window = 200k
compaction_model = claude-sonnet-4-6

# Provider API keys
provider.groq = gsk_...
provider.cerebras = csk-...
provider.nvidia = nvapi-...
provider.local.url = http://localhost:11434/v1

# Appearance
theme = cyberpunk
```

Provider keys can also be set via environment variables (`GROQ_API_KEY`, `CEREBRAS_API_KEY`, etc.) or through `/settings → Providers` in the TUI.

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
├── core/        # config, session, chain, auth, protocol, logging
├── events/      # event bus: types, priority queue, inotify watcher, formatting
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
