# Phase 9 — Lego-Block Sidecar: Total Modality Neutralization

**Author:** team
**Status:** draft (awaiting review)
**Branch:** `feat/path-b-phase9-shim-removal`
**Worktree:** `~/Projects/Maha-Media/.worktrees/SynapsCLI-path-b-phase9-shim-removal`
**Convergence mode:** `none` (human-confirmed); use targeted subagents in dedicated worktrees for parallel inspection/implementation where useful

---

## Iron rule

> **The sidecar abstraction has nothing to do with voice. Or STT. Or
> speech. Or any specific modality. The host knows nothing about what a
> plugin does — it only knows how to spawn it, hand it bytes, and route
> its output. Every name, type, enum, state, capability, mode, event,
> command, and config field that mentions voice / STT / listening /
> transcribing / dictation / barge-in / speaking is a violation.**

Phase 7 partially achieved this (the *user-facing* command surface is now
generic). Phase 8 added multi-sidecar hosting. **Phase 9 finishes the
job** — the wire protocol, the lifecycle event types, the UI status enum,
and every related host symbol must be modality-agnostic.

When Phase 9 is done, the host should be able to ship without anyone
realizing voice is even a possible plugin. A future plugin doing
gestures, OCR, BCI, telemetry, or a debugger sidecar should fit through
the same hole without the host changing.

---

## Why now

