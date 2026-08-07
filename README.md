<p align="center">
  <img src="assets/banner.png" alt="SynapsCLI" width="100%" />
</p>

<h3 align="center">Lightning fast terminal-native agent runtime</h3>

<p align="center">
  <a href="https://crates.io/crates/synaps"><img src="https://img.shields.io/crates/v/synaps?color=orange&label=crates.io" alt="crates.io"></a>
  <img src="https://img.shields.io/badge/rust-1.80%2B-orange.svg" alt="Rust">
  <a href="https://ratatui.rs/"><img src="https://ratatui.rs/built-with-ratatui/badge.svg" alt="Built With Ratatui" height="20"></a>
  <img src="https://img.shields.io/badge/binary-20MB-success.svg" alt="20MB binary">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="License"></a>
  <a href="https://discord.gg/JCdgRYqVDP"><img src="https://img.shields.io/badge/Discord-join%20the%20server-5865F2?logo=discord&logoColor=white" alt="Discord"></a>
</p>

<p align="center">
  Run AI agents from one binary. Tools, subagents, and extensions built in.<br>
  Any model, 20MB, 20ms cold start.
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
synaps login          # OAuth, cloud credentials, provider keys, or local Ollama
synaps                # launch the TUI
```

Headless, same engine, for scripts and CI:

```bash
echo "summarize the git diff" | synaps chat
```

### Sign in with what you already pay for, use cloud identity, or run locally

```bash
synaps login                      # searchable picker: OAuth, cloud, and API-key providers

# Subscription OAuth
synaps login --provider claude
synaps login --provider openai-codex
synaps login --provider xai-auth
synaps login --provider github-copilot
synaps login --provider google-gemini  # requires a Synaps-owned Google Desktop OAuth client registration

# Static API key
synaps login --provider kimi            # Moonshot AI (MOONSHOT_API_KEY / KIMI_API_KEY)

# Cloud identity; credentials remain broker-owned
synaps login --provider azure-openai
synaps login --provider aws-bedrock
synaps login --provider google-vertex

# Then choose an exact provider-qualified model
synaps                            # /model openai-codex/gpt-5.5

# Ollama or any local model (no account, no key, no cloud)
ollama serve                      # LM Studio, vLLM, llama.cpp all work too
synaps                            # /model local/llama3.2
```

OAuth and cloud credentials flow through the typed credential broker; long-lived tokens and cloud secrets are not handed to model runtimes. Availability still depends on each upstream account, entitlement, cloud registration, IAM policy, region, and quota. Cloud broker routes (Azure OpenAI, Amazon Bedrock, Google Vertex AI) are currently **text-only**: they are marked `text-only` in model listings, and a mode that requires tools fails with a typed unsupported-capability error before any credential use or network access.

Synaps auto-targets `http://localhost:11434/v1`, which is Ollama's default, so a running Ollama just works. Point it anywhere else with `provider.local.url` in config or the `LOCAL_ENDPOINT` env var. Your keys, your box, nothing phones home.

---

## What's in the box

- **🎭 Named agents.** Crew members with roles. Dispatch by name, watch them think in a live panel.
- **🔄 Steer them mid-flight.** Dispatch, poll, steer, inspect, collect, and explicitly reconcile. Redirect an agent while it is still working; bounded tombstones preserve the collection path after finished-worker cleanup.
- **🔐 Brokered provider identity.** Typed OAuth, cloud, and API-key identities survive login, model selection, routing, and status without stripping the provider. Long-lived credentials remain inside the broker boundary.
- **🧩 Policy-driven orchestration.** Exact model identities, typed roles, write scopes, concurrency limits, and lifecycle gates authorize workers before credentials, network, billing, threads, or worker state.
- **⚙️ Exact reasoning controls.** `/settings` and `/effort` expose only modes supported by the active provider-qualified model, including distinct Codex `xhigh`/`max`/`ultra` and Claude Fable 5 `max`/`ultracode` semantics.
- **🔌 Build anything on it.** Process-isolated extensions in any language, MCP servers, an event bus, custom tools. A small core with enough hooks to bolt on whatever you want and glue it to whatever you've got.
- **🌐 Native and cloud model transports.** Claude, ChatGPT/Codex, Grok/xAI, GitHub Copilot, Gemini Code Assist, Kimi (Moonshot AI), Azure OpenAI, Amazon Bedrock, and Google Vertex AI, plus OpenAI-compatible providers such as Groq, Cerebras, NVIDIA NIM, OpenRouter, or local Ollama. Provider and account support varies; unsupported routes fail closed.
- **🧠 Continuous memory.** Project-scoped persistent memory with sensitivity classes. Context, decisions, and entities survive across sessions automatically.
- **💰 Per-turn budgets.** Tool calls, wall clock, provider rounds, output tokens, result bytes, and cost — all bounded and logged per turn.
- **📡 Event bus.** Any script, cron, or service can poke a running session and the agent reacts in real time.
- **🧠 Context that lasts.** 90%+ prompt-cache hit rate. `/compact` checkpoints history. Chain sessions across days. Continuous memory persists across projects.
- **🤖 Autonomous mode.** `synaps watcher` runs a fleet with heartbeats, crash recovery, cost limits, and session handoff.
- **⚡ Fast and lean.** ~210K lines of Rust, one 20MB binary, ~2ms cold start, zero runtime deps.
- **🎨 19 themes.** `catppuccin`, `gruvbox`, `nord`, `rose-pine`, `dracula`, `tokyo-night`, plus originals like `neon-rain` and `night-city`. Hot-swap with `/theme`.

---

## Modes

| Command | What it does |
|---------|-------------|
| `synaps` | Interactive TUI: streaming, markdown, syntax highlighting, subagent panel |
| `synaps chat` | Headless, same engine, stdin/stdout. Scripts, pipes, CI |
| `synaps server` | WebSocket API: token auth, origin validation, streaming |
| `synaps rpc` | Line-JSON IPC for bridges (Slack, Discord) |
| `synaps watcher` | Supervisor daemon for autonomous agent fleets |
| `synaps auth-broker` | Credential broker for multi-machine shared auth |

## Configuration

No YAML. No TOML. No JSON. Just `key = value` in `~/.synaps-cli/config`:

```ini
model = anthropic/claude-sonnet-4-6
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

Synaps is young (started April 2026) and moving fast: 1,900+ commits in its first 3 months, shipped to crates.io, Homebrew, and the AUR, and listed in [awesome-ratatui](https://github.com/ratatui/awesome-ratatui). Good first contributions: a new provider in the catalog, an extension, a theme, docs, or any [`good first issue`](https://github.com/HaseebKhalid1507/SynapsCLI/labels/good%20first%20issue).

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
├── agent-core/      # provider identity, auth broker, models, prompts, orchestration policy
├── agent-engine/    # provider transports, runtime, worker lifecycle, tools, extensions
│                   # MCP, events, skills, sidecar, cloud and OpenAI-compatible routing
├── agent-tui/       # ratatui UI, model/effort settings, themes, plugin modals
└── (root)           # the `synaps` binary crate: CLI dispatch, login, watcher, broker
```
Native Anthropic, OpenAI Responses/chat, Gemini Code Assist, and cloud-provider transports all emit the same `StreamEvent`, so the TUI and tool loop stay provider-blind. Provider-qualified identities and pre-authorized execution plans remain typed through routing.
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
