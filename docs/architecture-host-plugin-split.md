# Architecture: Host / Plugin Split

`synaps-cli` hosts plugin processes and routes structured messages; the
plugin decides what they *mean*. A plugin author adds a tool, a provider,
a command or a settings panel without a change to core.

There is one way for a plugin to run its own process: an **extension**
(the `extension` block of `plugin.json`). The separate "sidecar" process
mechanism (`provides.sidecar`, `/sidecar`, `sidecar_toggle_key`) was
removed in 0.12.0; see `docs/upgrading/0.12.0.md`.

## The split

```
┌─────────────────────────────────┐   ┌─────────────────────────────────┐
│       APP (core)                │   │         PLUGIN                  │
├─────────────────────────────────┤   ├─────────────────────────────────┤
│  ChatUI                         │   │  Manifest (.synaps-plugin/      │
│  ├─ rendering, input editing    │   │            plugin.json)         │
│  └─ slash-command palette       │   │  ├─ name, version               │
│                                 │   │  ├─ extension (process, perms)  │
│  ExtensionManager               │   │  ├─ commands  (slash cmds)      │
│  ├─ load / unload / reload      │   │  ├─ settings  (panel defs)      │
│  ├─ leases, restart, idle stop  │   │  ├─ keybinds  (defaults)        │
│  ├─ RPC routing                 │   │  └─ help_entries                │
│  └─ plugin_info cache           │   │                                 │
│                                 │   │  Extension process              │
│  Hook bus / Tool registry /     │   │  ├─ tools, hooks, providers     │
│  Provider registry              │   │  ├─ command handlers            │
│                                 │   │  ├─ settings editor handlers    │
│  Settings storage, keybinds     │   │  └─ owns its config namespace   │
│  Permissions enforcement        │   │                                 │
│  Process transport (stdio,      │   │  Setup script (extension.setup) │
│  cleared environment)           │   │                                 │
└─────────────────────────────────┘   └─────────────────────────────────┘
                 ▲                                     ▲
                 └──────────── WIRE CONTRACT ──────────┘
```

## The wire contract

JSON-RPC over the extension's stdio. Hooks, permissions and manifest
rules are in `docs/extensions/contract.json`.

### App → Plugin (requests)

| Method                    | Returns                                             |
|---------------------------|-----------------------------------------------------|
| `initialize`              | `{ protocol_version, capabilities }`                |
| `info.get`                | `{ build, capabilities, models }`                   |
| `command.invoke`          | command result + streamed `command.output` notifs   |
| `settings.editor.open`    | render payload                                      |
| `settings.editor.key`     | render payload                                      |
| `settings.editor.commit`  | effect (config write / command invoke)              |
| `tool.call`               | tool result                                         |
| `hook.handle`             | hook result                                         |
| `provider.complete`       | LLM completion                                      |
| `provider.stream`         | streamed `provider.stream.event` notifs             |
| `shutdown`                | -                                                   |

### Plugin → App (notifications)

| Notification                          | Purpose                                  |
|---------------------------------------|------------------------------------------|
| `task.*`                              | background work progress                 |
| `widget.upsert` / `widget.dismiss`    | status widgets shown by the TUI          |
| `command.output`                      | stream from a `/foo` command invocation  |
| `provider.stream.event`               | LLM token stream                         |

## The boundary in one sentence

**The app hosts a process and pipes structured messages through it. The plugin decides what the process *means*.**
