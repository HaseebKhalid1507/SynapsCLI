# Voice Sidecar Extension Plan

## Status

Planning document. No implementation has been started from this plan.

## Decisions Locked

```yaml
convergence: none
execution: task-based
extension_model: sidecar process
transport: JSONL over stdin/stdout
core_default: lightweight extension host
heavy_voice_stack: sidecar-owned
```

## Goal

Keep SynapsCLI core lightweight while supporting an opencode-style voice mode through the new hook / extension system.

Core owns:

```text
voice extension protocol
sidecar lifecycle
event routing
TUI status
config
security/privacy boundaries
```

Sidecar owns:

```text
mic capture
VAD
STT provider
TTS provider
audio playback
barge-in detection
platform audio dependencies
model paths
```

This avoids making the main `synaps` binary depend on heavy/native voice libraries by default.

---

## Architecture Decision Record

### Decision 1: Voice runs as a sidecar process

Accepted.

```text
synaps
  └─ spawns/communicates with synaps-voice-local
```

### Decision 2: Transport is JSONL over stdio

Accepted as initial implementation target.

```text
stdin  = core → sidecar JSONL messages
stdout = sidecar → core JSONL messages
stderr = diagnostics only
```

Important rule:

> Sidecar must never write raw audio or transcripts to stderr/stdout except structured protocol messages. Core must not log transcripts unless explicit debug config is enabled.

### Decision 3: Protocol is versioned immediately

Example:

```json
{"type":"hello","protocol_version":1,"extension":"synaps-voice-local","capabilities":["stt"]}
```

This prevents compatibility pain as the extension system evolves.

---

## Proposed Sidecar Protocol

### Core → Sidecar

```json
{"type":"init","config":{"mode":"dictation","language":"en"}}
{"type":"voice_control_pressed"}
{"type":"voice_control_released"}
{"type":"assistant_chunk","text":"Hello"}
{"type":"assistant_completed","text":"Hello world."}
{"type":"stop_tts"}
{"type":"shutdown"}
```

### Sidecar → Core

```json
{"type":"hello","protocol_version":1,"extension":"synaps-voice-local","capabilities":["stt"]}
{"type":"status","state":"ready","capabilities":["stt","tts","barge_in"]}
{"type":"listening_started"}
{"type":"listening_stopped"}
{"type":"transcribing_started"}
{"type":"partial_transcript","text":"open cargo"}
{"type":"final_transcript","text":"open Cargo.toml"}
{"type":"voice_command","command":"submit"}
{"type":"tts_started"}
{"type":"tts_finished"}
{"type":"barge_in"}
{"type":"error","message":"Whisper model missing"}
```

---

## Dependency Graph

```text
Sidecar protocol types
    ↓
Mock sidecar
    ↓
Sidecar process host
    ↓
Sidecar-backed voice config
    ↓
TUI/event bridge
    ↓
Local voice sidecar shell
    ↓
Mic + VAD in sidecar
    ↓
Whisper STT in sidecar
    ↓
Assistant-output/TTS hooks
    ↓
Kokoros TTS in sidecar
    ↓
Barge-in + conversation mode
```

---

# Phase 1: Sidecar Skeleton

## Task 1: Define sidecar voice protocol

**Description:**  
Create serializable Rust message types for the voice sidecar protocol.

**Acceptance criteria:**
- [ ] Protocol has versioned hello/init messages.
- [ ] Core-to-sidecar messages include:
  - `Init`
  - `VoiceControlPressed`
  - `VoiceControlReleased`
  - `AssistantChunk`
  - `AssistantCompleted`
  - `StopTts`
  - `Shutdown`
- [ ] Sidecar-to-core messages include:
  - `Hello`
  - `Status`
  - `ListeningStarted`
  - `ListeningStopped`
  - `TranscribingStarted`
  - `PartialTranscript`
  - `FinalTranscript`
  - `VoiceCommand`
  - `TtsStarted`
  - `TtsFinished`
  - `BargeIn`
  - `Error`
- [ ] Types derive `Serialize`, `Deserialize`, `Debug`, `Clone`, `PartialEq`.
- [ ] JSON round-trip tests exist.
- [ ] No voice provider dependencies are added.

**Verification:**
- [ ] `cargo test voice_sidecar_protocol --lib`

**Dependencies:** None

**Likely files:**
- `src/voice/sidecar_protocol.rs`
- `src/voice/mod.rs`

**Scope:** S

---

## Task 2: Add mock sidecar binary for tests/dev

**Description:**  
Add a lightweight mock sidecar that speaks the protocol and emits deterministic responses.

Example behavior:

