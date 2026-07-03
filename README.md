<p align="center">
  <img src="assets/banner.png" alt="SynapsCLI" width="100%" />
</p>

<h3 align="center">The agent runtime that boots before your Node binary finishes importing.</h3>

<p align="center">
  <a href="https://github.com/HaseebKhalid1507/SynapsCLI/stargazers"><img src="https://img.shields.io/github/stars/HaseebKhalid1507/SynapsCLI?style=flat&color=yellow" alt="Stars"></a>
  <a href="https://crates.io/crates/synaps"><img src="https://img.shields.io/crates/d/synaps?color=orange&label=installs" alt="Downloads"></a>
  <img src="https://img.shields.io/badge/rust-1.80%2B-orange.svg" alt="Rust">
  <a href="https://ratatui.rs/"><img src="https://ratatui.rs/built-with-ratatui/badge.svg" alt="Built With Ratatui" height="20"></a>
  <img src="https://img.shields.io/badge/binary-15MB-success.svg" alt="15MB binary">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="License"></a>
</p>

<p align="center">
  A single 15MB Rust binary. No Python. No <code>node_modules</code>. No 400MB Electron shell.<br>
  Just a fast, multi-agent runtime that lives in your terminal.<br><br>
  <a href="https://github.com/HaseebKhalid1507/SynapsCLI/wiki"><b>📖 Wiki</b></a> · <a href="https://github.com/HaseebKhalid1507/SynapsCLI/wiki/Installation"><b>⚡ Quick Start</b></a> · <a href="ELI5.md"><b>🧒 ELI5</b></a> · <a href="#-benchmarks"><b>📊 Benchmarks</b></a>
</p>

---

