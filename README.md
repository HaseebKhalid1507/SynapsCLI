<p align="center">
  <img src="assets/banner.png" alt="SynapsCLI" width="100%" />
</p>

<h3 align="center">Run a crew of AI agents in your terminal. Dispatch them, steer them mid-task, watch them work in parallel.</h3>

<p align="center">
  <a href="https://crates.io/crates/synaps"><img src="https://img.shields.io/crates/v/synaps?color=orange&label=crates.io" alt="crates.io"></a>
  <img src="https://img.shields.io/badge/rust-1.80%2B-orange.svg" alt="Rust">
  <a href="https://ratatui.rs/"><img src="https://ratatui.rs/built-with-ratatui/badge.svg" alt="Built With Ratatui" height="20"></a>
  <img src="https://img.shields.io/badge/binary-15MB-success.svg" alt="15MB binary">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="License"></a>
</p>

<p align="center">
  One Rust binary. A whole crew of named agents, each with a role, running at once.<br>
  15MB, boots in 2ms, works offline.<br><br>
  <a href="https://github.com/HaseebKhalid1507/SynapsCLI/wiki"><b>📖 Wiki</b></a> · <a href="https://github.com/HaseebKhalid1507/SynapsCLI/wiki/Installation"><b>⚡ Quick Start</b></a> · <a href="ELI5.md"><b>🧒 ELI5</b></a> · <a href="#benchmarks"><b>📊 Benchmarks</b></a>
</p>

---