```text
stdin:  {"type":"init",...}
stdout: {"type":"hello","protocol_version":1,...}
stdout: {"type":"status","state":"ready","capabilities":["stt"]}
stdin:  {"type":"voice_control_pressed"}
stdout: {"type":"listening_started"}
stdin:  {"type":"voice_control_released"}
stdout: {"type":"transcribing_started"}
stdout: {"type":"final_transcript","text":"mock transcript"}
```

**Acceptance criteria:**
- [ ] Mock sidecar can run as a binary or test fixture.
- [ ] It reads JSONL from stdin.
- [ ] It writes JSONL protocol messages to stdout.
- [ ] It exits cleanly on `Shutdown`.
- [ ] It has no STT/TTS deps.

**Verification:**
- [ ] `cargo run --bin synaps-voice-mock` manually works.
- [ ] `cargo test voice_sidecar_mock --lib` or integration equivalent.

**Dependencies:** Task 1

**Likely files:**
- `src/bin/synaps-voice-mock.rs` or `tests/fixtures/voice_mock.rs`
- `src/voice/sidecar_protocol.rs`

**Scope:** S

---

## Task 3: Implement sidecar process host

**Description:**  
Add a core-side manager that spawns the configured sidecar, sends protocol messages, receives events, and exposes them as internal app events.

**Acceptance criteria:**
- [ ] Host can spawn a sidecar command.
- [ ] Host sends `Init`.
- [ ] Host reads stdout JSONL asynchronously.
- [ ] Host writes stdin JSONL asynchronously.
- [ ] Sidecar crashes produce a recoverable `VoiceEvent::Error`.
- [ ] Shutdown terminates child cleanly.
- [ ] No TUI blocking.

**Verification:**
- [ ] `cargo test voice_sidecar_host --lib`
- [ ] Manual with mock sidecar:
  ```bash
  synaps configured with synaps-voice-mock
  /voice
  F8 press/release
  mock transcript appears
  ```

**Dependencies:** Tasks 1-2

**Likely files:**
- `src/voice/sidecar_host.rs`
- `src/voice/mod.rs`
- existing voice runtime wiring

**Scope:** M

---

## Task 4: Add sidecar-backed voice provider mode to config

**Description:**  
Allow voice provider selection via config.

Suggested config:

```toml
[voice]
enabled = false
provider = "sidecar"

[voice.sidecar]
command = "synaps-voice-local"
args = []
restart_on_crash = false
protocol_version = 1
```

Suggested provider enum:

```rust
enum VoiceProviderKind {
    Builtin,
    Sidecar,
    Disabled,
}
```

**Acceptance criteria:**
- [ ] Config can select disabled/builtin/sidecar.
- [ ] Missing sidecar command produces user-visible voice error.
- [ ] Sidecar is not spawned until voice is enabled or triggered.
- [ ] Mic still never opens unless voice is explicitly used.
- [ ] Existing config remains backward-compatible.

**Verification:**
- [ ] `cargo test voice_config --lib`
- [ ] Manual invalid sidecar command shows an error, not panic.

**Dependencies:** Task 3

**Likely files:**
- config modules/files
- `src/voice/types.rs`
- `src/voice/sidecar_host.rs`

**Scope:** M

---

## Task 5: Bridge sidecar events into current TUI voice pipeline

**Description:**  
Map sidecar protocol events to existing internal `VoiceEvent` behavior.

Mapping:

```text
SidecarEvent::FinalTranscript(text)
  → VoiceEvent::FinalTranscript(text)

SidecarEvent::PartialTranscript(text)
  → VoiceEvent::PartialTranscript(text)

SidecarEvent::VoiceCommand(command)
  → existing command routing

SidecarEvent::BargeIn
  → future hook, initially stop TTS/no-op if unsupported

SidecarEvent::Error(message)
  → app.voice.last_error
```

**Acceptance criteria:**
- [ ] Mock sidecar final transcript inserts into input.
- [ ] Mock sidecar command can trigger existing submit behavior.
- [ ] Status/error events update app voice state.
- [ ] Existing F8 press/release behavior is preserved.
- [ ] Existing voice tests still pass.

**Verification:**
- [ ] `cargo test voice --lib`
- [ ] `cargo test voice_input --bin synaps`
- [ ] `cargo test voice_keybind_tests --bin synaps`

**Dependencies:** Tasks 3-4

**Likely files:**
- `src/chatui/mod.rs`
- `src/chatui/app.rs`
- `src/voice/types.rs`
- `src/voice/sidecar_host.rs`

**Scope:** M

---

## Checkpoint 1: Sidecar skeleton complete

After Tasks 1-5:

