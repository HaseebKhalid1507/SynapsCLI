<p align="center">
  <img src="assets/banner.png" alt="SynapsCLI" width="100%" />
</p>

<h3 align="center">Lightning fast terminal native agent harness</h3>

<p align="center">
  <a href="https://crates.io/crates/synaps"><img src="https://img.shields.io/crates/v/synaps?color=orange&label=crates.io" alt="crates.io"></a>
  <img src="https://img.shields.io/badge/rust-1.80%2B-orange.svg" alt="Rust">
  <a href="https://ratatui.rs/"><img src="https://ratatui.rs/built-with-ratatui/badge.svg" alt="Built With Ratatui" height="20"></a>
  <img src="https://img.shields.io/badge/binary-15MB-success.svg" alt="15MB binary">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="License"></a>
  <a href="https://discord.gg/JCdgRYqVDP"><img src="https://img.shields.io/badge/Discord-join%20the%20server-5865F2?logo=discord&logoColor=white" alt="Discord"></a>
</p>

<p align="center">
  Run AI agents from one binary. Tools, subagents, and extensions built in.<br>
  Any model, 15MB, 2ms boot.
</p>

---

<!-- TODO: replace with the hero demo GIF (see task #115): single strong view, high contrast, app not shell -->
<p align="center">
  <img src="https://github.com/user-attachments/assets/8e0ae020-cf63-4547-b769-782625cbd1f6" alt="SynapsCLI — a crew of agents running in parallel" width="900" />
</p>

Synaps is a fast, terminal-native agent harness written in Rust. It runs AI agents with built-in tools, subagents, and extensions, and works with any model, from Claude and ChatGPT to a local Ollama. Run it as a TUI, headless in CI, a server, or a daemon. Extend it with plugins in any language, MCP, and an event bus, and adapt it to your workflow instead of the other way around.

---

## Install

```bash
cargo install synaps              # crates.io
```

<details>
<summary>More options (brew, AUR, .deb, shell installer, source)</summary>

```bash
brew install HaseebKhalid1507/tap/synaps    # macOS / Linux
yay -S synaps                               # Arch / EndeavourOS

# Debian/Ubuntu
curl -LO https://github.com/HaseebKhalid1507/SynapsCLI/releases/latest/download/synaps_amd64.deb
sudo dpkg -i synaps_amd64.deb

# Shell installer (any platform)
curl -sSL https://github.com/HaseebKhalid1507/SynapsCLI/releases/latest/download/synaps-installer.sh | sh

# From source
git clone https://github.com/HaseebKhalid1507/SynapsCLI && cd SynapsCLI
cargo build --release && ./target/release/synaps
```
</details>

*New to agents? Start with the [ELI5](ELI5.md). Want the full tour? The [Wiki](https://github.com/HaseebKhalid1507/SynapsCLI/wiki) has 36 pages.*

---

## Quick start

```bash
synaps login          # Claude Pro/Max, or a provider key, or point at local Ollama
synaps                # launch the TUI
```

Headless, same engine, for scripts and CI:

```bash
echo "summarize the git diff" | synaps chat
```

### Sign in with what you already pay for, or nothing at all

```bash
synaps login                      # pick Claude or ChatGPT (Codex) from the provider menu

# ChatGPT / Codex
synaps                            # then: /model openai-codex/gpt-5.5

# Ollama or any local model (no account, no key, no cloud)
ollama serve                      # LM Studio, vLLM, llama.cpp all work too
synaps                            # then: /model local/llama3.2
```

Synaps auto-targets `http://localhost:11434/v1`, which is Ollama's default, so a running Ollama just works. Point it anywhere else with `provider.local.url` in config or the `LOCAL_ENDPOINT` env var. Your keys, your box, nothing phones home.

---

## What's in the box

- **🎭 Named agents.** Crew members with roles. Dispatch by name, watch them think in a live panel.
- **🔄 Steer them mid-flight.** dispatch, poll, steer, collect. Redirect an agent while it's still working. This is the part nobody else does.
- **🔌 Build anything on it.** Process-isolated extensions in any language, MCP servers, an event bus, custom tools. A small core with enough hooks to bolt on whatever you want and glue it to whatever you've got.
- **🌐 Any OpenAI-compatible model, cloud or local.** Claude and ChatGPT natively, plus Groq, Cerebras, NVIDIA NIM, OpenRouter, or your own Ollama. Run a different model per agent. Swap mid-session with `/model`.
- **📡 Event bus.** Any script, cron, or service can poke a running session and the agent reacts in real time.
- **🧠 Context that lasts.** 90%+ prompt-cache hit rate. `/compact` checkpoints history. Chain sessions across days.
- **🤖 Autonomous mode.** `synaps watcher` runs a fleet with heartbeats, crash recovery, cost limits, and session handoff.
- **⚡ Fast and lean.** ~87K lines of Rust, one 15MB binary, ~2ms cold start, zero runtime deps.
- **🎨 18 themes.** `catppuccin`, `gruvbox`, `nord`, `rose-pine`, `dracula`, `tokyo-night`, plus originals like `neon-rain` and `night-city`. Hot-swap with `/theme`.

---

## Modes

| Command | What it does |
|---------|-------------|
| `synaps` | Interactive TUI: streaming, markdown, syntax highlighting, subagent panel |
| `synaps chat` | Headless, same engine, stdin/stdout. Scripts, pipes, CI |
| `synaps server` | WebSocket API: token auth, origin validation, streaming |
| `synaps rpc` | Line-JSON IPC for bridges (Slack, Discord) |
| `synaps watcher` | Supervisor daemon for autonomous agent fleets |

## Configuration

No YAML. No TOML. No JSON. Just `key = value` in `~/.synaps-cli/config`:

```ini
model = claude-sonnet-4-6
thinking = high
theme = tokyo-night
identity = You are a senior engineer who writes clean, tested code.
disabled_tools = bash, ls          # remove built-ins at boot (read-only profiles)
provider.groq = gsk_...
```

<details>
<summary>Advanced: shared-credential broker, bridge mirror, all keys</summary>

See the [Wiki](https://github.com/HaseebKhalid1507/SynapsCLI/wiki) for the full config reference, the multi-machine **auth broker** (share one OAuth credential across a fleet over WireGuard or TLS), and the bridge heartbeat mirror.
</details>

---

## Extensions

Drop a folder in `~/.synaps-cli/plugins/` and it's live on next boot:

```
~/.synaps-cli/plugins/my-guard/
├── .synaps-plugin/plugin.json    # manifest: hooks, permissions, keybinds
└── main.py | index.js | <any>    # JSON-RPC 2.0 over stdio, any language
```

Extensions are separate processes, not linked code, so they're language-agnostic, crash-isolated, and sandboxed. Hook `before_tool_call`, `before_message`, `on_session_start`, and 4 more.

Protocol spec: [docs/extensions/](docs/extensions/).

---

## Contributing

Synaps is young (started April 2026) and moving fast: 1,300+ commits in its first 3 months, shipped to crates.io, Homebrew, and the AUR, and listed in [awesome-ratatui](https://github.com/ratatui/awesome-ratatui). Good first contributions: a new provider in the catalog, an extension, a theme, docs, or any [`good first issue`](https://github.com/HaseebKhalid1507/SynapsCLI/labels/good%20first%20issue).

```bash
git clone https://github.com/HaseebKhalid1507/SynapsCLI && cd SynapsCLI
cargo build && cargo test
```

See [CONTRIBUTING.md](CONTRIBUTING.md). Issues and PRs welcome, the maintainer answers fast.

Come hang out in the [**Synaps Discord**](https://discord.gg/JCdgRYqVDP) — questions, ideas, show-and-tell, or just to watch the crew run.

---

## Philosophy

- **Agents are not chat.** They're autonomous programs that happen to use language models. Treat them like services.
- **Multi-agent is the default.** Single-agent is just n=1.
- **Speed is a feature.** A 2-second boot already lost the dev who wanted it in a git hook.
- **The terminal is the IDE.** If you need Electron to be productive, your tools are wrong.

<details>
<summary><b>Architecture</b></summary>

```
crates/
├── agent-core/      # identity, models, auth, pricing
├── agent-engine/    # LLM runtime (Anthropic-native + OpenAI-compat router),
│                    #   tools, extensions, MCP, events, skills, sidecar
├── agent-tui/       # ratatui terminal UI, themes, settings, plugin modals
└── (root)           # the `synaps` binary crate: CLI dispatch, watcher, broker
```
Two API paths (Anthropic native and OpenAI-compatible for 17 providers) both emit the same `StreamEvent`, so the TUI and tool loop are provider-blind.
</details>

---

## License

Apache 2.0. See [LICENSE](LICENSE).

<p align="center">
  <sub>Synaps is built with Synaps.</sub>
</p>

<p align="center">
  <sub>Because every other CLI agent was a 400MB Electron app pretending to be a terminal tool.</sub>
</p>
