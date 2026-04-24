<p align="center">
  <img src="assets/banner.png" alt="SynapsCLI" />
</p>

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
- 🖱️ **Mouse text selection.** Left-click drag to select, right-click to copy/paste. Works in the TUI.
- 🔌 **MCP + plugins + skills.** Model Context Protocol servers spawn lazily. Skills load from markdown. Plugins ship as marketplaces.
- 🔗 **Plugin agent resolution.** `subagent(agent: "dev-tools:sage")` — dispatch agents from installed plugins via `plugin:agent` syntax.

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

# Custom keybinds
keybind.F5 = /compact
keybind.A-h = /help
keybind.A-k = /keybinds
keybind.F6 = disabled
```

Provider keys can also be set via environment variables (`GROQ_API_KEY`, `CEREBRAS_API_KEY`, etc.) or through `/settings → Providers` in the TUI.

---

## Keybinds

Plugins can register keyboard shortcuts, and users can override or add their own.

### User keybinds (in config)

```
keybind.F5 = /compact           # F5 runs /compact
keybind.A-s = /scholar          # Alt+S runs /scholar
keybind.A-k = /keybinds         # Alt+K shows all keybinds
keybind.F6 = disabled           # Disable a plugin keybind
```

### Plugin keybinds (in plugin.json)

Plugins declare keybinds in their manifest:

```json
{
  "name": "my-plugin",
  "keybinds": [
    {
      "key": "F5",
      "action": "slash_command",
      "command": "compact",
      "description": "Quick compact"
    },
    {
      "key": "A-r",
      "action": "load_skill",
      "skill": "code-review",
      "description": "Load code review"
    }
  ]
}
```

### Key notation

| Notation | Meaning |
|----------|---------|
| `C-x` | Ctrl+X |
| `A-x` | Alt+X |
| `S-x` | Shift+X |
| `C-A-x` | Ctrl+Alt+X |
| `F1`–`F12` | Function keys |
| `Space`, `Tab`, `Enter`, `Esc` | Special keys |

### Priority

Core keybinds (Ctrl+C, Esc, Enter, etc.) are never overridable. User config overrides plugins. `/keybinds` to see what's registered.

---

<details>
<summary><b>🧬 Architecture (for the curious)</b></summary>

One binary. Subcommands dispatched from `main.rs`. Two API paths: Anthropic (native) and OpenAI-compatible (17 providers).

```
src/
├── main.rs          # unified CLI entry point + subcommand dispatch
├── cmd/             # subcommand handlers (run, chat, server, client, agent, login, watcher)
├── chatui/          # TUI: event loop, rendering, markdown, themes, settings, plugins
│   └── settings/    # /settings modal — model picker, provider keys, themes
├── runtime/         # THE BRAIN
│   ├── api.rs       # Anthropic API + provider router (try_route)
│   ├── stream.rs    # tool dispatch loop (provider-agnostic)
│   └── openai/      # OpenAI-compatible provider engine
│       ├── registry.rs   # 17 providers, 55+ models
│       ├── stream.rs     # SSE streaming + BytesMut/memchr parsing
│       ├── translate.rs  # Anthropic↔OpenAI format bridge
│       ├── wire.rs       # StreamDecoder + tool call accumulation
│       └── ping.rs       # /ping health check
├── core/            # config, session, chain, auth, models, logging
├── events/          # event bus: types, priority queue, inotify watcher
├── tools/           # 15 built-in tools (bash, read, write, edit, subagent*, shell*, etc.)
├── mcp/             # Model Context Protocol client, lazy server spawning
├── watcher/         # supervisor daemon, IPC, heartbeats
└── skills/          # markdown-driven behavioral guidelines + plugin marketplace
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
| `send` | Push events into a running session |
| `status` | Check account usage |

**Provider routing:** Model IDs with a `/` (e.g. `groq/llama-3.3-70b`) route through `runtime/openai/`. Everything else goes to Anthropic. Both paths emit the same `StreamEvent` type — the TUI and tool loop are provider-blind.

Config lives at `~/.synaps-cli/` — config, sessions, agents, plugins, skills, chains, mcp.json. Project-local `.synaps-cli/` overrides global. Provider API keys stored with `0600` permissions.

</details>

---

## License

MIT. See [LICENSE](LICENSE).

## Author

Built by [Haseeb Khalid](https://github.com/HaseebKhalid1507) because every other CLI agent was a 400MB Electron app pretending to be a terminal tool.
