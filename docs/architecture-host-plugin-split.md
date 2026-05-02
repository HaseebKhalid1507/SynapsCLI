# Architecture: Host / Plugin Split

After Phase 7 ("Deassume the Host"), `synaps-cli` is modality-agnostic:
the host hosts a process and routes structured events; the plugin
decides what the process *means*. A community plugin author writing
an OCR sidecar, an agent runner, a foot-pedal trigger, an EEG
dictation source, or a clipboard mirror has no need to PR core to
add their kind.

## The split

```
╔═══════════════════════════════════════════════════════════════════════════════╗
║                         synaps-cli  ◄──► plugin sidecar                       ║
║                       (the host)             (the guest)                      ║
╠═══════════════════════════════════════════════════════════════════════════════╣
║                                                                               ║
║   ┌─────────────────────────────────┐   ┌─────────────────────────────────┐   ║
║   │       APP (core)                │   │         PLUGIN                  │   ║
║   │       synaps-cli                │   │   e.g. local-voice-plugin       │   ║
║   ├─────────────────────────────────┤   ├─────────────────────────────────┤   ║
║   │                                 │   │                                 │   ║
║   │  ChatUI                         │   │  Manifest (.synaps-plugin/      │   ║
║   │  ├─ rendering, viewport         │   │            plugin.json)         │   ║
║   │  ├─ input editing               │   │  ├─ name, version               │   ║
║   │  ├─ slash-command palette       │   │  ├─ provides.sidecar.command    │   ║
║   │  └─ /sidecar toggle | status    │   │  ├─ commands  (slash cmds)      │   ║
║   │                                 │   │  ├─ settings  (panel defs)     │   ║
║   │  Sidecar lifecycle              │   │  ├─ keybinds  (defaults)        │   ║
║   │  ├─ discover() — walks          │   │  ├─ help_entries                │   ║
║   │  │  manifests                   │   │  └─ permissions                 │   ║
║   │  ├─ spawn / supervise process   │   │                                 │   ║
║   │  ├─ press / release             │   │  Sidecar binary                 │   ║
║   │  └─ build_spawn_args (combines  │   │  ├─ audio capture / VAD         │   ║
║   │     plugin args + manifest      │   │  ├─ STT model load & inference  │   ║
║   │     default model fallback)     │   │  ├─ wake-word, hotword, etc.    │   ║
║   │                                 │   │  └─ owns plugin config          │   ║
║   │  ExtensionManager               │   │     namespace reads             │   ║
║   │  ├─ load / unload / reload      │   │                                 │   ║
║   │  ├─ RPC routing                 │   │  Slash-command handlers         │   ║
║   │  ├─ capability snapshot store   │   │  (/voice download, /voice       │   ║
║   │  └─ plugin_info cache           │   │   models, /voice ...)           │   ║
║   │                                 │   │                                 │   ║
║   │  Hook bus / Tool registry /     │   │  Settings editor handlers       │   ║
║   │  Provider registry              │   │  (custom pickers, downloaders)  │   ║
║   │                                 │   │                                 │   ║
║   │  Settings storage (TOML)        │   │  Setup script                   │   ║
║   │  ├─ sidecar_toggle_key          │   │  └─ scripts/setup.sh            │   ║
║   │  └─ migrate_legacy_*            │   │                                 │   ║
║   │     (one-release shims)         │   │  Capability declarations        │   ║
║   │                                 │   │  ├─ kind: "voice"  (free-form)  │   ║
║   │  Keybind registry               │   │  ├─ name, modes                 │   ║
║   │  └─ /sidecar toggle default     │   │  └─ permissions                 │   ║
║   │                                 │   │                                 │   ║
║   │  Permissions enforcement        │   │  Build info                     │   ║
║   │                                 │   │  └─ backend, features, version  │   ║
║   │  Process transport              │   │                                 │   ║
║   │  └─ JSONL framed over stdio     │   │  All "voice" knowledge lives    │   ║
║   │                                 │   │  here — never escapes the       │   ║
║   │  ❌ NO modality knowledge       │   │  plugin boundary.                │   ║
║   │  ❌ NO hardcoded plugin names   │   │                                 │   ║
║   │  ❌ NO enumerated capability    │   │                                 │   ║
║   │     kinds (stt/tts/wake_word)   │   │                                 │   ║
║   └─────────────────────────────────┘   └─────────────────────────────────┘   ║
║                  ▲                                       ▲                    ║
║                  │                                       │                    ║
║                  └──────── WIRE CONTRACT ────────────────┘                    ║
║                                                                               ║
╚═══════════════════════════════════════════════════════════════════════════════╝
```