- [ ] Core can run without heavy voice dependencies.
- [ ] Mock sidecar can emit transcript.
- [ ] Transcript reaches current input pipeline.
- [ ] F8 still works.
- [ ] `/voice` still works.
- [ ] No mic opens on app start.
- [ ] Human review before migrating local STT.

---

# Phase 2: Move Local Voice into Sidecar

## Task 6: Create `synaps-voice-local` sidecar shell

**Description:**  
Add a real local voice sidecar binary that speaks the protocol but initially uses stubbed STT/TTS.

**Acceptance criteria:**
- [ ] Binary starts and emits `Hello`.
- [ ] Binary emits `Status { ready }`.
- [ ] It responds to press/release events.
- [ ] It can emit a configured fake transcript for smoke testing.
- [ ] It shares protocol types with core.

**Verification:**
- [ ] `cargo run --bin synaps-voice-local -- --mock-transcript "hello"`
- [ ] Manual with SynapsCLI using sidecar provider.

**Dependencies:** Checkpoint 1

**Likely files:**
- `src/bin/synaps-voice-local.rs`
- `src/voice/sidecar_protocol.rs`
- maybe `src/voice/local_sidecar.rs`

**Scope:** S/M

---

## Task 7: Move/wrap mic + VAD in local sidecar

**Description:**  
Port current mic capture and VAD control into `synaps-voice-local`.

**Acceptance criteria:**
- [ ] Sidecar opens mic only after `VoiceControlPressed`.
- [ ] Sidecar stops capture on `VoiceControlReleased`.
- [ ] VAD emits utterance boundaries.
- [ ] No raw audio is logged.
- [ ] Sidecar emits listening/transcribing status messages.

**Verification:**
- [ ] Targeted unit tests for VAD still pass.
- [ ] Manual: run sidecar, press/release from core, status updates.

**Dependencies:** Task 6

**Likely files:**
- existing mic/VAD modules
- local sidecar binary/module

**Scope:** M

---

## Task 8: Move/wrap whisper STT in local sidecar

**Description:**  
Wire existing `whisper-rs` STT into the sidecar.

**Acceptance criteria:**
- [ ] Captured utterance transcribes locally.
- [ ] Final transcript emits via sidecar protocol.
- [ ] Model missing emits protocol `Error`.
- [ ] Whisper model is loaded lazily or sidecar-start configurable.
- [ ] Transcript is not logged by default.

**Verification:**
- [ ] `cargo test voice --lib`
- [ ] Manual live mic test:
  ```text
  /voice
  F8
  speak
  F8
  transcript appears
  ```

**Dependencies:** Task 7

**Likely files:**
- `src/voice/stt_whisper.rs`
- local sidecar module/bin
- config

**Scope:** M

---

## Checkpoint 2: Local STT sidecar working

After Tasks 6-8:

- [ ] `synaps` binary can be built without `whisper-rs` if sidecar feature is disabled.
- [ ] `synaps-voice-local` owns mic/VAD/STT.
- [ ] Voice transcript reaches SynapsCLI through sidecar protocol.
- [ ] No raw audio/transcript logging by default.
- [ ] Human review before TTS/barge-in work.

---

# Phase 3: TTS and Assistant Hooks

## Task 9: Add assistant-output hook events for TTS

**Description:**  
Emit assistant response events from core to active voice sidecar.

**Acceptance criteria:**
- [ ] Core can send `AssistantChunk` and/or `AssistantCompleted` messages to sidecar.
- [ ] Events are sent only when TTS is enabled/configured.
- [ ] Tool output, secret prompts, and code blocks are excluded by default.
- [ ] Tests verify TTS hook does not receive secret prompt content.

**Verification:**
- [ ] `cargo test voice_tts_hooks --lib`
- [ ] Existing secure prompt tests still pass.

**Dependencies:** Checkpoint 1 or 2

**Likely files:**
- `src/chatui/mod.rs`
- assistant streaming path
- `src/voice/sidecar_protocol.rs`
- config

**Scope:** M

---

## Task 10: Kokoros compatibility spike in sidecar

**Description:**  
Verify direct Kokoros crate embedding works in the local sidecar. Only fall back to a `koko` binary if direct crate embedding fails.

**Acceptance criteria:**
- [ ] Minimal sidecar-owned Kokoros synthesis path compiles behind `voice-tts-kokoros` or sidecar-specific feature.
- [ ] A short text sample can synthesize/play or write audio in a test/demo path.
- [ ] Result documents exact crate/API constraints.
- [ ] Core `synaps` does not gain Kokoros dependency by default.

**Verification:**
- [ ] Targeted build/check for sidecar feature.
- [ ] Manual TTS smoke test.

