# Pre-Phase 2 Checkpoint: Voice Decoupling

Date: 2026-05-01
Branch: `feat/phase-2-extensions-capability-platform`

## Goal

Remove the local voice/STT sidecar from Synaps CLI core and relocate it to the `synaps-skills` plugin repository so Phase 2 can build a generic extension capability platform instead of treating voice as foundational core behavior.

## Commit inventory from `c319e687a6f19d93e8f4882c90dde617aabe4f4e`

### Generic/reusable ideas

- Sidecar process lifecycle concepts: start, stop, shutdown, event stream.
- JSON line protocol shape for sidecar control/events.
- Capability metadata pattern: plugin manifest can declare a sidecar-like capability.
- Tests around sidecar protocol round-tripping.

### Voice-specific/plugin-owned implementation

- Microphone capture and audio buffer logic.
- VAD thresholds and utterance segmentation.
- Whisper model loading and transcription.
- Transcript sanitization and voice command mapping.
- Voice-specific binaries and config keys.

### Accidental core coupling removed here

- `src/voice/**` core module.
- `src/bin/synaps-voice-local.rs` and `src/bin/synaps-voice-mock.rs`.
- Core Cargo dependencies/features for `whisper-rs`, `cpal`, and `voice-*`.
- TUI `/voice` command, F8/Ctrl+Alt+V hardcoded behavior, and voice header state.
- Core tests for the local voice runtime.

## Contract direction for Phase 2

Core should expose generic extension contracts only:

- extension lifecycle and health/status
- registered tools, hooks, commands, providers
- generic process/sidecar supervision if needed
- generic capability metadata and diagnostics

Core should not expose voice as a built-in first-class subsystem. Voice can return later as a plugin-provided capability once the generic sidecar/capability contract exists.

## Voice plugin relocation

The voice implementation has been copied into a dedicated `synaps-skills` worktree:

- repo worktree: `/home/jr/Projects/Maha-Media/.worktrees/synaps-skills-local-voice-plugin`
- branch: `feat/local-voice-plugin`
- plugin path: `local-voice-plugin/`
- commit: `feat: add local voice sidecar plugin`

That plugin owns:

- `whisper-rs` and `cpal` dependencies
- audio capture
- VAD
- Whisper STT
- setup/build script
- plugin manifest metadata

## Verification performed

- Synaps CLI grep after revert found no `voice`, `Voice`, `whisper`, `vad`, `cpal`, or `stt` references in `Cargo.toml`, `src`, or `tests`.
- Local voice plugin test command passed:
  - `cd local-voice-plugin && cargo test`
  - result: 1 integration test passed; warnings only.

## Checkpoint

This checkpoint marks completion of pre-Phase 2 goals 1-5: the core voice dependency has been removed from Synaps CLI, and the voice sidecar implementation now lives in `synaps-skills` as a plugin-owned implementation path.
