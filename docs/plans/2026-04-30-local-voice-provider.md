# Local Voice Provider Plan: whisper-rs + Kokoros

Date: 2026-04-30

## Summary

Set up a local voice provider stack for SynapsCLI:

- **STT:** `whisper-rs` over `whisper.cpp`
- **TTS:** Kokoro Rust / Kokoros from <https://github.com/lucasjinreal/Kokoros>
- **TUI integration:** provider sits at the base `chatui` event loop and emits app-level voice events.

Important compatibility note: Kokoros was reportedly updated recently for current Rust. Proceed with a direct Kokoros compatibility spike first; if direct embedding is still blocked, report the exact blocker before falling back to a local `koko` provider process.

---

## Convergence mode

Approved: `convergence: holdout`

Reason: this touches the base TUI loop, audio capture, native dependencies, accessibility behavior, and local model management. Holdout mode provides strict role isolation for higher confidence.

Fixed params:

- `threshold`: `0.8`
- `axis_weights`: default code-review weights
- `max_fix_iterations`: `2`
- `max_total_calls`: `10`

---

## User-approved decisions

1. Convergence mode: `holdout`.
2. Kokoros/MSRV: proceed assuming direct Kokoros embedding should work with current Rust/Kokoros updates; verify in the compatibility spike before choosing fallback.
3. First voice UX: push-to-talk first, with toggle optional through settings/fallback.

---

## Dependency graph

```text
Config + feature flags
  ├── Voice event types / provider traits
  │   ├── STT provider: cpal + whisper-rs
  │   ├── TTS provider: Kokoros adapter
  │   └── TUI event-loop integration
  │       ├── voice dictation into active input context
  │       ├── voice command mapping
  │       └── accessibility/status UI
  └── Tests + docs + setup guidance
```

---

## Task 1: Voice feature flags and config schema

**Description:** Add optional voice configuration without changing runtime behavior yet.

**Acceptance criteria:**
- [ ] Voice is off by default.
- [ ] Config supports STT/TTS provider settings.
- [ ] Config defines path expansion/validation semantics: `~` expansion is supported when paths are used; missing model paths are actionable errors only when the relevant voice provider is active; defaults do not require model files.
- [ ] Invalid backend/mode names produce clear warnings or errors without silently activating unknown providers.
- [ ] Heavy native dependencies are gated behind feature flags.
- [ ] Existing builds without voice remain unaffected.

**Likely config keys:**

```text
voice.enabled = false
voice.mode = push_to_talk
voice.stt_backend = whisper-rs
voice.stt_model_path = ~/.synaps-cli/models/whisper/ggml-base.en.bin
voice.stt_language = en
voice.stt_show_partials = true
voice.stt_auto_submit = false
voice.stt_silence_submit_ms = 1000
voice.stt_vad_rms_threshold = 0.01
voice.stt_min_speech_ms = 250
voice.stt_preroll_ms = 300
voice.stt_max_utterance_ms = 30000
voice.max_transcript_chars = 16000

voice.tts_enabled = false
voice.tts_backend = kokoros
voice.tts_model_path = ~/.synaps-cli/models/kokoro/kokoro-v1.0.onnx
voice.tts_voices_path = ~/.synaps-cli/models/kokoro/voices-v1.0.bin
voice.tts_voice = af_sky
voice.tts_auto_speak = false
```

**Verification:**
- [ ] `cargo test`
- [ ] `cargo build`
- [ ] Manual: config parser accepts voice keys and preserves unknown keys.

**Dependencies:** None  
**Files likely touched:**
- `Cargo.toml`
- `src/core/config.rs`
- possibly `src/chatui/settings/defs.rs`
- possibly `src/chatui/settings/schema.rs`

**Scope:** S

---

## Task 2: Define voice provider contracts and events

**Description:** Add internal voice abstractions independent of Whisper/Kokoros implementation.

**Acceptance criteria:**
- [ ] `VoiceEvent` supports start, stop, partial transcript, final transcript, TTS state, and error.
- [ ] A `VoiceEventSender` / channel contract is defined for provider-to-runtime event delivery.
- [ ] `VoiceRuntime` or equivalent owner coordinates providers, worker lifecycle, cancellation, and event forwarding.
- [ ] `SpeechToTextProvider` trait is defined and emits events through the approved channel contract.
- [ ] `TextToSpeechProvider` trait is defined and emits events through the approved channel contract.
- [ ] Provider work is explicitly off the TUI thread, with graceful shutdown semantics.
- [ ] No TUI behavior changes yet.

**Proposed shape:**

