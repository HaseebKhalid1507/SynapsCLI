# Code Review: Local Voice Provider Implementation Plan

Reviewed plan: `docs/plans/2026-04-30-local-voice-provider.md`  
Date: 2026-04-30

## Verdict: REQUEST CHANGES

## Overview

The plan is well-structured, incremental, and correctly identifies major risk areas: native dependencies, TUI event-loop impact, MSRV/Kokoros compatibility, privacy, and accessibility. However, several architectural and safety requirements are either underspecified or deferred too late, which could lead to a difficult implementation or privacy/security regressions.

## Issues

### 🟡 Important — Provider contracts do not define event delivery or threading model

**Location:** `docs/plans/2026-04-30-local-voice-provider.md:96-129`

Task 2 defines `VoiceEvent`, `SpeechToTextProvider`, and `TextToSpeechProvider`, but the proposed traits only expose `start`, `stop`, and `speak`. There is no contract for how events are emitted back to the TUI loop.

This matters because later tasks depend on partial transcripts, final transcripts, errors, TTS state, and responsive cancellation. Without defining whether providers use channels, callbacks, async streams, or a shared event bus, implementation may drift into ad hoc patterns.

Suggested fix: add acceptance criteria and proposed shape for event emission, for example:

```rust
pub trait SpeechToTextProvider {
    fn start(&mut self, events: VoiceEventSender) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    fn is_running(&self) -> bool;
}
```

Or define a `VoiceRuntime` that owns providers and sends `VoiceEvent`s into the app-level event channel.

---

### 🟡 Important — App input abstraction lacks source/backpressure design

**Location:** `docs/plans/2026-04-30-local-voice-provider.md:146-163`

Task 3 introduces:

```rust
enum AppInputEvent {
    Terminal(crossterm::event::Event),
    Voice(VoiceEvent),
}
```

That is a good direction, but the plan does not describe how terminal events and voice events are multiplexed without blocking the TUI. This is important because mic/STT/TTS workers will be cross-thread producers, and terminal polling often has its own blocking behavior.

Suggested fix: include requirements for:

- bounded event channels;
- non-blocking TUI event loop;
- prioritization/fairness between terminal and voice events;
- graceful shutdown of worker threads;
- no unbounded queue growth from partial transcripts.

---

### 🟡 Important — `voice` feature may pull in mic dependencies too early

**Location:** `docs/plans/2026-04-30-local-voice-provider.md:181-197`

Task 4 is a Whisper STT spike that “does not yet use the mic,” but the proposed Cargo feature is:

```toml
voice = ["dep:cpal", "dep:whisper-rs"]
```

This pulls in `cpal` even for a model/fixture-only transcription spike. That weakens the earlier goal that heavy native dependencies are carefully gated.

Suggested fix: split features more granularly:

```toml
voice = []
voice-stt-whisper = ["voice", "dep:whisper-rs"]
voice-mic = ["voice", "dep:cpal"]
voice-tts-kokoros = ["voice", ...]
```

Then have later convenience features compose them if needed.

---

### 🟡 Important — VAD plan underspecifies testing and tuning

**Location:** `docs/plans/2026-04-30-local-voice-provider.md:245-265`

The simple RMS VAD is reasonable for a first implementation, but the plan does not specify configurable thresholds, minimum speech duration, pre-roll buffering, or sample-rate assumptions. Without those, the system may clip the beginning of utterances or emit finals for noise.

Suggested fix: add acceptance criteria for:

- configurable RMS threshold;
- minimum speech duration before accepting an utterance;
- pre-roll buffer so initial phonemes are not dropped;
- max utterance duration;
- tests for silence, short noise burst, continuous speech, and speech followed by silence.

---

### 🟡 Important — Sanitization policy needs to be defined earlier and more concretely

**Location:** `docs/plans/2026-04-30-local-voice-provider.md:275-285`

Task 7 correctly says transcript length should be capped and control characters sanitized. However, the exact policy is not defined, and this behavior is important enough to specify before wiring transcripts into input contexts.

Suggested fix: define a reusable helper such as:

```rust
sanitize_voice_transcript(input: &str, max_chars: usize) -> String
```

Acceptance criteria should include:

- strips or replaces control characters except allowed `\n` when appropriate;
- normalizes CRLF;
- enforces char boundary-safe truncation;
- rejects or escapes terminal control sequences;
- unit tests for Unicode, ANSI escape sequences, very long input, and multiline text.

---

### 🔴 Critical — Voice commands can trigger destructive/contextual actions without an explicit safety model

**Location:** `docs/plans/2026-04-30-local-voice-provider.md:338-365`

Task 9 maps phrases like “submit”, “escape”, “go up”, and “go down” into app actions. This can be risky: accidental speech, background audio, or transcription errors could submit prompts, close modals, or perform unintended actions.

