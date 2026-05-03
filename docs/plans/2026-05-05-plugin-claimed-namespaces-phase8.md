# Phase 8 — Plugin-Claimed Namespaces & Multi-Sidecar Hosting

**Status:** draft
**Branch:** TBD (`feat/path-b-phase8-claimed-namespaces`)
**Convergence:** `none` (two clean slices; per-slice PRs)
**Coordination:** requires synaps-skills manifest update for `local-voice-plugin`

## Premise

Phase 7 deassumed the host *internally* — `src/voice/` became
`src/sidecar/`, `VoiceManager` became `SidecarManager`, the typed
`voice` capability became a free-form `kind` field. But Phase 7 also
*invented* new top-level user-facing surfaces — `Category::Sidecar`,
`/sidecar toggle/status`, `sidecar_toggle_key` — that sit alongside
the plugin's own already-good UX (`Voice` settings, `/voice` command).

The bug: the plugin chose its identity (`Voice`, `/voice`) and core
chose a parallel one (`Sidecar`, `/sidecar`). Two settings panels,
two command trees, one underlying lifecycle. Users see the seam.

Worse: only the *first* plugin with `provides.sidecar` ever wins
discovery. A user who wants voice + OCR + clipboard mirror
simultaneously has no path; the second and third plugins are silently
ignored.

## Objective

After Phase 8 lands:

