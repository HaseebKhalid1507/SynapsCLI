# Phase 7 — Deassume the Host

**Status:** in progress
**Branch:** `feat/path-b-phase7-deassume-host`
**Worktree:** `~/Projects/Maha-Media/.worktrees/SynapsCLI-path-b-phase7-deassume-host`
**Convergence:** `none` (mechanical refactor with mostly mechanical risk; per-slice PRs)

## Premise

Phase 6 removed all *whisper-specific* code from core. But "voice" itself
is still hardcoded into the contract: the extension protocol has a typed
`voice` capability slot, the manager has a `voice_capabilities` index,
the validator enumerates `"stt" | "tts" | "wake_word"`, and the sidecar
lifecycle / chatui glue / toggle key all carry the word.

A community plugin author writing an OCR sidecar, an agent runner, a
foot-pedal trigger, an EEG dictation source, or a clipboard mirror has
to either:

- Pretend their plugin is "voice" and live with misnamed structs in
  their RPC payloads, or
- Send a PR to core to add their kind.

That's the failure. The host should host. It should not enumerate the
modalities its guests are allowed to be.

## Out of scope (deferred)

- **Collapsing `voice::manager` into `extensions::runtime::process`.**
  The two protocols are genuinely different (request/response RPC vs.
  bidirectional push streaming). Worth a future phase, not this one.
- **Generic event-bus subscriber/emitter contract.** The "plugin
  declares `subscribes` / `emits`" reframing is the right long-term
  shape but is its own design exercise. Phase 7 only de-names what
  exists; it does not redesign the wiring.
- **Plugin manifest field renames** (`provides.voice_sidecar` etc.).
  Touching the manifest schema is a synaps-skills change too and
  belongs in a coordinated Phase 8.

## Objective

After Phase 7 lands, `grep -ri '\bvoice\b' src/` returns **zero** hits
in synaps-cli core, with the sole exception of:

- One-release back-compat aliases for the legacy `voice` capability key
  and `voice_toggle_key` setting, clearly marked `#[deprecated]` or
  `// legacy alias`.
- Test fixture display strings (e.g. `"Local Whisper STT"`) that are
  data, not code.

The `local-voice-plugin` itself keeps its name, its `/voice` command,
its Voice settings panel, and its identity. **Only the host's vocabulary
changes.**

## Success criteria

- [ ] No structural `Voice*` types in `src/extensions/` or `src/voice/`
- [ ] No hardcoded modality enum (`"stt" | "tts" | "wake_word"`) in core
- [ ] Capability validation gates on declared permissions only
- [ ] `src/voice/` renamed to a modality-neutral module
- [ ] `src/chatui/voice.rs` renamed to a modality-neutral module
- [ ] `voice_toggle_key` setting renamed with one-release back-compat
- [ ] Local-voice plugin still passes its full smoke gate (no plugin code change required for this phase)
- [ ] All test targets green (`cargo test --all-targets -- --test-threads=1`)
- [ ] Net LOC change: +/- < 200 (this is a rename, not a rewrite)

## Slices

Each slice is one commit, one PR-able unit, and leaves the tree green.

### Slice A — Generic capability declaration (the core leak)

Replace the typed `voice: Option<VoiceCapabilityDeclaration>` slot in
the extension `initialize` response with a generic capability list.

**Before:**
```rust
pub struct InitializeCapabilitiesResult {
    pub tools: Vec<RegisteredExtensionToolSpec>,
    pub providers: Vec<RegisteredProviderSpec>,
    pub voice: Option<VoiceCapabilityDeclaration>,
}

pub struct VoiceCapabilityDeclaration {
    pub name: String,
    pub modes: Vec<String>,                    // ← enumerated in core
    pub endpoint: Option<String>,
}

pub fn validate_voice_capability(...) -> Result<(), String> {
    // hardcoded: stt|wake_word → audio.input, tts → audio.output
}
```

**After:**
```rust
pub struct InitializeCapabilitiesResult {
    pub tools: Vec<RegisteredExtensionToolSpec>,
    pub providers: Vec<RegisteredProviderSpec>,
    pub capabilities: Vec<CapabilityDeclaration>,
}

pub struct CapabilityDeclaration {
    pub kind: String,                  // free-form, plugin-defined
    pub name: String,
    #[serde(default)]
    pub permissions: Vec<String>,      // permissions this capability needs
    #[serde(default)]
    pub params: serde_json::Value,     // free-form metadata
}

pub fn validate_capability(
    decl: &CapabilityDeclaration,
    permissions: &PermissionSet,
) -> Result<(), String> {
    // 1. name + kind non-empty
    // 2. every declared permission must be granted
    // (no enumeration of modalities)
}
```

**Deserialization back-compat (one release):** if a plugin sends
`voice: { name, modes, endpoint }` instead of `capabilities: [...]`,
core synthesizes a `CapabilityDeclaration { kind: "voice",
permissions: <derived from modes>, ... }` so existing plugins keep
working unchanged.

**Files:**
- `src/extensions/runtime/process.rs` — types + validator
- `src/extensions/manager.rs` — index by `(plugin_id, kind)` instead of
  `voice_capabilities: HashMap<String, ...>`
- Tests in same files

**Acceptance:**
- [ ] No `VoiceCapabilityDeclaration` struct in core
- [ ] No `validate_voice_capability` function in core
- [ ] `validate_capability` does not branch on capability kind
- [ ] Legacy `voice: {...}` payload still parses and is accepted
- [ ] All tests pass