The previous PR series (PR1 from this worktree, body of #28) listed seven
"one-release back-compat shims" planned for Phase 9 removal. **Those
seven items were the *named* shims**. While scoping their removal I
audited the full `src/sidecar/` and `src/chatui/sidecar.rs` surface and
found the iron rule has *deep* violations beyond named shims: the host
itself enumerates voice/STT semantics in its core types. Removing only
the named shims would leave the architectural smell intact.

This phase replaces enumerated voice-semantics with **plugin-defined
free-form strings**, plus a small generic event vocabulary that any
sidecar — voice, gesture, OCR, telemetry — can drive.

---

## Architectural shape (the "after" picture)

### Wire protocol — minimal host-recognized frames

```rust
// src/sidecar/protocol.rs (after Phase 9)

pub const SIDECAR_PROTOCOL_VERSION: u16 = 2;

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SidecarCommand {
    /// Plugin-defined initialization payload. Host does not interpret
    /// the body; it forwards verbatim.
    Init { config: serde_json::Value },

    /// Generic activation trigger. `name` is plugin-defined
    /// ("press", "release", "tap", "double_tap", whatever).
    /// Replaces voice_control_pressed / voice_control_released.
    Trigger {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<serde_json::Value>,
    },

    Shutdown,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SidecarFrame {
    /// Handshake. `capabilities` are plugin-defined free-form tags.
    Hello {
        protocol_version: u16,
        extension: String,
        capabilities: Vec<String>,
    },

    /// Plugin reports its current state. `state` is plugin-defined
    /// ("idle", "active", "busy", "recording", whatever).
    /// Optional `label` is human-readable for the pill.
    Status {
        state: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default)]
        capabilities: Vec<String>,
    },

    /// Plugin wants to insert text into the user's input buffer.
    /// Generic enough for voice dictation, OCR paste, snippet expansion,
    /// gesture-to-text, AI autocomplete, etc.
    InsertText {
        text: String,
        mode: InsertTextMode,
    },

    Error { message: String },

    /// Pass-through for plugin-specific events the host does not act on.
    /// Future plugins may use this to talk to other plugins via the host
    /// event bus, or to log structured telemetry.
    #[serde(other)]
    Custom,  // captured as Value in the deserialiser
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InsertTextMode {
    /// Live in-progress text (e.g. partial transcript). Replaces the
    /// previous `Append` segment from the same plugin.
    Append,
    /// Final committed text. Locks the segment into the buffer.
    Final,
    /// Replace the entire input buffer.
    Replace,
}
```

**Removed entirely:**

- `SidecarSessionMode` enum — replaced by free-form `serde_json::Value` in `Init.config`
- `SidecarCapability` enum — replaced by free-form `Vec<String>`
- `SidecarProviderState` enum — replaced by free-form `state: String` in `Status`
- `SidecarConfig` struct — fields collapse into `Init.config`
- `SidecarEvent::ListeningStarted/ListeningStopped/TranscribingStarted` — replaced by `Status { state: "..." }`
- `SidecarEvent::PartialTranscript/FinalTranscript` — replaced by `InsertText { mode: Append/Final }`
- `SidecarEvent::VoiceCommand` — replaced by `Custom` pass-through, or by a future generic `Intent { name, args }` if a use case emerges
- `SidecarEvent::BargeIn` — dropped (plugin-internal concept, not host-level)
- `SidecarCommand::TriggerPressed/Released` — replaced by generic `Trigger { name }`
- `VOICE_SIDECAR_PROTOCOL_VERSION` deprecated alias

### Host-internal lifecycle event — also fully neutral

```rust
// src/sidecar/manager.rs (after Phase 9)

pub enum SidecarLifecycleEvent {
    Ready {
        protocol_version: u16,
        extension: String,
        capabilities: Vec<String>,
    },
    StateChanged { state: String, label: Option<String> },
    InsertText { text: String, mode: InsertTextMode },
    Custom { kind: String, payload: serde_json::Value },
    Error(String),
    Exited,
}
```

### Host UI consumer (`src/chatui/sidecar.rs`)

`SidecarUiStatus` becomes a tiny generic enum:

```rust
pub enum SidecarUiStatus {
    /// Plugin not running.
    Stopped,
    /// Plugin running, currently idle (state == "idle" or "ready").
    Idle,
    /// Plugin running, currently doing work. `label` is from the plugin
    /// or falls back to the state string.
    Active { label: String },
    /// Plugin reported an error.
    Error(String),
}
```

The pill renders `label` directly. Voice plugin sends `label: "Recording"`, gesture plugin sends `label: "Tracking"`, OCR plugin sends `label: "Scanning"` — host doesn't care.

### Plugin manifest — declares semantic mapping (optional)

For pill colours and ordering, plugins can declare in `provides.sidecar.lifecycle.states`:

```json
{
  "states": {
    "idle":     { "color": "muted",   "active": false },
    "active":   { "color": "accent",  "active": true  },
    "busy":     { "color": "warning", "active": true  }
  }
}
```

If `states` is omitted, the host uses defaults (any non-`idle`/non-`stopped` state is `Active`). **No host-side voice knowledge is needed.**

---

## Sub-phase split

This is multiple PRs. Order matters because 9B/C/D bump the wire protocol
(cross-repo flag day) and 9A is independent.

| Sub-phase | Scope | Wire? | Cross-repo? | PR count |
|---|---|:-:|:-:|:-:|
| **9A** | Drop the seven named back-compat shims listed in PR1 body. No wire change. | ✗ | ✗ | 1 |
| **9B** | Replace voice-named enums with free-form strings on the wire. Bumps protocol 1→2. | ✓ | ✓ | 2 (host + plugin) |
| **9C** | Refactor host-internal `SidecarLifecycleEvent` to neutral shape. | ✗ | ✗ | 1 |
| **9D** | Refactor `chatui/sidecar.rs` `SidecarUiStatus` and pill rendering to neutral shape. | ✗ | ✗ | 1 |
| **9E** | synaps-skills `local-voice-plugin` adapts to v2 wire shape; declares state mappings. | ✓ | ✓ | 1 (synaps-skills) |
| **9F** | Documentation sweep, tests stripped of voice references, host/plugin split chart updated. | ✗ | ✗ | 1 |

**Order:** 9A → (9C, 9D in parallel) → 9B + 9E in lockstep → 9F.

9A is pure removal and ships first independently. 9C/9D are internal
refactors with no wire impact and can land before the wire flag day.
9B + 9E are the cross-repo flag day proper.

---

## Phase 9A — Drop the named shims (no wire impact)

Single-repo, no protocol change. Each slice = one commit.

### 9A.1 — Drop `LegacyVoiceCapability`
- **Files:** `src/extensions/runtime/process.rs`
- **Diff:** ~30 lines removed
- **Verify:** `cargo build && cargo test --test-threads=1`; `grep -rn "LegacyVoiceCapability" src/` empty
- **Scope:** XS

### 9A.2 — Drop migration helpers
- **Files:** `src/chatui/mod.rs` (lines 186–187, 1862–1908, 1909–1980 plus their tests in the same file)
- **Removes:** `migrate_legacy_voice_config_keys()`, `migrate_legacy_sidecar_toggle_key()`, both call-sites in TUI bootstrap, all tests for both
- **Verify:** `cargo build && cargo test --test-threads=1`; `grep -rn "migrate_legacy_voice\|migrate_legacy_sidecar_toggle_key" src/ tests/` empty
- **Scope:** S

### 9A.3 — Drop `voice_toggle_key` fallback reads
- **Files:** `src/chatui/settings/draw.rs:578`, `src/chatui/settings/input.rs:499`, `src/skills/mod.rs:111-115`
- **Verify:** `grep -rn "voice_toggle_key" src/` empty
- **Scope:** XS

### 9A.4 — Drop `/voice` builtin alias
- **Files:** `src/chatui/commands.rs:802-835` plus tests at lines 1358, 1448, 1473
- **Note:** `/voice` continues to work *because the local-voice plugin claims it as its lifecycle command in Phase 8*, not because the host hardcodes it. After 9A.4, `/voice` works iff the plugin is installed.
- **Verify:** `grep -rn '"voice" =>' src/chatui/` returns nothing
- **Scope:** S

### 9A.5 — Drop `voice_sidecar` serde alias
- **Files:** `src/skills/manifest.rs`, `src/sidecar/discovery.rs` (test fixtures)
- **Adds:** Negative test asserting legacy `voice_sidecar` field name produces a clear deserialization error
- **Verify:** `grep -rn 'alias = "voice_sidecar"' src/` empty
- **Scope:** S

### 9A.6 — Drop `VOICE_SIDECAR_PROTOCOL_VERSION` deprecated alias
- **Files:** `src/sidecar/protocol.rs:30-36`
- **Verify:** `grep -rn "VOICE_SIDECAR_PROTOCOL_VERSION" src/` empty
- **Scope:** XS

### 9A.7 — Plan/docs
- **Files:** `docs/plans/2026-05-06-shim-removal-phase9.md` (this file), `docs/plans/2026-05-04-deassume-host-phase7.md` (mark shims removed)
- **Scope:** XS

**9A checkpoint:** `cargo test --test-threads=1` green, manual smoke with installed `local-voice` still works (wire unchanged).

---

## Phase 9B — Generalize the wire protocol (cross-repo flag day)

Single coordinated change across SynapsCLI + synaps-skills. Bumps
`SIDECAR_PROTOCOL_VERSION` from 1 to 2. Hosts at v2 reject v1 plugins
with a clear "update local-voice via /plugins" message.

### 9B.1 — Generalize `SidecarSessionMode` → drop entirely; `Init.config` is `serde_json::Value`
- The host no longer knows about session modes. Plugin defines its own config schema in `Init.config`.
- The local-voice plugin will continue to receive `{"mode": "dictation", "language": "en"}` because that's what its host driver sends — but the *host enum* is gone.

### 9B.2 — Generalize `SidecarCapability` enum → `Vec<String>`
- `capabilities: Vec<String>` everywhere. Plugin emits `["stt"]` or `["gesture", "ocr"]`; host stores opaquely.

### 9B.3 — Generalize `SidecarProviderState` → drop; `Status.state: String`
- Plugin emits `state: "idle" | "active" | "busy" | "recording" | "scanning"` — anything. Host stores the string.

### 9B.4 — Replace voice-named `SidecarEvent` variants
- Drop `ListeningStarted`/`ListeningStopped`/`TranscribingStarted` → all become `Status { state, label, ... }`
- Drop `PartialTranscript`/`FinalTranscript` → both become `InsertText { text, mode: Append|Final }`
- Drop `VoiceCommand` → unused; plugin can emit a `Custom` frame if it ever wants intent dispatch
- Drop `BargeIn` → plugin-internal concept

### 9B.5 — Drop `SidecarConfig.language` field
- Already covered by 9B.1 (config is opaque `Value`)

### 9B.6 — Generalize `SidecarCommand::TriggerPressed/Released` → `Trigger { name: String }`
- Wire becomes `{"type": "trigger", "name": "press"}` and `{"type": "trigger", "name": "release"}`
- Drop `#[serde(rename = "voice_control_pressed/released")]` shims

### 9B.7 — Bump `SIDECAR_PROTOCOL_VERSION` 1 → 2
- Host rejects v1 plugins in `Hello` handshake with a clear, marketplace-pointing error.

### 9B.8 — Add `InsertTextMode` enum (the only plugin-semantic addition)
- This is the one non-trivial new abstraction. It's host-generic (any plugin can drive text insertion) and replaces voice-specific `PartialTranscript`/`FinalTranscript`.

### Verification (9B)
- All wire-shape tests in `src/sidecar/protocol.rs` updated to assert the new shape
- New negative test: a v1 `Hello` produces a clear version-mismatch error
- `grep -rniE "voice|stt|speech|transcrib|listen|barge|dictat|conversat|whisper" src/sidecar/` returns nothing except plugin-name references in test fixtures

---

## Phase 9C — Refactor `SidecarLifecycleEvent` (host-internal, no wire)

After 9B lands, the manager's translation layer is rewritten to emit the
new host-internal event shape. This is internal-only; nothing on the
wire moves.

- **Files:** `src/sidecar/manager.rs`
- **Diff:** Replace voice-named variants with `StateChanged { state, label }`, `InsertText { text, mode }`, `Custom { kind, payload }`
- **Doc-comments:** "STT is available" → "Sidecar handshake complete"; "BargeIn dropped" comment removed
- **Verify:** All consumers in `src/chatui/sidecar.rs` updated; `cargo build && cargo test`

---

## Phase 9D — Refactor `chatui/sidecar.rs` UI consumer

- **Files:** `src/chatui/sidecar.rs`
- **Replace** `SidecarUiStatus { Listening, Transcribing, ... }` with `Stopped | Idle | Active { label } | Error(String)`
- **State translation:** map `state == "idle" | "ready" | "stopped"` → `Idle`/`Stopped`; everything else → `Active { label: label.unwrap_or(state) }`
- **Pill rendering:** display `label` directly. No voice-specific UI text in host.
- **Tests:** rewrite "Voice:" pill assertions to use generic plugin display name like "TestPlugin: Active"
- **Verify:** With local-voice plugin emitting `state: "listening", label: "Recording"`, the pill shows `Voice: Recording` (display_name from claim + label from state) — but that's because the *plugin* says so, not the host.

---

## Phase 9E — synaps-skills `local-voice-plugin` v2 adoption

Mirror image of 9B in the plugin repo.

- **Branch:** `feat/local-voice-protocol-v2` (new)
- **Files:** `local-voice-plugin/src/protocol.rs`, `src/main.rs`, `.synaps-plugin/plugin.json`
- **Changes:**
  - Drop the plugin's local `SidecarSessionMode/Capability/ProviderState/Event/Config` enums; mirror SynapsCLI v2 shape
  - `Hello.capabilities: vec!["stt".into(), "barge_in".into()]` (free-form strings)
  - Emit `Status { state: "idle", label: Some("Ready") }` on init, `state: "listening", label: Some("Recording")` on press, etc.
  - Replace `SidecarEvent::FinalTranscript { text }` with `InsertText { text, mode: Final }`
  - Replace `SidecarEvent::PartialTranscript { text }` with `InsertText { text, mode: Append }`
  - Bump plugin `version` in plugin.json
  - Add `provides.sidecar.lifecycle.states: { "idle": {...}, "listening": {...}, ... }` declaration
- **Coordinated merge:** 9E PR opened first; 9B PR opened second; both merged in lockstep with synaps-skills first, then SynapsCLI.

---

## Phase 9F — Tests + docs sweep

- **Strip voice references from `src/sidecar/` tests:** rename `local-voice` fixtures to generic `test-plugin` where the test isn't *specifically* about voice plugin compat
- **Update `docs/host-plugin-split.md`** to reflect the lego-block sidecar shape
- **Update Phase 7 plan doc** to mark all one-release shims removed
- **New doc:** `docs/sidecar-protocol.md` — formal spec of the v2 wire shape, free-form state strings, and the `provides.sidecar.lifecycle.states` mapping convention
- **Verify (whole-phase):**
  ```
  grep -rniE 'voice|stt|speech|transcrib|listen|barge|dictat|conversat|whisper|speak\b' src/sidecar/ src/chatui/sidecar.rs src/chatui/sidecar/
  ```
  should return zero modality-leaking matches (only references in *test fixture data* describing a voice-shaped plugin, never in *host code or types*).

---

## Confirmed decisions

1. **Scope ambition:** full 9A–9F. The sidecar abstraction must become lego-block generic; host code must not mention voice/STT/speech/listening/transcribing/dictation/barge-in/etc.
2. **`InsertText`:** first-class host-generic wire frame.
3. **Convergence mode:** `none`; use subagents instead where useful.
4. **Execution:** use dedicated worktrees for subagent work; choose the cleverest safe ordering toward the end goal.

## Original open questions (resolved)

1. **Scope ambition.** The right reading of "lego blocks" is the full
   redesign above (9A through 9F). Resolved: full redesign.
2. **`InsertText` as a host-generic frame.** Is "insert text into the
   user's input buffer" host-universal enough to be a first-class wire
   frame, or should it be a `Custom` frame that the host happens to
   recognize from any plugin that emits it? Resolved: first-class.
3. **Trigger payload semantics.** Should `SidecarCommand::Trigger { name }`
   carry an optional `payload: Value`? That would let one plugin handle
   multiple trigger flavours from a single keybind config — recommend
   yes (already in the proposal above), defaulting to `None`.
4. **`Custom` frame catch-all.** Right now I propose using
   `#[serde(other)]` for unrecognized frames. The serde rust convention
   is to capture the original `Value` for forwarding. Approve?
5. **Migration error UX.** When v2 host meets v1 plugin, should the
   error route to the marketplace UI directly ("Update local-voice")
   or print a CLI-only message? Recommend: marketplace UI.
6. **Convergence mode.** I declared `none` because each slice is
   mechanical and well-tested. The 9B + 9E flag day might warrant
   `informed` if you want a second-set-of-eyes critique on the
   protocol-v2 shape before lock-in. Resolved: keep `none`; use subagents.

---

## Verification (whole phase)

After all of 9A–9F:

- [ ] `cargo build --release` clean
- [ ] `cargo test --test-threads=1` — full suite green
- [ ] `cargo clippy -- -D warnings` clean
- [ ] `grep -rniE 'voice|stt|speech|transcrib|listen|barge|dictat|conversat|whisper|speak\b' src/sidecar/ src/chatui/sidecar.rs` returns zero matches
- [ ] All sidecar tests use generic plugin/fixture names; nothing voice-specific in `src/sidecar/`
- [ ] Plugin manifest convention for `provides.sidecar.lifecycle.states` documented
- [ ] Cross-repo lockstep merge of 9B + 9E completed; protocol v2 live
- [ ] Manual end-to-end with `local-voice` plugin still works (because the *plugin* still implements voice; the host just doesn't know)

---

## Estimated effort

| Sub-phase | Effort |
|---|---|
| 9A | 1 session, 7 small commits, ~250 lines deleted |
| 9B | 1 session, single PR, ~400 lines changed (wire-protocol redesign) |
| 9C | 1 session, ~150 lines changed |
| 9D | 1 session, ~200 lines changed (UI consumer + tests) |
| 9E | 1 session in synaps-skills, ~250 lines changed |
| 9F | 1 session, docs + test cleanup, ~150 lines changed + new spec doc |

**Total:** ~1500 lines net change, 6 PRs, 2 repos, one cross-repo flag
day. Scope is comparable to Phase 7 + Phase 8 combined.

---

## Implementation discipline

- Worktree: `~/Projects/Maha-Media/.worktrees/SynapsCLI-path-b-phase9-shim-removal` (active)
- Branch: `feat/path-b-phase9-shim-removal`
- 9A may stay on this worktree
- 9B/C/D/E may each warrant fresh worktrees off `dev` (post-9A merge)
- After each PR merges: cleanup per `worktrees-by-default`
- TDD where tests don't exist yet; otherwise verify-by-existing-tests-pass
- Verification gate before any "done" claim per `verification-before-completion`
