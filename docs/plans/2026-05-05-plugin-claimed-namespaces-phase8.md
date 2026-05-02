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

1. Extend `SidecarManifest` with optional `lifecycle: Option<SidecarLifecycle>`
   carrying `command`, `settings_category`, `display_name`, `importance`.
2. At extension load time, when a plugin declares `provides.sidecar.lifecycle`:
   - Dynamically register `<command> toggle` and `<command> status` as
     plugin-aliased lifecycle commands. Internally they dispatch to
     the existing `CommandAction::SidecarToggle` / `SidecarStatus`.
   - Inject a virtual setting field (toggle-key) into the named
     `settings_category`. The field is owned by core but rendered
     under the plugin's panel.
3. Settings UI: hide `Category::Sidecar` when at least one plugin
   has claimed a category. (Strict: hide always once any plugin
   claims; if zero plugins claim, show the legacy panel.)
4. Pill / status line / error messages use the plugin's `display_name`
   when known; fall back to `"sidecar"` only when no claim exists.
5. `/sidecar toggle` becomes an ambiguity-aware fallback:
   - 0 unclaimed plugins: command hidden from palette, errors with
     "no sidecar to toggle".
   - 1 unclaimed plugin: command toggles it.
   - 2+ unclaimed plugins: command errors with "multiple unclaimed
     sidecars: <list>; use `/sidecar <plugin-id> toggle`".
6. Add `/sidecar <plugin-id> toggle/status` qualified form.
7. Migration: keep `Category::Sidecar` and `sidecar_toggle_key` as
   one-release fallback. Add a startup migration that copies
   `sidecar_toggle_key` into the active claimed plugin's namespace
   when one exists.

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

1. `discovery::discover_all() -> Vec<DiscoveredSidecar>`. Keep
   `discover()` as `discover_all().first()` for one release.
2. `App.sidecar: Option<SidecarUiState>` → `App.sidecars: HashMap<String, SidecarUiState>`
   keyed by plugin id.
3. `CommandAction::SidecarToggle` / `SidecarStatus` carry an
   `Option<String>` plugin-id payload. The dispatcher uses it to pick
   the target sidecar; `None` means "the unique unclaimed one" (slice 8A).
4. Plugin-claimed lifecycle commands (from 8A) auto-bind to the right
   plugin id at registration time — no ambiguity.
5. Pill renderer iterates `App.sidecars` sorted by:
   1. `importance` descending (default 0)
   2. `display_name` alphabetical
6. Each `SidecarLifecycleEvent` carries the originating plugin id so
   the chatui routes it to the right `SidecarUiState`.
7. Per-plugin keybinds (declared in plugin manifest's `keybinds`):
   - Loaded into the keybind registry on plugin load.
   - **Overlap rule:** if a key is already bound, the second plugin's
     binding is rejected. A `tracing::warn!` is logged, a warning is
     surfaced in `/extensions` (one-line indicator next to the
     conflicting plugin), and the chat log gets a one-time system
     message at startup. The user can re-bind via the keybind editor
     to resolve. **No silent overrides.**
8. Drop the global `sidecar_toggle_key` setting. Migration: at
   startup, if found and there's exactly one plugin with a claimed
   lifecycle, write it into that plugin's namespace and delete the
   global key.

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