1. **Plugins claim their lifecycle UX.** When `local-voice-plugin` is
   loaded, `/voice toggle` and `/voice status` exist as the user-facing
   commands (auto-registered by core from the plugin's manifest); the
   "Voice" settings panel hosts the toggle-key field; the pill shows
   "Voice" not "Sidecar". `Category::Sidecar` is gone from the user
   surface.

2. **Multiple sidecars coexist.** `discover_all()` returns every
   plugin with `provides.sidecar`. Each runs in its own
   `SidecarUiState`, has its own toggle command, its own keybind, its
   own pill segment. Toggling one doesn't touch the others.

3. **Generic `/sidecar` survives as a safe fallback.** Plugins that
   *don't* claim a namespace (quick prototypes, demo plugins) are
   reachable via `/sidecar <plugin-id> toggle`. The unqualified
   `/sidecar toggle` works only when exactly one such plugin exists;
   ambiguity errors instead of silently picking one.

## Out of scope (deferred to a future phase)

- **Sidecar inter-process coordination.** Two sidecars that both want
  the microphone don't negotiate; that's a permissions-system concern
  (`audio.input` exclusivity) and doesn't belong in the lifecycle
  layer.
- **Cross-plugin event piping** (e.g. an OCR sidecar firing a final-
  payload that an LLM-tools plugin consumes). That's the event-bus
  redesign in `docs/plans/event-bus-enhancements.md`.
- **Auto-detection of overlapping keybinds across the entire keybind
  registry.** Slice 8B handles overlap among sidecar-toggle keys
  specifically; the broader cross-action overlap is a separate
  feature.

## Success criteria

- [ ] `local-voice-plugin` loaded → `/voice toggle` works, `/sidecar toggle`
      either errors (multiple unclaimed) or routes to whatever is unclaimed.
- [ ] Settings UI shows "Voice" with the toggle-key field; no separate
      "Sidecar" panel.
- [ ] Pill row shows plugin's `display_name`, not `"sidecar"`.
- [ ] Test fixture: two plugins both providing sidecars → both load, both
      independently toggleable, both shown in pill.
- [ ] Two plugins declaring the same default keybind → first-loaded wins,
      second emits a clear warning to the chat log and `/extensions`.
- [ ] All existing tests pass at `--test-threads=1`.
- [ ] Net core LOC change is *negative* (we collapse `Category::Sidecar`,
      `sidecar_toggle_key`, hardcoded pill string, etc. into per-plugin
      registration).

## Manifest schema additions

```jsonc
{
  "provides": {
    "sidecar": {
      "command": "bin/voice-plugin",
      "protocol_version": 1,
      "model": { "default_path": "...", "required_for_real_stt": true },

      // NEW in Phase 8 — all optional; absent ⇒ falls back to
      // generic `/sidecar` namespace and the legacy "Sidecar" UI.
      "lifecycle": {
        "command":           "voice",   // /voice toggle, /voice status
        "settings_category": "voice",   // toggle-key field added here
        "display_name":      "Voice",   // pill, /extensions, errors
        "importance":        100        // pill ordering (desc); default 0
      }
    }
  },
  "keybinds": [
    { "key": "ctrl+space", "command": "voice toggle" }   // standard plugin keybind
  ]
}
```

The plugin author sets these. Core uses them to dynamically register
the lifecycle commands and inject the toggle-key field. **No core
changes are required to support a new plugin's namespace** — the
manifest carries it.

## Slices

### Slice 8A — Plugin-claimed lifecycle namespace (single sidecar)

This slice keeps single-sidecar discovery (the existing `discover()`
return-the-first behavior) but moves the *user-facing UX* under the
plugin's namespace. Multi-sidecar comes in 8B.

**Steps:**

1. ✅ **8A.1 (`66e2fee`)** — Extend `SidecarManifest` with
   `lifecycle: Option<SidecarLifecycle>` (`command`, `settings_category`,
   `display_name`, `importance` clamped `-100..=100`); add
   `discover_all_in()` that returns every sidecar (keeps `discover_in()`
   wrapping `.first()`).
2. ✅ **8A.2 (`44cc80a`)** — Plugin lifecycle claims registered in the
   `CommandRegistry` keyed by command word; first-loaded wins on
   collision (collisions exposed via `lifecycle_claim_collisions()`).
   Dispatcher in `chatui/commands.rs` intercepts claimed commands
   *before* the builtin match arm, routing
   `<word>` / `<word> toggle` → `SidecarToggle`,
   `<word> status` → `SidecarStatus`, and falling through to the
   plugin-command resolver for any other subcommand. Claimed command
   words appear in `all_commands()` for autocomplete.
3. ✅ **8A.3 (`e81f7e5`)** — Registry's `plugin_settings_categories()`
   injects a synthetic `_lifecycle_toggle_key` field at the front of
   the claimed plugin's settings category. Cycler over
   `["F8","F2","F12","C-V","C-G"]`. No-op (with `tracing::warn!`) when
   `lifecycle.settings_category` references a non-existent category.
4. ✅ **8A.4 (`6ad8469`)** — `schema::visible_categories(claims)`
   returns `CATEGORIES` minus `Category::Sidecar` if any claim has
   `settings_category.is_some()`. Static `Category::Sidecar` and
   `sidecar_toggle_key` setting kept for back-compat. (Wire-up of
   `visible_categories` in `draw.rs` deferred — TODO comment in
   `schema.rs`.)
5. ✅ **8A.5 (`74120ce`,`8001c99`,`03197da`)** —
   `SidecarUiState.display_name: Option<String>` field + setter;
   `status_line()` uses display name; `sidecar_pill_span` idle/error
   pills carry display name (listening/transcribing stay
   modality-neutral); chatui handler messages (`"X online"`,
   `"X press failed"`, etc.) resolve display name with `"sidecar"`
   fallback. The setter is wired but unused — slice 8B's dispatcher
   will call it post-spawn.
6. ✅ **8A.6 + 8A.7 (`d3f8945`)** — `/sidecar` dispatcher arm rewritten:
   - Unqualified (0 claims): dispatches as before (back-compat).
   - Unqualified (1 claim): dispatches + System hint to use
     `/<command> toggle`.
   - Unqualified (2+ claims): Error with disambiguation listing plugin
     ids and per-plugin commands; returns `None`.
   - Qualified `/sidecar <plugin-id> <toggle|status>`: looks up claim
     by plugin id; unknown id errors with the loaded list. Subcommand
     `toggle`/`status` dispatches no-payload variant (TODO for 8B
     plugin-id payload).
7. ✅ **8A.8 (`d864f1b`)** — `migrate_sidecar_toggle_key_to_claimed_plugins()`
   runs at startup. For each claim with a `settings_category`, copies
   `sidecar_toggle_key` into `plugins.{plugin}.{cat}._lifecycle_toggle_key`
   when the new key isn't already set. Idempotent.

**Tests:**
- Manifest deserialization: `lifecycle` parses with all fields, with
  none, with partial fields.
- Command registry: `/voice toggle` resolves to `SidecarToggle` action
  when plugin claims `command: "voice"`.
- Settings: toggle-key field appears in plugin's category; absent from
  default Sidecar category when claimed.
- Fallback: with no plugin claiming, `/sidecar toggle` still works.
- Ambiguity: integration test with two unclaimed plugins → unqualified
  command errors with disambiguation hint.

**Files touched (estimated):**
- `src/skills/manifest.rs` (+30 LOC for lifecycle struct)
- `src/extensions/manager.rs` (lifecycle registration on load) (+40)
- `src/chatui/commands.rs` (qualified `/sidecar <plugin>` form) (+30)
- `src/chatui/settings/defs.rs` (drop `Category::Sidecar` when claimed) (-15)
- `src/chatui/draw.rs` (pill uses `display_name`) (+10/-5)
- `src/chatui/sidecar.rs` (status_line uses display_name) (+5/-2)
- Tests: `+100`

**Net core LOC: ~+150** (driven by tests; logic is small)

**Scope:** M

### Slice 8B — Multiple concurrent sidecars

**Steps:**

1. ✅ **8B.1 (`91d4763`)** — `App.sidecar: Option<SidecarUiState>` →
   `App.sidecars: HashMap<String, SidecarUiState>` keyed by plugin id.
   `discover_all_in()` already exists (8A.1); chatui spawn handler
   targets one entry per plugin lazily. New
   `SidecarUiState::spawn_for(discovered, ...)` lets the dispatcher pick
   a specific plugin instead of always picking `discover()[0]`.
2. ✅ **8B "payload"** (`4a4e5fd`) —
   `CommandAction::SidecarToggle { plugin_id: Option<String> }` /
   `SidecarStatus { plugin_id: Option<String> }`. Lifecycle-claim arm
   passes `Some(claim.plugin)`; qualified `/sidecar <pid> ...` passes
   `Some(pid)`; bare `/sidecar` (0 claims) and the deprecated `/voice`
   alias pass `None` (back-compat for legacy single-sidecar).
3. ✅ **8B.3 + 8B.4** (`91d4763`,`f8fcbe4`) — pill renderer iterates
   `App.sidecars` sorted by `importance` desc then `display_name`
   alphabetical. Pure helper `order_sidecar_pills(inputs, claims)`
   extracted for testability. Event loop uses
   `futures::future::select_all` over `app.sidecars.iter_mut()`,
   tagging each `next_event()` with its plugin id;
   `chatui::sidecar::handle_event(app, &plugin_id, event)` routes by
   key. `Exited` events remove a single entry, not the whole map.
4. ✅ **8B.5** (covered by 8A.1) — `discover_all_in` already returns
   every sidecar.
5. ✅ **8B step 7** (`31f74ef` — already merged) — `KeybindRegistry`
   records `KeybindCollision { losing_plugin, key, winning_owner,
   reason }`. First-loaded wins. The new `SidecarToggle { plugin_id:
   Some(claim.plugin) }` payload flows through the keybind→slash-command
   path, so `keybinds[]` entries pointing at `<command> toggle` correctly
   target their own plugin's sidecar.
6. ⏳ **8B step 8** (deferred, one-release back-compat window) —
   drop the global `sidecar_toggle_key` setting. Migration is already
   in place (8A.8 copies into plugin namespace); the actual deletion of
   the legacy key is left for the next major version.

**Tests:**
- Two plugins with sidecars → both discovered, both in `App.sidecars`.
- Toggle plugin A → only A's state changes; B remains as it was.
- Pill ordering: `importance: 100` plugin renders before `importance: 0`;
  ties resolve alphabetically by `display_name`.
- Final transcripts route to the correct `SidecarUiState` (the one
  that emitted them).
- Two plugins both declaring `ctrl+space`: first wins; second's
  `loaded_with_warnings` flag set; warning in `/extensions`.
- Per-plugin status: `/voice status` and `/ocr status` show only their
  own state.

**Files touched (estimated):**
- `src/sidecar/discovery.rs` (+30)
- `src/sidecar/manager.rs` (no change — already per-instance)
- `src/chatui/app.rs` (HashMap migration) (+15/-5)
- `src/chatui/sidecar.rs` (event routing by plugin id) (+30/-10)
- `src/chatui/draw.rs` (multi-pill rendering) (+25/-15)
- `src/chatui/commands.rs` (CommandAction payload extension) (+30/-10)
- `src/skills/keybinds.rs` (overlap detection) (+25)
- Tests: `+200`

**Net core LOC: ~+300** (mostly tests and the multi-pill renderer)

**Scope:** L

## Risks

| Risk | Mitigation |
|---|---|
| Existing local-voice plugin breaks because manifest doesn't have `lifecycle` | Slice 8A keeps the unclaimed code path live; plugin works as before until its manifest is updated. |
| User has bound `sidecar_toggle_key` and migration moves it to the wrong namespace | Only migrate when exactly one claimed plugin exists. Otherwise leave the global key and emit a one-time deprecation warning. |
| Multi-pill renderer overflows narrow terminals | Truncate pill segments, prefix the most-important one, show overflow count. Test with 80-col terminal and 5 sidecars. |
| Two plugins racing to register the same `<command> toggle` | Conflict at registry-insert time → second registration rejected with the same overlap-warning treatment as keybinds. |
| Plugin authors abuse `importance` to grab the leftmost pill spot | Document range (`-100..=100`); core caps higher values silently. Cosmetic; no security impact. |

## Verification per slice

After each slice:
```bash
cargo build --all-targets 2>&1 | tail -5
cargo test --all-targets -- --test-threads=1 2>&1 | grep -E '^test result:'
```

After Slice 8A, additionally:
```bash
# load local-voice (after its manifest gets the lifecycle block) and confirm:
synaps  # /voice toggle works, no Category::Sidecar visible, pill says "Voice"
```

After Slice 8B, additionally:
```bash
# Smoke: load 2 sidecar-providing plugins simultaneously, toggle each
# independently, confirm pill shows both, confirm keybind overlap warning.
```

## Decision log (from review)

- **Q1 — `/sidecar` removal vs. fallback?** Keep as fallback, but
  ambiguity-aware: errors with disambiguation hint when multiple
  unclaimed sidecars exist. (User: "play it safe.")
- **Q2 — Toggle key per-plugin?** Yes. Overlap rule: first-loaded
  wins, subsequent registrations are *rejected with a visible warning*
  surfaced in `/extensions` and the chat log. No silent overrides.
- **Q3 — Pill ordering?** `importance` descending (default 0,
  range `-100..=100`), ties broken alphabetically by `display_name`.

## Coordination with synaps-skills

The synaps-skills repo's `local-voice-plugin/.synaps-plugin/plugin.json`
needs three additions for Slice 8A to take effect:

```jsonc
"provides": {
  "sidecar": {              // (or keep "voice_sidecar" for compat — both still parse)
    "command": "bin/synaps-voice-plugin",
    "setup": "scripts/setup.sh",
    "protocol_version": 1,
    "model": { ... },
    "lifecycle": {                          // ◄── new
      "command": "voice",
      "settings_category": "voice",
      "display_name": "Voice",
      "importance": 50
    }
  }
},
"keybinds": [                               // ◄── new (was empty)
  { "key": "ctrl+space", "command": "voice toggle" }
],
"commands": [
  {
    "name": "voice",
    "subcommands": [
      "toggle", "status",                   // ◄── new (auto-registered, but
                                            //    listing them here makes the
                                            //    palette + help discoverable)
      "help", "models", "download", "rebuild"
    ]
  }
]
```

These changes are *additive* — the plugin keeps working with the
current core if a user updates the plugin first, and keeps working
with the new core if a user updates core first. No flag day required.

## Phasing

Slice 8A is the user-visible win. Slice 8B is the architectural win
(unlocks multi-sidecar plugins). Recommend landing 8A first and
shipping it standalone before starting 8B; users get the cleaner
single-sidecar UX immediately, and 8B can be designed against real
usage feedback.

---

## Status (2026-05-05)

**Slice 8A: shipped.** All 8 sub-slices landed (8A.1 through 8A.8) on
branch `feat/path-b-phase7-and-8`. Lifecycle claims, ambiguity-aware
`/sidecar`, hidden `Category::Sidecar`, virtual toggle-key field,
display-name plumbing, qualified `/sidecar <plugin> ...`, and the
startup migration for `sidecar_toggle_key` are all in.

**Slice 8B: shipped (with one deferral).** `App.sidecars` is a HashMap
keyed by plugin id; `CommandAction::SidecarToggle` / `SidecarStatus`
carry `plugin_id: Option<String>`; multi-segment pill ordered by
importance desc + alphabetical tiebreak; `KeybindCollision` recorded
on the keybind registry; per-plugin keybind dispatch auto-targets the
correct plugin. Step 8 — *drop the global `sidecar_toggle_key`
setting* — is deferred to the next major version; the back-compat
migration shim (8A.8) keeps existing user configs working.

**Sibling repo: shipped.** `local-voice-plugin/.synaps-plugin/plugin.json`
adopts the new shape (commit `d946848` on
`feat/local-voice-plugin-commands-tasks`):

- `provides.voice_sidecar` → `provides.sidecar` (canonical name)
- `provides.sidecar.lifecycle = { command: "voice", settings_category: "voice", display_name: "Voice", importance: 50 }`
- `keybinds = [{ key: "C-Space", action: "slash_command", command: "voice toggle", description: ... }]`

Pinned by `tests::local_voice_plugin_json_parses_with_phase8_lifecycle_and_keybinds`
in `src/skills/manifest.rs`.

## Remaining follow-ups (not in scope for this PR)

1. **`/extensions` UI surfacing of collisions.** The plumbing exists:
   - `KeybindRegistry::collisions() -> &[KeybindCollision]`
   - `CommandRegistry::lifecycle_claim_collisions() -> &[(String, String, String)]`
   Today these are only logged via `tracing::warn!`. The `/extensions`
   command should render both lists so users can see *why* their
   keybind / lifecycle command isn't taking effect. Pure render work;
   no schema or dispatch changes.

2. **Next-major shim removal.** Drop in one batch when bumping the
   plugin protocol major version:
   - `LegacyVoiceCapability` + handler in `src/skills/capabilities.rs`
   - `migrate_legacy_voice_config_keys()` (config startup)
   - `migrate_legacy_sidecar_toggle_key()` (settings startup)
   - `voice_toggle_key` fallback reads
   - `/voice` builtin alias in `CommandRegistry::builtin_commands()`
   - `#[deprecated] type VoiceSidecarManifest = SidecarManifest`
   - `#[serde(alias = "voice_sidecar")]` on `PluginProvides.sidecar`
   - The global `sidecar_toggle_key` setting (8B step 8)
   Each shim has a `// TODO(next-major):` or `#[deprecated]` marker
   already; `rg "next-major|TODO\(phase 8\)|TODO\(deprecated\)"`
   should enumerate them.