<!-- TODO: replace with the hero demo GIF (see task #115) — single strong view, high contrast, app not shell -->
<p align="center">
  <img src="assets/demo.gif" alt="SynapsCLI Demo" width="720" />
</p>

---

## Why this exists

Every other agent CLI is a Python framework that adds 250ms of overhead per call, or a Node tool that drags a `node_modules` the size of a small OS, or an Electron app pretending to be a terminal tool. They're slow to start, heavy to install, and they abstract so much you can't see what your agent is actually doing.

**Synaps is the opposite.** One compiled Rust binary. It starts in ~2ms, ships as 15MB, runs any model from any provider, and treats agents like what they are — **autonomous programs**, not chat windows. Multiple named agents live in it, work in parallel, and persist across sessions.

If you've ever been annoyed that "an AI CLI" means a heavyweight runtime, this is for you.

---

## 📊 Benchmarks

Cold-start / runtime overhead — the tax every tool pays *before it does any work*:

| command | median | |
|---|---|---|
| `synaps --version` (Rust binary) | **2.3 ms** | |
| `python3 -c pass` (bare interpreter) | 30.2 ms | ~13× slower, importing nothing |
| `node -e ''` (bare interpreter) | 46.1 ms | ~20× slower, importing nothing |

**synaps fully starts before a Python or Node interpreter finishes launching** — before a single dependency is imported. A LangChain/CrewAI import stacks *hundreds* of ms on top of that floor.

- **15MB single static binary.** No `node_modules`, no venv, no runtime.
- *Methodology:* 30 runs each, 3 warmups, median reported, `time.perf_counter` around `subprocess.run`. Reproduce with the three commands above. This measures **runtime/startup overhead**, not end-to-end agent latency (that's model-bound — identical across runtimes). The point: the runtime itself adds ~nothing.

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

## 60-second start

```bash
synaps login                            # OAuth with Claude Pro/Max …
export GROQ_API_KEY="gsk_..."           # … or any provider key (free tiers work)
synaps                                   # launch the TUI

# headless — same engine, for scripts/CI/pipes:
echo "summarize the git diff" | synaps chat
```

Set a key, pick a model, go. Anthropic is native; 17 providers / 55+ models route through an OpenAI-compatible layer — swap mid-session with `/model`.

### Sign in with what you already pay for — or nothing at all

**Claude Pro/Max** (OAuth, no API key):
```bash
synaps login                      # or non-interactive: synaps login --provider claude
```

**ChatGPT Plus/Pro** (OAuth via Codex):
```bash
synaps login --provider openai-codex
synaps                            # then: /model openai-codex/gpt-5.5
```

**Ollama / local models** (no account, no key, no cloud):
```bash
ollama serve                      # LM Studio, vLLM, llama.cpp all work too
synaps                            # then: /model local/llama3.2
```

Synaps auto-targets `http://localhost:11434/v1` — Ollama's default — so a running Ollama just works. Point elsewhere with `provider.local.url` in config or the `LOCAL_ENDPOINT` env var.

---

## What it looks like

```
╭ ◈ 4 agents ────────────────────────────────────╮
│  ✓ spike    done                         12.3s  │
│  ⠹ chrollo  ⚙ read (tool #5)              8.1s  │
│  ✓ shady    done                          9.7s  │
│  ⠹ zero     thinking...                   4.2s  │
╰─────────────────────────────────────────────────╯
```

You dispatch named agents. They work in parallel. You watch them think — and steer them mid-flight.

```bash
subagent(agent: "spike", task: "refactor the auth module")        # dispatch + wait

subagent_start(agent: "chrollo", task: "audit this codebase")     # dispatch reactive
subagent_steer(handle_id: "sa_1", message: "focus on the API routes")  # redirect mid-run
subagent_collect(handle_id: "sa_1")                                # gather when ready
```

Agents aren't anonymous forks — they're crew members with names, system prompts, specializations, and memory. You build a team, not a chatbot. A **watcher** daemon supervises the fleet so they don't crash or blow your budget.

*New to agents? → [ELI5](ELI5.md). Full tour → [Wiki](https://github.com/HaseebKhalid1507/SynapsCLI/wiki) (36 pages).*

---

## What's in the box

- **⚡ Fast & lean.** ~87K lines of Rust across 3 library crates + a binary crate, 15MB single binary, ~2ms cold start, zero runtime deps.
- **🌐 Multi-provider.** Anthropic-native + an OpenAI-compatible layer for 17 providers / 55+ models (Groq, Cerebras, NVIDIA NIM, OpenRouter, …) incl. free tiers. `/model` to swap.
- **🎭 Named agents.** Crew members with souls — dispatch by name, watch in the live panel.
- **🔄 Reactive orchestration.** dispatch → poll → **steer** → collect. Multi-agent workflows, not fire-and-forget.
- **🔌 Process-isolated extensions.** JSON-RPC 2.0 over stdio — language-agnostic, crash-isolated, sandboxed. Hook 7 lifecycle events to add guardrails, memory, context injection, anything.
- **📡 Event bus.** Any script/cron/service can poke a running session — the agent reacts in real time.
- **🧠 Context that lasts.** 90%+ prompt-cache hit rate; `/compact` checkpoints history; chain sessions across days.
- **🤖 Autonomous mode.** `synaps watcher` — heartbeats, crash recovery, cost limits, session handoff.
- **🎨 18 themes.** `catppuccin`, `gruvbox`, `nord`, `rose-pine`, `dracula`, `tokyo-night`… plus originals like `neon-rain` and `night-city`. Hot-swap with `/theme`.

> **Honest scope:** Synaps is Anthropic-first today — the Anthropic path has the deepest feature support (caching, retry, cost). The OpenAI-compatible path covers everything else and is actively being brought to full parity ([tracking](docs/open-provider-issues.md)). We'd rather tell you that than overclaim.

---

## Modes

| Command | What it does |
|---------|-------------|
| `synaps` | Interactive TUI — streaming, markdown, syntax highlighting, subagent panel |
| `synaps chat` | Headless — same engine, stdin/stdout. Scripts, pipes, CI |
| `synaps server` | WebSocket API — token auth, origin validation, streaming |
| `synaps rpc` | Line-JSON IPC — for bridges (Slack, Discord) |
| `synaps watcher` | Supervisor daemon for autonomous agent fleets |

## Configuration

No YAML. No TOML. No JSON. `key = value` in `~/.synaps-cli/config`:

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

See the [Wiki](https://github.com/HaseebKhalid1507/SynapsCLI/wiki) for the full config reference, the multi-machine **auth broker** (share one OAuth credential across a fleet over WireGuard/TLS), and the bridge heartbeat mirror.
</details>

---

## Extensions

Drop a folder in `~/.synaps-cli/plugins/` — live on next boot:

```
~/.synaps-cli/plugins/my-guard/
├── .synaps-plugin/plugin.json    # manifest: hooks, permissions, keybinds
└── main.py | index.js | <any>    # JSON-RPC 2.0 over stdio — any language
```

Because extensions are **separate processes** (not linked code), they're language-agnostic, crash-isolated, and sandboxed. Hook `before_tool_call`, `before_message`, `on_session_start`, and 4 more. Real example: **finlens** — a 6-lens finance research workflow built entirely as an extension, no core changes.

Protocol spec: [docs/extensions/](docs/extensions/).

---

## Contributing

Synaps is young (started April 2026) and moving fast — **1,300+ commits in its first ~3 months.** Good first contributions: a new provider in the catalog, an extension, a theme, docs, or any [`good first issue`](https://github.com/HaseebKhalid1507/SynapsCLI/labels/good%20first%20issue).

```bash
git clone https://github.com/HaseebKhalid1507/SynapsCLI && cd SynapsCLI
cargo build && cargo test
```

See [CONTRIBUTING.md](CONTRIBUTING.md). Issues and PRs welcome — the maintainer answers fast.

---

## Philosophy

- **Agents are not chat.** Autonomous programs that happen to use language models. Treat them like services.
- **Speed is a feature.** A 2-second boot already lost the dev who wanted it in a git hook.
- **Multi-agent is the default.** Single-agent is just n=1.
- **The terminal is the IDE.** If you need Electron to be productive, your tools are wrong.

<details>
<summary><b>Architecture</b></summary>

```
crates/
├── agent-core/      # identity, models, auth, pricing
├── agent-engine/    # LLM runtime (Anthropic-native + OpenAI-compat router),
│                    #   tools, extensions, MCP, events, skills, sidecar
├── agent-tui/       # ratatui terminal UI, themes, settings, plugin modals
└── (root)           # the `synaps` binary crate — CLI dispatch, watcher, broker
```
Two API paths (Anthropic native + OpenAI-compatible for 17 providers) both emit the same `StreamEvent` — the TUI and tool loop are provider-blind.
</details>

---

## License

Apache 2.0. See [LICENSE](LICENSE).

<p align="center">
  <sub>Because every other CLI agent was a 400MB Electron app pretending to be a terminal tool.</sub>
</p>
