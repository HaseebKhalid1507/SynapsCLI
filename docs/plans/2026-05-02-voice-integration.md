# Voice Integration Plan

Date: 2026-05-02
Branches:
- synaps-cli: `feat/voice-integration` (off `feat/phase-2-extensions-capability-platform`)
- synaps-skills: `feat/local-voice-plugin` (existing, off `main`)

## Goal

User-facing voice dictation in Synaps CLI, with the actual speech-to-text running entirely inside the `local-voice-plugin` sidecar from `synaps-skills`. Synaps CLI gains:

- `/voice` slash command (`toggle`, `status`)
- Default toggle key `F8`, configurable to `Ctrl+V` / `Ctrl+Shift+V` / custom via Settings
- "Voice" section in the Settings panel
- Listening indicator pill in the chat header
- Transcripts inserted directly into the input buffer
- Sidecar discovery via plugin manifest `provides.voice_sidecar.command`

## Architecture

```
+-------------------------------+      line-JSON over      +---------------------------------+
| synaps-cli (this branch)      |  <-- stdio  -->          | local-voice-plugin sidecar      |
|                               |                          |                                 |
|  src/voice/                   |   SidecarCommand→        |  src/main.rs (already exists)   |
|    protocol.rs (port from PG) |   ←SidecarEvent          |  src/audio.rs                   |
|    manager.rs (lifecycle)     |                          |  src/vad.rs                     |
|    state.rs (Idle/Listening)  |                          |  src/stt_whisper.rs             |
|                               |                          |                                 |
|  src/chatui/commands.rs       |                          |  Cargo features:                |
|    /voice toggle/status       |                          |    default = mock-only          |
|                               |                          |    local-stt = mic + whisper    |
|  src/chatui/settings/         |                          |                                 |
|    Voice section              |                          |  --mock-transcript "hello"      |
|                               |                          |  --model-path ~/...             |
|  src/skills/manifest.rs       |                          |                                 |
|    + provides.voice_sidecar   |                          |                                 |
+-------------------------------+                          +---------------------------------+
```

Voice **data plane** = line-JSON over stdio (simple, low-latency, plugin already speaks it).
Voice **metadata** = Phase 2 `VoiceCapabilityDeclaration` from Slice V (informational, surfaced in `/extensions status`).

Sidecar protocol types live in `synaps-cli/src/voice/protocol.rs` (the pre-Phase 2 checkpoint flagged
"sidecar lifecycle + line-JSON" as generic/reusable for core).

## Slices

| # | Where | What | Tests |
|---|---|---|---|
| V1 | synaps-cli | Extend `PluginManifest` with `provides.voice_sidecar` | manifest deserialize unit test |
| V2 | synaps-cli | `src/voice/protocol.rs` — `SidecarCommand`, `SidecarEvent` types | round-trip serde |
| V3 | synaps-cli | `src/voice/manager.rs` — spawn/shutdown sidecar, line-JSON IO | spawn `--mock-transcript`, await `FinalTranscript` |
| V4 | synaps-cli | `/voice toggle` & `/voice status` slash command | command dispatch test |
| V5 | synaps-cli | Wire `FinalTranscript` into chatui input buffer | integration test with mock |
| V6 | synaps-cli | Settings: Voice section (enable, toggle-key dropdown, language) | section render snapshot |
| V7 | synaps-cli | Listening indicator pill in chat header | render snapshot |
| V8 | both | Cross-repo smoke test — install plugin, configure, F8 mock toggle works | manual smoke + scripted test |
| V9 | both | Docs + CHANGELOG | — |

## Constraints

- CI must build with **default features only** in synaps-cli (no whisper/cpal/clang).
- Tests use the plugin's `--mock-transcript` mode (already supported, no audio deps required).
- Real-mic verification is a manual gate, not a CI gate.
- Toggle mode only (no push-to-talk in this round).

## Key bindings

`KeybindRegistry` already parses `F8`, `C-V`, `C-S-V` etc. Default keybind ships in
`local-voice-plugin/.synaps-plugin/plugin.json` (`F8` → `slash_command voice toggle`).
User overrides via `~/.synaps/agent/settings.json` `keybinds.voice toggle = "C-V"`.
Settings UI is a thin wrapper around the same registry.

## Out of scope

- Push-to-talk
- TTS / wake-word (declared in Slice V capability metadata, not implemented)
- Per-session microphone consent dialog
- Streaming partial-transcript display in the input box (only final transcripts inserted)