```rust
pub enum VoiceEvent {
    ListeningStarted,
    ListeningStopped,
    PartialTranscript(String),
    FinalTranscript(String),
    Error(String),
    TtsStarted,
    TtsStopped,
}

pub type VoiceEventSender = tokio::sync::mpsc::Sender<VoiceEvent>;

pub trait SpeechToTextProvider {
    fn start(&mut self, events: VoiceEventSender) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    fn is_running(&self) -> bool;
}

pub trait TextToSpeechProvider {
    fn speak(&mut self, text: &str, events: VoiceEventSender) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
}
```

Design notes:
- Use bounded channels for provider events.
- Prefer `try_send` or backpressure-aware sends for high-frequency partials.
- Providers must never mutate TUI state directly.

**Verification:**
- [ ] `cargo test`
- [ ] `cargo build`
- [ ] Unit tests for event/state types where useful.

**Dependencies:** Task 1  
**Files likely touched:**
- `src/voice/mod.rs`
- `src/voice/types.rs`
- `src/lib.rs`

**Scope:** S

---

## Task 3: Add app-level input event abstraction

**Description:** Decouple the TUI loop from raw `crossterm::Event` enough for voice events to enter through the same base loop.

**Acceptance criteria:**
- [ ] Existing keyboard, mouse, paste behavior is unchanged.
- [ ] TUI loop can accept both terminal events and voice events.
- [ ] Terminal and voice sources are multiplexed through non-blocking `tokio::select!` branches or equivalent.
- [ ] Voice event channel is bounded; high-frequency partial transcript events cannot grow memory without bound.
- [ ] Terminal input remains responsive during voice event bursts.
- [ ] Voice worker shutdown is triggered when the TUI exits.
- [ ] Voice events currently no-op or show status only.

**Proposed shape:**

```rust
enum AppInputEvent {
    Terminal(crossterm::event::Event),
    Voice(VoiceEvent),
}
```

**Verification:**
- [ ] `cargo test`
- [ ] `cargo build`
- [ ] Manual: type in TUI, paste, slash commands, settings modal, models modal still work.

**Dependencies:** Task 2  
**Files likely touched:**
- `src/chatui/mod.rs`
- `src/chatui/input.rs`
- maybe `src/chatui/app.rs`

**Scope:** M

---

## Task 4: Whisper STT provider spike

**Description:** Build a minimal feature-gated `whisper-rs` provider that can load a model and transcribe a supplied PCM buffer or WAV fixture. This does not yet use the mic.

**Acceptance criteria:**
- [ ] Feature-gated dependency on `whisper-rs`.
- [ ] Provider loads configured model path.
- [ ] Provider transcribes a short known audio file or test fixture when present.
- [ ] Missing model path produces clear error, not panic.
- [ ] Model paths are validated only when the Whisper provider is constructed/used, not when voice is disabled.
- [ ] Raw audio buffers and full transcripts are not logged by default.

**Cargo feature candidates:**

```toml
voice = []
voice-stt-whisper = ["voice", "dep:whisper-rs"]
voice-mic = ["voice", "dep:cpal"]
voice-tts-kokoros = ["voice", ...]
voice-metal = ["voice-stt-whisper", "whisper-rs/metal"]
voice-cuda = ["voice-stt-whisper", "whisper-rs/cuda"]
voice-vulkan = ["voice-stt-whisper", "whisper-rs/vulkan"]
voice-openblas = ["voice-stt-whisper", "whisper-rs/openblas"]
```

**Verification:**
- [ ] `cargo build`
- [ ] `cargo build --features voice`
- [ ] Optional/manual: run fixture transcription with local model.

**Dependencies:** Task 2  
**Files likely touched:**
- `Cargo.toml`
- `src/voice/stt_whisper.rs`
- `src/voice/mod.rs`

**Scope:** M

---

## Task 5: Microphone capture provider with `cpal`

**Description:** Add local microphone capture and feed 16kHz mono PCM into the STT worker.

**Acceptance criteria:**
- [ ] Mic capture starts/stops on command.
- [ ] Audio capture runs off the TUI thread.
- [ ] Audio stream uses bounded channels/ring buffer.
- [ ] Mic permission/device errors are surfaced as `VoiceEvent::Error`.
- [ ] Raw mic data is never logged; device diagnostics avoid unnecessary sensitive details.

**Design notes:**
- Use `cpal` for cross-platform capture.
- Convert input to mono float PCM.
- Resample to 16kHz if needed. A resampler crate may be needed; otherwise begin with a simple first-pass conversion.
- Do not log raw audio.