## The wire contract

Transport: **JSONL over stdio, `Content-Length`-framed messages.**

### Plugin → App (RPC responses)

| Method                    | Returns                                             | Phase |
|---------------------------|-----------------------------------------------------|-------|
| `initialize`              | `{ protocol_version, capabilities[] }`              | 1     |
| `info.get`                | `{ build, capabilities, models }`                   | 5     |
| `sidecar.spawn_args`      | `{ args[], language? }`                             | 7-F   |
| `command.invoke`          | command result + streamed `command.output` notif    | 6     |
| `settings.editor.open`    | render payload                                      | 5     |
| `settings.editor.key`     | render payload                                      | 5     |
| `settings.editor.commit`  | effect (config write / command invoke)              | 5     |
| `tool.call`               | tool result                                         | 1     |
| `provider.complete`       | LLM completion                                      | 4     |
| `provider.stream`         | streamed `provider.stream.event` notifs             | 4     |
| `shutdown`                | -                                                   | 1     |

### Plugin → App (notifications)

| Notification                          | Purpose                                  |
|---------------------------------------|------------------------------------------|
| `state_changed`                       | sidecar lifecycle state                  |
| `listening_started` / `_stopped`      | trigger-driven session boundaries        |
| `transcribing_started`                | streaming output begins                  |
| `final_transcript` / `partial_*`      | content payloads                         |
| `task.*`                              | background work progress                 |
| `config.changed`                      | plugin saw its own config update         |
| `command.output`                      | stream from a `/foo` command invocation  |
| `provider.stream.event`               | LLM token stream                         |

### App → Plugin (sidecar commands)

| Command       | Meaning                                                |
|---------------|--------------------------------------------------------|
| `press`       | trigger on  (modality-neutral; was "voice control")    |
| `release`     | trigger off                                            |
| `set_mode`    | switch session mode (dictation/command/...)            |
| `shutdown`    | graceful exit                                          |

## What changed in Phase 7

| Before                                                  | After                                                  |
|---------------------------------------------------------|--------------------------------------------------------|
| `voice: Option<VoiceCapabilityDeclaration>` typed slot  | `capabilities: Vec<CapabilityDeclaration>` (free-form `kind`) |
| `validate_voice_capability` hardcoded stt/tts/wake_word | `validate_capability` — gates only on declared permissions |
| `src/voice/` module                                     | `src/sidecar/` (modality-neutral)                      |
| `VoiceManager`, `VoiceUiState`, `Category::Voice`       | `SidecarManager`, `SidecarUiState`, `Category::Sidecar`|
| `voice_toggle_key` config                               | `sidecar_toggle_key` (with one-release alias)          |
| `/voice` builtin                                        | `/sidecar toggle` builtin; `/voice` is plugin-owned    |
| `read_local_voice_setting("local-voice", ...)` in core  | `sidecar.spawn_args` RPC — plugin self-configures      |
| `provides.voice_sidecar` manifest field                 | `provides.sidecar` (with `voice_sidecar` serde alias)  |

## The boundary in one sentence

**The app hosts a process and pipes structured events through it. The plugin decides what the process *means*.**

## What this enables

You can now write a plugin that:

- Declares `kind: "ocr"` in its capability declaration → core groups it under "ocr" in `/extensions` without any change to core.
- Provides a `provides.sidecar.command` that points at an OCR daemon → core spawns it with the same lifecycle code that hosts voice.
- Implements `sidecar.spawn_args` to supply its own model paths and language settings → core never reads the plugin's config namespace.
- Owns `/ocr scan`, `/ocr clip`, etc. as plugin slash commands → core dispatches them generically.
- Defines its own settings panel under `Category::Sidecar` (or any other category) → core renders it.

No changes to core. No PRs upstream. No "voice" appearing in the plugin's RPC payloads or settings keys.