**Dependencies:** Task 9

**Likely files:**
- local sidecar TTS module
- `Cargo.toml`
- maybe docs note

**Scope:** M

---

## Task 11: Implement Kokoros TTS sidecar provider

**Description:**  
Implement Kokoros-backed TTS in `synaps-voice-local`.

**Acceptance criteria:**
- [ ] Sidecar accepts `AssistantChunk` / `AssistantCompleted`.
- [ ] Text is chunked into speakable units.
- [ ] Playback can be stopped via `StopTts`.
- [ ] Code blocks/tool output are not spoken by default.
- [ ] Feature is gated or packaged outside the core binary.

**Verification:**
- [ ] `cargo test voice_tts --lib` or sidecar-specific equivalent.
- [ ] Manual: assistant final response is spoken.
- [ ] Manual: stop command stops playback.

**Dependencies:** Task 10

**Likely files:**
- local sidecar TTS module
- `src/voice/tts/kokoros.rs` if kept in tree
- config

**Scope:** M/L, split further if needed after spike

---

## Checkpoint 3: TTS working through sidecar

After Tasks 9-11:

- [ ] Core sends assistant output only through protocol.
- [ ] Sidecar handles Kokoros and playback.
- [ ] TTS can be stopped.
- [ ] Core remains lightweight by default.
- [ ] Human review before barge-in/conversation mode.

---

# Phase 4: Barge-In and Conversation Mode

## Task 12: Add barge-in behavior

**Description:**  
Support interrupting TTS and optionally current model generation when the user starts voice input.

**Acceptance criteria:**
- [ ] F8 while TTS is speaking sends `StopTts` to sidecar.
- [ ] Sidecar can emit `BargeIn`.
- [ ] Config controls whether barge-in cancels model generation.
- [ ] Default behavior stops TTS only.

**Verification:**
- [ ] `cargo test voice_barge_in --bin synaps`
- [ ] Manual: assistant speaking → F8 → speech stops → listening starts.

**Dependencies:** Checkpoint 3

**Likely files:**
- `src/chatui/mod.rs`
- `src/voice/sidecar_host.rs`
- config

**Scope:** M

---

## Task 13: Add conversation voice mode

**Description:**  
Add opencode-like hands-light conversational mode.

**Acceptance criteria:**
- [ ] `/voice mode conversation` exists.
- [ ] Final transcript auto-submits as user message in conversation mode.
- [ ] Assistant response can be spoken via TTS.
- [ ] Existing `send`/`send it` behavior remains valid in dictation mode.

**Verification:**
- [ ] `cargo test voice_conversation_mode --bin synaps`
- [ ] Manual end-to-end conversation.

**Dependencies:** Tasks 5, 9, 11

**Likely files:**
- `src/chatui/commands.rs`
- `src/chatui/mod.rs`
- `src/voice/types.rs`

**Scope:** M

---

## Checkpoint 4: Opencode-style voice mode complete

After Tasks 12-13:

- [ ] User can use voice dictation mode.
- [ ] User can use voice conversation mode.
- [ ] Assistant responses can be spoken by sidecar TTS.
- [ ] F8 can interrupt speech.
- [ ] Core stays provider-light.

---

# Privacy and Security Invariants

These apply to all tasks:

- [ ] Mic must not open unless voice is enabled/triggered by the user.
- [ ] No raw audio logging.
- [ ] No transcript logging unless explicit debug config is enabled.
- [ ] Sidecar stdout must be protocol-only.
- [ ] Sidecar stderr must not contain transcripts/audio by default.
- [ ] TTS must not speak secrets, tool output, or code blocks by default.
- [ ] Sidecar crashes must not crash the TUI.
- [ ] Core must remain responsive; sidecar I/O must not block TUI rendering/input.

---

# Implementation Rules

- Use a dedicated git worktree before implementation.
- Follow TDD: failing/targeted test first, then implementation.
- Keep tasks small and independently verifiable.
- Do not push or create PRs unless explicitly asked.
- Avoid long full-suite test timeouts; prefer targeted tests such as:
  - `cargo test voice --lib`
  - `cargo test voice_input --bin synaps`
  - `cargo test voice_keybind_tests --bin synaps`

---

# Recommended First Worktree When Approved

```bash
cd /home/jr/Projects/Maha-Media/SynapsCLI
git fetch origin --prune
git worktree add -b feat/voice-sidecar-extension ../.worktrees/SynapsCLI-voice-sidecar-extension dev
cd ../.worktrees/SynapsCLI-voice-sidecar-extension
```

Implementation should not happen on the primary checkout.