**Verification:**
- [ ] `cargo build --features voice`
- [ ] Manual: start/stop mic without freezing TUI.
- [ ] Manual: unplug/no mic error is visible and recoverable.

**Dependencies:** Task 4  
**Files likely touched:**
- `Cargo.toml`
- `src/voice/audio.rs`
- `src/voice/stt_whisper.rs`

**Scope:** M

---

## Task 6: VAD / utterance segmentation

**Description:** Avoid transcribing silence and decide when a final transcript is ready.

**Acceptance criteria:**
- [ ] Silence does not continuously trigger expensive Whisper calls.
- [ ] User speech results in a final transcript after configured silence.
- [ ] `voice.stt_silence_submit_ms` is respected.
- [ ] RMS threshold is configurable and has a documented default.
- [ ] Minimum speech duration filters short noise bursts.
- [ ] Pre-roll buffering avoids clipping initial phonemes.
- [ ] Maximum utterance duration prevents unbounded recording/transcription work.
- [ ] Auto-submit remains disabled by default.

**Implementation options:**
1. Start with simple RMS energy threshold + silence timer.
2. Upgrade to Whisper VAD or Silero later.

Recommended first implementation: simple VAD, because it is easier to validate and avoids more native complexity initially.

**Verification:**
- [ ] `cargo build --features voice`
- [ ] Unit tests for segmentation state machine: silence, short noise burst, continuous speech, speech followed by silence, pre-roll preservation, and max-duration cutoff.
- [ ] Manual: speak -> final transcript appears; silence alone does not.

**Dependencies:** Task 5  
**Files likely touched:**
- `src/voice/vad.rs`
- `src/voice/stt_whisper.rs`

**Scope:** M

---

## Task 7: Voice dictation into TUI input contexts

**Description:** Final STT transcripts insert into the active TUI input context.

**Acceptance criteria:**
- [ ] In normal chat input, final transcript inserts at cursor.
- [ ] In settings text editors, final transcript inserts into editor buffer.
- [ ] In plugin marketplace URL editor, final transcript inserts into editor buffer.
- [ ] Transcript length is capped and control chars are sanitized.
- [ ] A reusable `sanitize_voice_transcript(input: &str, max_chars: usize)` helper exists before UI insertion.
- [ ] Sanitizer normalizes CRLF, strips or replaces disallowed control characters, removes ANSI/terminal escape sequences, and truncates at valid UTF-8 boundaries.
- [ ] Inserted transcripts are not logged unless an explicit debug setting enables transcript logging.

**Important:** Apply the same or stricter limit policy as paste. Current main paste limit is 100k chars; voice should probably be smaller, e.g. 8k or 16k chars per utterance.

**Verification:**
- [ ] `cargo test`
- [ ] Unit tests for sanitizer: Unicode, ANSI escape sequences, very long input, CRLF normalization, control chars, and multiline policy.
- [ ] `cargo build --features voice`
- [ ] Manual: dictate into chat input.
- [ ] Manual: dictate into settings editor.

**Dependencies:** Task 6  
**Files likely touched:**
- `src/chatui/input.rs`
- `src/chatui/settings/input.rs`
- `src/chatui/plugins/input.rs`
- `src/voice/types.rs`

**Scope:** M

---

## Task 8: Push-to-talk and voice control keybinds

**Description:** Add keybindings to start/stop/toggle voice capture.

**Acceptance criteria:**
- [ ] Default keybind starts/stops listening.
- [ ] Push-to-talk mode supported if terminal key release events are available/reliable.
- [ ] Toggle mode supported as fallback.
- [ ] Voice state visible in TUI.

**Likely approach:**
- First implement push-to-talk.
- Keep toggle mode as a settings/fallback option if terminal key-release behavior is unreliable.

Possible defaults:
- `Ctrl+Alt+V`: toggle dictation
- `/voice on`, `/voice off`, `/voice status`

**Verification:**
- [ ] `cargo build --features voice`
- [ ] Manual: toggle listening.
- [ ] Manual: status indicator changes.

**Dependencies:** Task 7  
**Files likely touched:**
- `src/chatui/input.rs`
- `src/chatui/commands.rs`
- `src/chatui/draw.rs`
- `src/chatui/app.rs`

**Scope:** M

---

## Task 9: Voice command mapping

**Description:** Translate spoken phrases into app actions.

**Acceptance criteria:**
- [ ] “submit” can submit current input only when explicitly enabled by `voice.stt_auto_submit` or `voice.commands.submit_enabled`.
- [ ] Destructive/contextual actions such as submit, escape/cancel, and modal-closing commands are off by default or require explicit command mode/wake prefix.
- [ ] “cancel” / “escape” maps to abort/close only in safe, explicitly enabled contexts.
- [ ] “slash settings” becomes `/settings`.
- [ ] “slash model” / “slash models” opens model flow.
- [ ] Command mapping is conservative and configurable.
- [ ] Ambiguous phrases and near matches are treated as dictation, not commands.