<!-- TODO: replace with the hero demo GIF (see task #115): single strong view, high contrast, app not shell -->
<p align="center">
  <img src="assets/demo.gif" alt="SynapsCLI Demo" width="720" />
</p>

---

## Why a crew?

Every other agent CLI gives you one assistant in a box. One model, one train of thought, one thing at a time. That's fine until the job is bigger than one head.

Synaps gives you a team. Dispatch `spike` to refactor while `chrollo` audits the codebase and `shady` tears the result apart, all at once, each with its own system prompt and tools. Poke them mid-task, redirect them, pull the results when they're done. You're not chatting with an AI. You're running a crew, and you're the one calling the shots.

And yeah, it's a single 15MB Rust binary that boots in 2ms. The anti-bloat stuff is real. It's just not the point.

```
╭ ◈ 4 agents ────────────────────────────────────╮
│  ✓ spike    done                         12.3s  │
│  ⠹ chrollo  ⚙ read (tool #5)              8.1s  │
│  ✓ shady    done                          9.7s  │
│  ⠹ zero     thinking...                   4.2s  │
╰─────────────────────────────────────────────────╯
```

```bash
subagent(agent: "spike", task: "refactor the auth module")        # dispatch and wait
subagent_start(agent: "chrollo", task: "audit this codebase")     # dispatch reactive
subagent_steer(handle_id: "sa_1", message: "focus on the API routes")  # redirect mid-run
subagent_collect(handle_id: "sa_1")                                # gather when ready
```

Agents aren't anonymous forks. They're crew members with names, system prompts, specializations, and memory. A `watcher` daemon supervises the fleet so nothing crashes or blows your budget. You can even run a different model per agent: `chrollo` on Claude for the deep stuff, `spike` on a local Ollama for grunt work, same crew.

---

## Quick start

```bash
cargo install synaps
synaps login          # Claude Pro/Max, or a provider key, or point at local Ollama
synaps                # launch the TUI
```

Headless, same engine, for scripts and CI:

```bash
echo "summarize the git diff" | synaps chat
```

### Sign in with what you already pay for, or nothing at all

**Claude Pro/Max** (OAuth, no API key):
```bash
synaps login                      # or non-interactive: synaps login --provider claude
```

**ChatGPT Plus/Pro** (OAuth via Codex):
```bash
synaps login --provider openai-codex
synaps                            # then: /model openai-codex/gpt-5.5
```

**Ollama or any local model** (no account, no key, no cloud):
```bash
ollama serve                      # LM Studio, vLLM, llama.cpp all work too
synaps                            # then: /model local/llama3.2
```

Synaps auto-targets `http://localhost:11434/v1`, which is Ollama's default, so a running Ollama just works. Point it anywhere else with `provider.local.url` in config or the `LOCAL_ENDPOINT` env var. Your keys, your box, nothing phones home.

---

## What's in the box

- **🎭 Named agents.** Crew members with roles. Dispatch by name, watch them think in a live panel.
- **🔄 Steer them mid-flight.** dispatch, poll, steer, collect. Redirect an agent while it's still working. This is the part nobody else does.
- **🌐 Any OpenAI-compatible model, cloud or local.** Claude and ChatGPT natively, plus Groq, Cerebras, NVIDIA NIM, OpenRouter, or your own Ollama. Run a different model per agent. Swap mid-session with `/model`.
- **🔌 Process-isolated extensions.** JSON-RPC 2.0 over stdio, any language, crash-isolated, sandboxed. Hook 7 lifecycle events to add guardrails, memory, context injection, whatever you need.
- **📡 Event bus.** Any script, cron, or service can poke a running session and the agent reacts in real time.
- **🧠 Context that lasts.** 90%+ prompt-cache hit rate. `/compact` checkpoints history. Chain sessions across days.
- **🤖 Autonomous mode.** `synaps watcher` runs a fleet with heartbeats, crash recovery, cost limits, and session handoff.
- **⚡ Fast and lean.** ~87K lines of Rust, one 15MB binary, ~2ms cold start, zero runtime deps.
- **🎨 18 themes.** `catppuccin`, `gruvbox`, `nord`, `rose-pine`, `dracula`, `tokyo-night`, plus originals like `neon-rain` and `night-city`. Hot-swap with `/theme`.

> **Honest scope:** Synaps is Anthropic-first today. The Anthropic path has the deepest feature support (caching, retry, cost). The OpenAI-compatible path covers everything else and is being brought to full parity ([tracking](docs/open-provider-issues.md)). Rather tell you that than overclaim.

> Built with a crew: this release's changelog was written by a Synaps agent crew running in parallel. It uses itself.

---

## Benchmarks

Cold-start overhead, the tax every tool pays before it does any work:

| command | median | |
|---|---|---|
| `synaps --version` (Rust binary) | **2.3 ms** | |
| `python3 -c pass` (bare interpreter) | 30.2 ms | ~13× slower, importing nothing |
| `node -e ''` (bare interpreter) | 46.1 ms | ~20× slower, importing nothing |

Synaps fully starts before a Python or Node interpreter finishes launching, before a single dependency is imported. A LangChain or CrewAI import stacks hundreds of ms on top of that floor.

*Methodology:* 30 runs each, 3 warmups, median reported, `time.perf_counter` around `subprocess.run`. Reproduce with the three commands above. This measures startup overhead, not end-to-end agent latency (that's model-bound, identical across runtimes). The point is the runtime itself adds almost nothing.

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

Extensions are separate processes, not linked code, so they're language-agnostic, crash-isolated, and sandboxed. Hook `before_tool_call`, `before_message`, `on_session_start`, and 4 more. Real example: **finlens**, a 6-lens finance research workflow built entirely as an extension with no core changes.

Protocol spec: [docs/extensions/](docs/extensions/).

---

## Contributing

Synaps is young (started April 2026) and moving fast: 1,300+ commits in its first 3 months, shipped to crates.io, Homebrew, and the AUR, and listed in [awesome-ratatui](https://github.com/ratatui/awesome-ratatui). Good first contributions: a new provider in the catalog, an extension, a theme, docs, or any [`good first issue`](https://github.com/HaseebKhalid1507/SynapsCLI/labels/good%20first%20issue).

```bash
git clone https://github.com/HaseebKhalid1507/SynapsCLI && cd SynapsCLI
cargo build && cargo test
```

See [CONTRIBUTING.md](CONTRIBUTING.md). Issues and PRs welcome, the maintainer answers fast.

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
  <sub>Because every other CLI agent was a 400MB Electron app pretending to be a terminal tool.</sub>
</p>
