# Architecture

```
crates/
├── agent-core/      # provider identity, auth broker, models, prompts, orchestration policy
├── agent-engine/    # provider transports, runtime, session daemon, worker lifecycle, tools, extensions
│                   # MCP, events, skills, sidecar, cloud and OpenAI-compatible routing
├── agent-tui/       # ratatui UI, model/effort settings, themes, plugin modals
└── (root)           # the `synaps` binary crate: CLI dispatch, login, watcher, broker
```

The session daemon hosts one `EngineHost` (extensions, MCP, and providers shared
across sessions) and a `SessionActor` per session. The TUI, `synaps attach`, and
headless clients connect to it over a Unix socket. Native Anthropic, OpenAI
Responses/chat, Gemini Code Assist, and cloud-provider transports all emit the
same `StreamEvent`, so the TUI and the tool loop stay provider-blind.
Provider-qualified identities and pre-authorized execution plans stay typed
through routing.

Deeper references:

- [docs/daemon-mode.md](docs/daemon-mode.md): the daemon, its protocol, sessions, reload and pins.
- [docs/architecture-host-plugin-split.md](docs/architecture-host-plugin-split.md): how the host and plugin halves divide responsibility.
- [docs/petri-net-model.md](docs/petri-net-model.md): a Petri-net model of a session, a turn, a tool batch and the prompt cache.
- [docs/README.md](docs/README.md): every reference doc.