**Scope:** M (3-5 files, mostly types + tests)

---

### Slice B — Rename `src/voice/` to `src/sidecar/`

Pure rename. The module owns: sidecar process supervision, the JSONL
push-stream protocol, plugin discovery for sidecar binaries.

| Before | After |
|---|---|
| `src/voice/mod.rs` | `src/sidecar/mod.rs` |
| `src/voice/manager.rs` | `src/sidecar/manager.rs` |
| `src/voice/discovery.rs` | `src/sidecar/discovery.rs` |
| `src/voice/protocol.rs` | `src/sidecar/protocol.rs` |
| `VoiceManager` | `SidecarManager` |
| `VoiceManagerEvent` | `SidecarEvent` |
| `VoiceManagerError` | `SidecarError` |
| `DiscoveredVoiceSidecar` | `DiscoveredSidecar` |
| `VoiceSidecarMode` | `SidecarSessionMode` |
| `VoiceControlPressed/Released` | `TriggerPressed/Released` |
| `VOICE_SIDECAR_PROTOCOL_VERSION` | `SIDECAR_PROTOCOL_VERSION` |

The wire-format strings stay as-is (`"voice_control_pressed"`) for one
release for plugin compatibility — the *Rust enum names* change but
serde aliases keep the old wire names accepted.

**Files:**
- `src/voice/*` → `src/sidecar/*` (rename + edit)
- `src/lib.rs` (module declaration)
- `src/chatui/voice.rs`, `src/chatui/mod.rs`, `src/chatui/app.rs`,
  `src/chatui/commands.rs`, `src/chatui/draw.rs` — import path updates

**Acceptance:**
- [ ] `src/voice/` directory no longer exists
- [ ] `src/sidecar/` exists with renamed types
- [ ] Wire-format JSON unchanged (verified by existing protocol tests)
- [ ] All tests pass

**Scope:** M (mechanical rename, ~10 files)

---

### Slice C — Rename `src/chatui/voice.rs`

| Before | After |
|---|---|
| `src/chatui/voice.rs` | `src/chatui/sidecar_ui.rs` |
| `VoiceUiState` | `SidecarUiState` |
| `VoiceUiStatus` | `SidecarUiStatus` |
| `App.voice` field | `App.sidecar` |
| `app.voice_toggle()` etc. | `app.sidecar_toggle()` |

The user-facing **status strings** (`"listening"`, `"transcribing"`)
already come from the plugin via the protocol, so no display changes.

**Files:**
- `src/chatui/voice.rs` → `src/chatui/sidecar_ui.rs`
- `src/chatui/mod.rs`, `app.rs`, `commands.rs`, `draw.rs`, `input.rs`

**Acceptance:**
- [ ] No `voice` field on `App`
- [ ] No `VoiceUiState`/`VoiceUiStatus` types
- [ ] Status line text unchanged for end users
- [ ] All tests pass

**Scope:** S (one rename, import edits)

---

### Slice D — Rename `voice_toggle_key` setting

Add `sidecar_toggle_key` as the canonical key. Read both
`sidecar_toggle_key` and the legacy `voice_toggle_key` for one release;
write only the new name. The migration helper added in Phase 6 gets a
new clause that copies `voice_toggle_key` → `sidecar_toggle_key` if the
new key is absent.

**Files:**
- `src/chatui/settings/defs.rs`
- `src/chatui/settings/schema.rs`
- `src/chatui/mod.rs` (migration)
- `CHANGELOG.md`

**Acceptance:**
- [ ] `sidecar_toggle_key` is the canonical setting
- [ ] Existing configs with `voice_toggle_key` still work (read fallback)
- [ ] One-shot migration writes `sidecar_toggle_key` once
- [ ] All tests pass

**Scope:** S

---

### Slice E — Sweep + smoke

- `grep -rin '\bvoice\b\|Voice\|VOICE' src/` — confirm zero structural hits
- Manually verify with installed local-voice plugin:
  - `/voice` toggles still work (via plugin command, unchanged)
  - `/voice models`, `/voice download base`, `/settings → Voice` still work
  - Push-to-talk hotkey still triggers transcription
- Update `CHANGELOG.md` with the migration notes

**Acceptance:**
- [ ] grep result reviewed — only legacy-alias lines remain
- [ ] Smoke gate passes with the installed plugin
- [ ] CHANGELOG documents the legacy-alias deprecation window

**Scope:** XS (verification only)

## Risks

| Risk | Mitigation |
|---|---|
| Breaking the local-voice plugin's `initialize` response | Slice A keeps `voice: {...}` parseable as legacy alias; plugin gets one release to migrate |
| Wire-format drift breaking sidecar comms | Slice B keeps wire strings; only Rust enum names change |
| Settings reset on upgrade | Slice D reads both keys; migration is idempotent (only writes when destination absent) |
| Hidden coupling I missed | Slice E grep sweep is the safety net |

## Verification per slice

After each slice:
```bash
cargo build --all-targets 2>&1 | tail -5
cargo test --all-targets -- --test-threads=1 2>&1 | grep -E '^test result:'
```

After Slice E, additionally:
```bash
grep -rin '\bvoice\b' src/ --include='*.rs' | grep -v 'legacy\|alias\|deprecated\|fixture'
# → expect empty
```
