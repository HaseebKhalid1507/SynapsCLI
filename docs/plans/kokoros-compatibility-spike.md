# Kokoros TTS compatibility spike

Date: 2026-04-30

## Result

Direct embedding was attempted with the current crates.io candidates:

- `kokoro-micro = 1.0.0`
- `kokoro-tiny = 0.1.0` (inspected as fallback candidate)

`kokoro-micro` exposes the API shape needed by the local sidecar:

```rust
let mut tts = kokoro_micro::TtsEngine::with_paths(model, voices).await?;
let audio = tts.synthesize_with_options(text, Some("af_sky"), 1.0, 1.0, Some("en"))?;
tts.save_wav("out.wav", &audio)?;
```

However, building it in this environment failed in the transitive `espeak-rs-sys` native build while compiling bundled `espeak-ng` phoneme data:

```text
ph_english_us_nyc(44): Bad vowel file: 'vwl_en_us_nyc/a_raised'
Compiled phonemes: 1 errors.
failed to run custom build command for `espeak-rs-sys v0.1.9`
```

Because of that native build failure, `voice-tts-kokoros` remains a lightweight feature gate in this checkpoint and does **not** pull Kokoros into the core dependency graph. The sidecar TTS protocol path is implemented and testable with deterministic WAV output via `--tts-output-wav`; replacing that shim with `kokoro_micro::TtsEngine` is isolated to `src/bin/synaps-voice-local.rs` once the native build issue is resolved or a better Kokoro crate/backend is selected.

## Invariants verified by design

- Core `synaps` default features remain `[]`.
- Core does not depend on Kokoros by default.
- Assistant text is sanitized before TTS: fenced code and `<tool_result>...</tool_result>` blocks are stripped.
- Sidecar TTS uses JSONL protocol events only (`assistant_completed`, `stop_tts`, `tts_started`, `tts_finished`).
- No raw audio or transcripts are logged.