**Examples:**

```text
"slash settings" -> "/settings"
"slash compact" -> "/compact"
"submit" -> Enter action
"new line" -> "\n"
"escape" -> Esc action
"go up" -> Up key equivalent
"go down" -> Down key equivalent
```

**Verification:**
- [ ] Unit tests for phrase mapping, ambiguous phrases, near matches, disabled command mode, wake prefix/command mode, and submit gating.
- [ ] Manual: voice opens settings.
- [ ] Manual: voice submits only when explicitly commanded or auto-submit enabled.

**Dependencies:** Task 8  
**Files likely touched:**
- `src/voice/commands.rs`
- `src/chatui/input.rs`

**Scope:** M

---

## Task 10: Kokoros compatibility spike

**Description:** Determine whether Kokoros can be embedded directly as a crate under SynapsCLI’s current Rust/MSRV constraints. Run this spike as early as practical after Task 1 or alongside Tasks 2–3 so provider contracts are not designed around the wrong TTS shape.

**Known issue / current assumption:**
- SynapsCLI currently declares `rust-version = "1.80"`.
- Kokoros was reportedly updated recently for current Rust.
- Direct embedding is preferred if it builds cleanly with the accepted toolchain/MSRV policy.

**Acceptance criteria:**
- [ ] Confirm whether direct dependency builds in this repo.
- [ ] Identify exact minimum Rust version required.
- [ ] Decide one of:
  - direct crate integration
  - git subdependency with MSRV bump
  - local binary/provider-process integration
  - defer TTS embedding

**Verification:**
- [ ] `cargo build --features voice-tts`
- [ ] Document result in implementation notes.

**Dependencies:** Task 1  
**Files likely touched:**
- `Cargo.toml`
- temporary spike only, unless accepted

**Scope:** S

---

## Task 11: Kokoros TTS adapter

**Description:** Implement Kokoros as a feature-gated TTS provider.

**Preferred path if direct crate works:**
- Call Kokoros APIs directly.
- Load model and voices from config paths.
- Generate audio to memory or temp file.
- Play through local audio output.

**Fallback path if direct crate is blocked:**
- Shell out to local `koko` binary.
- Use `koko stream` or `koko text`.
- Treat this as provider-process mode.
- Keep it optional and local-only.

**Acceptance criteria:**
- [ ] `voice.tts_enabled = true` can speak a test string.
- [ ] TTS does not block the TUI loop.
- [ ] TTS can be interrupted/stopped.
- [ ] Missing Kokoros model/voices produces clear setup message.
- [ ] No remote API is used.
- [ ] If falling back to local `koko` process mode, invoke it with `std::process::Command` args only; never shell-concatenate text or paths.
- [ ] TTS text is bounded and passed via stdin or another argument-safe mechanism.
- [ ] Hung subprocesses have timeout/kill behavior.
- [ ] stderr/setup errors are surfaced without logging sensitive TTS text unless debug-enabled.
- [ ] TTS text is not logged by default.

**Verification:**
- [ ] `cargo build --features voice-tts`
- [ ] Manual: synthesize “hello from Synaps”.
- [ ] Manual: interrupt speech.

**Dependencies:** Task 10  
**Files likely touched:**
- `Cargo.toml`
- `src/voice/tts_kokoros.rs`
- `src/voice/audio_out.rs`
- `src/voice/mod.rs`

**Scope:** M

---

## Task 12: Auto-speak assistant responses

**Description:** Optional TTS playback for assistant messages, similar to VS Code’s auto-synthesize behavior.

**Acceptance criteria:**
- [ ] Off by default.
- [ ] If the user used voice input and `voice.tts_auto_speak = true`, assistant text is spoken.
- [ ] Streaming responses are chunked sensibly, ideally sentence-by-sentence.
- [ ] Markdown/code/tool output is not read verbatim unless configured.
- [ ] TTS text/chunks are not logged by default.

**Verification:**
- [ ] Manual: dictate a prompt, hear response.
- [ ] Manual: disable auto-speak and confirm silence.
- [ ] Manual: code blocks are skipped or summarized according to default policy.

**Dependencies:** Task 11  
**Files likely touched:**
- `src/chatui/stream_handler.rs`
- `src/chatui/mod.rs`
- `src/voice/tts_kokoros.rs`

**Scope:** M

---