The plan says mapping is “conservative and configurable,” but this needs stronger acceptance criteria.

Suggested fix: require:

- voice commands off by default or gated behind explicit config;
- destructive actions require explicit enablement;
- “submit” only works when `voice.stt_auto_submit` or `voice.commands.submit_enabled` is enabled;
- commands are only recognized in command mode or with a wake phrase/prefix, e.g. “Synaps submit”;
- unit tests for ambiguous phrases and near matches.

---

### 🟡 Important — Kokoros binary fallback needs command-execution safety requirements

**Location:** `docs/plans/2026-04-30-local-voice-provider.md:416-420`

The fallback path shells out to a local `koko` binary. That is a reasonable fallback, but the plan does not specify how the process is invoked safely.

Suggested fix: add acceptance criteria:

- use `std::process::Command` with args, never shell string concatenation;
- configurable binary path must be validated or clearly trusted as user-controlled local config;
- text input passed through stdin or argument-safe mechanism;
- bounded input size;
- timeout/kill behavior on hung subprocess;
- stderr surfaced as setup/runtime error without leaking sensitive text unless debug-enabled.

---

### 🔴 Critical — Privacy safeguards are deferred too late

**Location:** `docs/plans/2026-04-30-local-voice-provider.md:470-479`

Task 13 says no raw audio or transcripts should be logged. This should not wait until the end. Audio and transcript handling begins in Tasks 4–7, and TTS text handling begins around Tasks 11–12.

Suggested fix: move privacy/logging requirements into earlier tasks:

- Task 4: do not log audio buffers or full transcripts by default.
- Task 5: do not log raw mic data/device-sensitive details beyond necessary diagnostics.
- Task 7: do not log inserted transcripts unless explicit debug setting is enabled.
- Task 11/12: do not log TTS text by default.

Task 13 can remain as final audit/polish, but the invariant should exist from first implementation.

---

### 🟡 Important — Model setup docs should include integrity and licensing

**Location:** `docs/plans/2026-04-30-local-voice-provider.md:497-514`

The docs task covers model downloads and platform dependencies, but does not mention checksums, trusted sources, model licenses, or expected disk sizes. Since this feature depends on local model files, setup instructions should help users avoid corrupted or untrusted downloads.

Suggested fix: add docs acceptance criteria for:

- official/trusted download URLs;
- checksum or hash verification where available;
- license notes for Whisper/Kokoro/Kokoros artifacts;
- expected model sizes;
- where models are stored;
- no automatic download unless explicitly approved by user.

---

### 🟡 Important — Config schema should specify path expansion and validation behavior

**Location:** `docs/plans/2026-04-30-local-voice-provider.md:50-90`

The config examples use `~/.synaps-cli/...`, but the plan does not specify whether `~` expansion is supported, whether relative paths are allowed, or when paths are validated.

Suggested fix: add acceptance criteria:

- `~` expansion behavior is defined and tested;
- missing model paths produce actionable errors only when feature/use path is active;
- voice-disabled config should not fail because model paths are missing;
- invalid backend names produce clear config errors;
- defaults do not require model files.

---

### 🟢 Suggestion — Kokoros compatibility spike could happen earlier

**Location:** `docs/plans/2026-04-30-local-voice-provider.md:375-395`

The Kokoros compatibility spike is placed after STT UX tasks. That is acceptable if TTS is lower priority, but the MSRV risk is known upfront and may affect feature structure, docs, CI, and dependency policy.

Suggestion: run Task 10 earlier in parallel with Tasks 1–3, or immediately after Task 1, so the project knows whether TTS will be direct crate, process-based, or deferred before designing provider traits too narrowly.

---

### 🟢 Suggestion — Checkpoints should include CI/build matrix expectations

**Location:** `docs/plans/2026-04-30-local-voice-provider.md:525-567`

The checkpoint structure is good, but it would be stronger if each checkpoint explicitly listed expected build combinations.

Suggestion: add build matrix checks such as:

```text
cargo build
cargo test
cargo build --no-default-features
cargo build --features voice-stt-whisper
cargo build --features voice-mic
cargo build --features voice-tts-kokoros
```

As applicable.

## Positives

- The plan is incremental and reviewable, with sensible task sizing.
- Voice is explicitly off by default.
- Native dependencies are intended to be feature-gated.
- The plan correctly identifies Kokoros/Rust 2024/MSRV as a compatibility risk.
- It separates STT, mic capture, VAD, dictation, commands, TTS, accessibility, and docs into manageable tasks.
- It includes manual verification for TUI responsiveness and user-visible behavior.
- It recognizes privacy concerns around raw audio and transcripts, though those safeguards should be moved earlier.