## Task 13: Accessibility indicators and safety polish

**Description:** Make voice state accessible, clear, and safe.

**Acceptance criteria:**
- [ ] TUI shows listening/processing/error status.
- [ ] Optional terminal bell or accessibility signal on start/stop.
- [ ] No raw audio is logged; this is audited from Tasks 4–5 onward, not introduced at the end.
- [ ] Transcripts are not logged unless explicit debug config is enabled; this is audited from Task 7 onward.
- [ ] Mic is never opened unless voice is enabled or user triggers it.

**Verification:**
- [ ] Manual: listening status is obvious.
- [ ] Manual: errors are visible.
- [ ] Review logs for no transcript/audio leakage.

**Dependencies:** Tasks 8, 11  
**Files likely touched:**
- `src/chatui/draw.rs`
- `src/chatui/app.rs`
- `src/core/logging.rs`
- `src/voice/mod.rs`

**Scope:** S

---

## Task 14: Documentation and setup scripts

**Description:** Document local model setup for Whisper and Kokoros.

**Acceptance criteria:**
- [ ] README/docs explain how to enable voice feature.
- [ ] Docs explain model downloads and paths.
- [ ] Docs list trusted/official download URLs where available, checksum/hash verification where available, model licenses, expected disk sizes, and storage locations.
- [ ] Docs state that SynapsCLI will not automatically download voice models unless the user explicitly approves.
- [ ] Docs list platform dependencies:
  - microphone permissions
  - Linux ALSA/Pulse/PipeWire notes
  - Kokoros `pkg-config` + `libopus-dev`
  - GPU acceleration feature flags
- [ ] Troubleshooting section included.

**Verification:**
- [ ] Docs reviewed.
- [ ] Fresh setup path is executable by a user.

**Dependencies:** Tasks 4, 10, 11  
**Files likely touched:**
- `README.md`
- `docs/voice.md`
- maybe setup scripts

**Scope:** S

---

# Checkpoints

## Checkpoint A: After Tasks 1–3

Goal: architectural foundation.

- [ ] Existing app builds and behaves unchanged.
- [ ] Voice event path exists but no native audio dependencies required.
- [ ] Build matrix passes for the current scope: `cargo build`, targeted tests, `cargo build --features voice`, and any granular feature builds introduced by Tasks 2–3.

## Checkpoint B: After Tasks 4–6

Goal: local STT works outside full UX.

- [ ] `whisper-rs` model loads.
- [ ] Mic capture works.
- [ ] Speech produces final transcript events.
- [ ] TUI remains responsive.
- [ ] Privacy invariant holds: no raw audio buffers or full transcripts in normal logs.
- [ ] Build matrix passes for introduced native features, e.g. `voice-stt-whisper`, `voice-mic`, and selected acceleration flags where available.

## Checkpoint C: After Tasks 7–9

Goal: voice can operate the app.

- [ ] Dictation into chat works.
- [ ] Voice commands work for basic app actions.
- [ ] Submit behavior is safe and configurable.
- [ ] Transcript sanitizer tests pass and command mapping tests cover disabled/ambiguous cases.

## Checkpoint D: After Tasks 10–12

Goal: TTS works.

- [ ] Kokoros path chosen: direct crate or local binary provider.
- [ ] App can speak text locally.
- [ ] Optional assistant response TTS works.
- [ ] If process fallback is used, subprocess invocation is argument-safe, bounded, interruptible, and timeout-protected.
- [ ] TTS text is not logged by default.

## Checkpoint E: After Tasks 13–14

Goal: accessibility and release polish.

- [ ] Clear status indicators.
- [ ] No privacy leaks in logs.
- [ ] Setup docs are complete.

---

# Recommended implementation order

1. Task 1 — config/features
2. Task 2 — provider contracts
3. Task 3 — app input abstraction
4. Task 4 — Whisper provider spike
5. Task 5 — mic capture
6. Task 6 — VAD/finalization
7. Task 7 — dictation insertion
8. Task 8 — keybinds/PTT first, toggle fallback
9. Task 9 — voice commands
10. Task 10 — Kokoros spike (may run earlier after Task 1 / alongside Tasks 2–3)
11. Task 11 — Kokoros adapter
12. Task 12 — auto-speak responses
13. Task 13 — accessibility/safety
14. Task 14 — docs

---

# Decisions approved before implementation

1. Convergence mode: `holdout`.
2. Kokoros/MSRV: proceed assuming direct Kokoros embedding should work with current Rust/Kokoros updates; verify in the compatibility spike before choosing fallback.
3. First voice UX: push-to-talk first, with toggle optional through settings/fallback.
