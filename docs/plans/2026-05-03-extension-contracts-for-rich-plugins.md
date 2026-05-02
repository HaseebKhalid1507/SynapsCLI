# Extension Contracts for Rich Plugins (Path B)

**Status:** Draft
**Date:** 2026-05-03
**Author:** voice-integration squad
**Convergence mode:** `none` — six discrete phases, each shippable as its
own PR, each with explicit acceptance gates. Human reviews every phase.

## Goal

Build the missing Phase 2 extension contracts so that **a third-party plugin
developer can replicate the entire whisper model-manager experience without
editing a single line of synaps-cli core**. Then refactor the existing
whisper-specific code out of `src/voice/` and into the
`local-voice-plugin` to prove the contracts work.

The end state: synaps-cli core knows nothing about whisper, ggml, cmake,
HuggingFace catalogs, CUDA / Metal / Vulkan, or any other STT-engine
implementation detail. Synaps owns the *generic* affordances (sidecar
lifecycle, voice pill, F8 toggle, transcript insertion); the plugin owns
*everything specific to its STT engine*.

## Non-Goals (this plan)

- **Replacing the entire Phase 2 extension manager.** We extend it; we
  don't rewrite it.
- **Building a second voice plugin.** Reference impl stays as
  `local-voice-plugin`. (Optional Phase 7 sketches a vosk stub.)
- **GUI editor for plugin manifests.** Manifests stay TOML-edited.
- **Changing the JSON-RPC transport.** Same line-JSON over stdio that
  Phase 2 already uses for hooks.
- **Sandbox / capability tightening beyond what Phase 2 already does.**
  Plugin-driven UI events are subject to the same trust checks as
  hooks today.
- **Hot-loading new plugins without restart.** Plugins still load at
  boot or on `/extensions reload`.

## The Drift This Plan Reverses

As of `8a0e142`, synaps-cli core contains:

| Code in core | Should live in | Reason |
|---|---|---|
| `src/voice/models.rs` (10-entry whisper catalog, SHA256s, HF URLs) | plugin | Pure whisper-specific data |
| `src/voice/download.rs` (streaming HTTP, atomic install) | plugin | Only caller is the whisper catalog |
| `src/voice/discovery.rs::probe_cuda/metal/vulkan/openblas + detect_host_backend` | plugin | These are whisper-rs cargo feature names |
| `src/voice/rebuild.rs` (`cargo clean -p whisper-rs-sys` + `setup.sh --features`) | plugin | String literal `whisper-rs-sys` in core; shells out to plugin's setup.sh anyway |
| `/voice models`, `/voice download`, `/voice rebuild` slash command parsing & rendering | plugin | User-facing affordances for whisper-specific concepts |
| Settings → Voice → STT model `EditorKind::ModelBrowser` | plugin | Renders the whisper catalog |
| Settings → Voice → STT backend cycler (`auto/cpu/cuda/metal/vulkan/openblas`) | plugin | These are whisper-rs feature names |
| Config keys `voice_stt_model_path`, `voice_stt_backend`, `voice_stt_model`, `voice_language` | plugin (under `local-voice.*` namespace) | Plugin-specific |
| `App::download_progress`, `download_filename`, `download_rx`, `cached_voice_compiled_backend` | generic core (renamed `active_tasks`, `cached_plugin_capabilities`) | Should be generic, not voice-specific |
| `render_download_progress_line()` (tqdm bar) | core (kept generic) | Generic chrome over plugin task data |

Total to move/refactor: ~1,500 LOC out of core, ~1,800 LOC into plugin,
~700 LOC of new contract code.

## What Stays in Core (the genuinely-generic parts)

- `src/voice/protocol.rs` — line-JSON wire format
- `src/voice/manager.rs` — sidecar lifecycle (spawn, supervise, restart)
- `voice_pill_span` in chat header — generic UX for "voice is active"
- F8 / `voice_toggle_key` keybind — generic toggle UX
- `insert_transcript()` — cursor-aware text insertion
- Phase 2 contracts themselves (slash commands, settings, keybinds, hooks)
- All chat UI rendering (markdown, wrap, tab-anchor, etc.)

## Convergence Decision

`convergence: none`.

- Each phase is independently shippable; failure of one doesn't strand others
- Contracts are additive (new RPC methods); no breaking changes to existing
  hook contract
- Human reviewer can validate each phase by running the smoke-test gate
  before approving

If a phase touches trust-boundary code (Phase 2's command/output channel
in particular — plugin can now push arbitrary System messages), we raise
to `convergence: pair` for that single phase.

---

## Phased Plan (6 phases)

### Phase 1 — Plugin-namespaced config + hot-reload

**Goal:** plugins can read/write their own config under
`~/.synaps-cli/plugins/<id>/config` with hot-reload notification, so we
can stop colonizing the global `~/.synaps-cli/config` keyspace.

**Scope:**

- New `src/extensions/config_store.rs` with `read_plugin_config(id, key)`,
  `write_plugin_config(id, key, value)`, `subscribe_changes(id) -> Receiver`
- File watcher on `~/.synaps-cli/plugins/<id>/config` (notify crate;
  already a transitive dep)
- New JSON-RPC methods on the extension protocol:
  - `config.get { key } -> { value: Option<String> }`
  - `config.set { key, value } -> { ok: bool }`
  - `config.subscribe { keys: Vec<String> }` — server-push: `config.changed { key, value }`
- Manifest schema additions (optional, declarative):
  ```toml
  [plugin.config]
  schema = [
    { key = "model_path", type = "string", default = "" },
    { key = "backend",    type = "string", default = "auto" },
  ]
  ```
- **Back-compat shim:** `voice_stt_model_path`, `voice_stt_backend`,
  `voice_stt_model`, `voice_language` continue to be readable from the
  global config for one release; plugin code reads via
  `config.get_with_legacy_fallback(...)`. Deprecation warning logged on
  fallback hit.
- Settings panel "Plugins" tab gets a per-plugin sub-view that lists the
  schema fields and lets the user edit them.

**Acceptance gates:**

- [ ] Unit tests for config_store (read, write, watch-notify, ignore non-watched keys)
- [ ] Integration test: spawn a stub plugin, plugin calls `config.set`,
      synaps process reads the value back and the file on disk reflects it
- [ ] Integration test: synaps writes to `~/.synaps-cli/plugins/foo/config`,
      plugin receives `config.changed` event within 200ms
- [ ] Existing voice config keys still resolve (back-compat smoke)
- [ ] `/extensions config foo` shows the per-plugin schema

**Estimated scope:** ~450 LOC + ~150 LOC tests
**Files touched (core):** `src/extensions/{config_store,manager}.rs`,
`src/extensions/manifest.rs`, `src/chatui/settings/plugins.rs`,
`src/extensions/runtime/process.rs`
**Risk:** low — additive; back-compat shim covers existing users

---

### Phase 2 — Plugin-driven slash commands ("interactive commands")

**Goal:** plugins can register slash commands (and subcommands) whose
execution is handed off to the plugin process, which streams structured
output events back. Synaps renders each event according to its kind.

**Scope:**

- New `ManifestCommand::Interactive` variant:
  ```toml
  [[commands]]
  name = "voice"
  description = "Voice dictation controls"
  interactive = true                # hands /voice off to the plugin
  subcommands = ["models", "download", "rebuild", "help"]
  ```
- New JSON-RPC method:
  - `command.invoke { command, args, request_id } -> stream of command.output { request_id, event }`
- Output event kinds:
  ```jsonc
  { "kind": "text",   "content": "..." }       // markdown-rendered as ChatMessage::Text
  { "kind": "system", "content": "..." }       // ChatMessage::System (wrapped)
  { "kind": "error",  "content": "..." }       // ChatMessage::Error
  { "kind": "table",  "headers": [...], "rows": [[...], ...] }  // table widget
  { "kind": "done" }                           // signals end-of-stream
  ```
- Synaps renders each event as it arrives; UI stays responsive
- **Refactor proof point:** move `/voice models` and `/voice help`
  plugin-side. Remove the corresponding code from
  `src/chatui/commands.rs`. The plugin's manifest registers the
  command; the plugin process handles `command.invoke`; synaps just
  renders.
- `EditorKind::ModelBrowser` stays in core but is fed by the plugin
  (Phase 4 finishes that move)

**Acceptance gates:**

- [ ] Unit tests: each event kind round-trips through serde correctly
- [ ] Integration test: stub plugin emits 5 mixed events; main loop
      pushes them in order to App messages
- [ ] `/voice models` works end-to-end via the plugin (no regression)
- [ ] `/voice help` works end-to-end via the plugin
- [ ] Removing the in-core handlers for those two commands is part of
      the same PR
- [ ] Bin tests pass (current 158 + new event-handling cases)

**Estimated scope:** ~700 LOC core + ~200 LOC plugin + ~200 LOC tests
**Files touched (core):** `src/extensions/runtime/process.rs` (new method
dispatch), `src/skills/manifest.rs` (Interactive variant), `src/skills/registry.rs`
(routing), `src/chatui/mod.rs` (event dispatch), `src/chatui/commands.rs`
(remove voice-models / voice-help arms)
**Files touched (plugin):** `local-voice-plugin/src/commands/{models,help}.rs`,
`local-voice-plugin/src/main.rs` (rpc dispatch wiring)
**Risk:** medium — new JSON-RPC streaming pattern; need to test
backpressure (plugin emits faster than UI renders)

---

### Phase 3 — Plugin-driven long-running tasks (sticky progress bar)

**Goal:** plugins can publish long-running tasks (downloads, rebuilds,
indexing jobs) outside the slash-command request/response cycle. Synaps
renders each as a sticky progress widget. Multiple concurrent tasks
allowed.

**Scope:**

- New JSON-RPC server-push events (no client method; plugin pushes
  spontaneously):
  ```jsonc
  { "method": "task.start",  "params": { "id": "dl-base", "label": "Downloading ggml-base.bin", "kind": "download" } }
  { "method": "task.update", "params": { "id": "dl-base", "current": 50000000, "total": 142000000 } }
  { "method": "task.log",    "params": { "id": "dl-base", "line": "..." } }       // for rebuilds (subprocess output)
  { "method": "task.done",   "params": { "id": "dl-base", "error": null } }
  ```
- Generic `App::active_tasks: HashMap<String, TaskState>` replaces the
  current voice-specific `download_progress` etc.
- `render_download_progress_line()` becomes `render_task_line()` and
  iterates tasks by ID
- Sticky bar now stacks vertically when multiple tasks are active (e.g.,
  download + concurrent rebuild)
- Layout: replace the single-line download slot with a `Min(0)` slot
  that grows to N lines for N active tasks (capped at 4 lines)
- Task `kind` enum: `download` (shows bar + bytes), `rebuild` (shows
  spinner + last log line), `generic` (shows label + spinner)
- For rebuild output specifically: `task.log` lines NOT pushed to chat;
  they go to the sticky panel which can be expanded with a keybind
  (default: F4) to a 50%-screen overlay (separate sub-deliverable;
  optional in this phase)
- **Refactor proof point:** move `/voice download <id>` plugin-side.
  Plugin handles the slash command (Phase 2 contract), spawns the
  download internally, publishes `task.*` events. Synaps just renders.
- Same for `/voice rebuild`: plugin handles command, runs `cargo clean
  -p whisper-rs-sys && bash setup.sh --features ...`, streams subprocess
  output as `task.log`, finishes with `task.done`.

**Acceptance gates:**

- [ ] Unit tests for `App::active_tasks` (insert, update, complete, error)
- [ ] Snapshot test for `render_task_line()` for each kind
- [ ] Multi-task test: 2 concurrent tasks render correctly without overlap
- [ ] Integration: stub plugin emits start→update×5→done sequence;
      App state matches at each tick
- [ ] `/voice download base` works plugin-side end-to-end
- [ ] `/voice rebuild cpu` works plugin-side end-to-end
- [ ] Removing core's `src/voice/{models,download,rebuild}.rs` is part
      of this PR (or Phase 6 — see decision below)
- [ ] No blocking calls in main event loop

**Estimated scope:** ~600 LOC core + ~500 LOC plugin + ~250 LOC tests
**Decision point:** do we delete `src/voice/{models,download,rebuild}.rs`
in Phase 3 or wait until Phase 6? **Defer to Phase 6** so Phase 3 ships
the contract + plugin reimplementation, leaving core as fallback. Phase 6
is the cleanup pass.
**Risk:** medium — sticky bar layout interactions with subagent panel +
input box need careful constraint math

---

### Phase 4 — Plugin settings categories with custom editors

**Goal:** plugins can register their own Settings categories with custom
editor kinds. The plugin renders the editor body via JSON-RPC events
similar to Phase 2's command output.

**Scope:**

- Manifest schema:
  ```toml
  [[settings.category]]
  id = "voice"
  label = "Voice"
  fields = [
    { key = "model_path",  label = "STT model",   editor = "custom" },
    { key = "backend",     label = "STT backend", editor = "cycler", options = ["auto", "cpu", "cuda", "metal", "vulkan", "openblas"] },
    { key = "language",    label = "Voice language", editor = "cycler", options = [...] },
  ]
  ```
- Built-in editor kinds (no plugin involvement): `text`, `cycler`, `picker`
- New `editor = "custom"` opens a plugin-rendered overlay
- JSON-RPC contract for custom editors:
  ```jsonc
  // synaps → plugin
  { "method": "settings.editor.open", "params": { "category": "voice", "field": "model_path" } }
  // plugin → synaps (server-push, repeated as state changes)
  { "method": "settings.editor.render", "params": {
      "rows": [
        { "label": "tiny.en (75 MB)",   "marker": "✓", "selectable": true,  "data": "/abs/path/.bin" },
        { "label": "base (142 MB)",     "marker": " ", "selectable": true,  "data": "download:base" },
        ...
      ],
      "cursor": 2,
      "footer": "↓ Up/Down  Enter to select or download  Esc to cancel"
  } }
  // synaps → plugin (on each key press)
  { "method": "settings.editor.key", "params": { "key": "Down" } }
  // plugin → synaps (when user hits Enter)
  { "method": "settings.editor.commit", "params": { "value": "/abs/path/.bin" } }
  // synaps writes the value via existing config path; closes editor
  ```
- **Refactor proof points:**
  - Move Settings → Voice → STT model from `EditorKind::ModelBrowser`
    to plugin-rendered `editor = "custom"`. Remove `ModelBrowser`
    variant from core.
  - Move Settings → Voice → STT backend cycler — declarative in
    manifest; remove from `defs.rs`.
  - Move Settings → Voice → Voice language cycler — declarative in
    manifest; remove from `defs.rs`.
- Synaps' built-in Settings categories remain unchanged

**Acceptance gates:**

- [ ] Unit tests for declarative cycler/picker/text editors registered
      via manifest
- [ ] Integration test: stub plugin renders custom editor; key events
      round-trip; commit writes config
- [ ] `/settings → Voice → STT model` works plugin-side (catalog
      browser, Enter-to-download path intact)
- [ ] `/settings → Voice → STT backend` cycler works (purely declarative)
- [ ] `/settings → Voice → Voice language` cycler works
- [ ] Removing `EditorKind::ModelBrowser`, `WhisperModelPicker`, and
      the three voice-* setting defs from core is part of this PR
- [ ] Snapshot tests for the custom-editor overlay frame
- [ ] No flicker on rapid key events (debounce render events)

**Estimated scope:** ~900 LOC core + ~400 LOC plugin + ~300 LOC tests
**Files touched (core):** `src/skills/manifest.rs`, `src/chatui/settings/{schema,defs,mod,draw,input}.rs`,
new `src/chatui/settings/plugin_editor.rs`
**Files touched (plugin):** `local-voice-plugin/src/settings/{model_browser,backend,language}.rs`,
plugin main loop dispatch
**Risk:** medium-high — settings UI is dense; custom-editor protocol
needs careful design to avoid flicker / partial-render glitches

---

### Phase 5 — Capability advertisement RPC

**Goal:** replace the ad-hoc `--print-build-info` stdin shim with a
proper JSON-RPC method. Plugins can advertise structured capability
metadata that synaps can query.

**Scope:**

- New JSON-RPC method (synaps → plugin):
  ```jsonc
  { "method": "info.get", "params": {} }
  // plugin response:
  {
    "result": {
      "build": { "backend": "cpu", "features": ["local-stt"], "version": "0.1.0" },
      "capabilities": {
        "supports_streaming": true,
        "supported_languages": ["auto", "en", "es", ...],
        "supported_backends": ["cpu", "cuda", "metal", "vulkan", "openblas"]
      },
      "models": [
        { "id": "tiny.en", "label": "tiny.en (75 MB)", "installed": true },
        ...
      ]
    }
  }
  ```
- Synaps caches the result on first plugin spawn; refreshes on demand
  (after `/voice rebuild` completes, or via explicit `info.refresh`)
- Replaces:
  - `voice/discovery.rs::read_build_info()` (which spawns a subprocess
    per call) with a cached RPC query
  - `App::cached_voice_compiled_backend` with generic
    `App::plugin_capabilities: HashMap<String, PluginCapabilities>`
- Plugin's `--print-build-info` flag is **deprecated but kept** for one
  release for back-compat (existing pre-RPC binaries still work)

**Acceptance gates:**

- [ ] Unit tests for `info.get` request/response round-trip
- [ ] Integration test: plugin returns capabilities; synaps caches them;
      Settings → Voice → "Current build: cpu" annotation reflects cache
- [ ] Cache invalidation works after `/voice rebuild`
- [ ] Back-compat: a plugin that only implements `--print-build-info`
      still functions (synaps falls back to the subprocess shim)
- [ ] Removing `App::cached_voice_compiled_backend` field (renamed to
      generic `plugin_capabilities`)

**Estimated scope:** ~350 LOC core + ~150 LOC plugin + ~150 LOC tests
**Files touched (core):** `src/extensions/runtime/process.rs`,
`src/voice/discovery.rs` (slim down), `src/chatui/app.rs`,
`src/chatui/settings/draw.rs`
**Files touched (plugin):** `local-voice-plugin/src/info.rs` (new),
`local-voice-plugin/src/main.rs` (RPC dispatch)
**Risk:** low — single new RPC method; back-compat path preserves
working users

---

### Phase 6 — The Big Move: refactor whisper-specifics out of core

**Goal:** delete every whisper-specific file from synaps-cli. Verify the
plugin can stand on its own using only Phases 1–5 contracts.

**Scope (deletions from core):**

- `src/voice/models.rs` — gone
- `src/voice/download.rs` — gone (moved to `local-voice-plugin/src/download.rs`)
- `src/voice/rebuild.rs` — gone (moved to `local-voice-plugin/src/rebuild.rs`)
- `src/voice/discovery.rs` — slim to ~30 LOC: only `discover()` (find
  the plugin manifest); delete `read_build_info`, `detect_host_backend`,
  `probe_*`
- `src/chatui/commands.rs` — delete `voice_models`, `voice_download`,
  `voice_rebuild`, `voice_help`, `render_models_table`,
  `voice_models_dir`, `voice_help_text` (~250 LOC removed)
- `src/chatui/mod.rs` — delete the `CommandAction::VoiceModels`,
  `VoiceDownload`, `VoiceRebuild`, `VoiceHelp` arms (~200 LOC removed)
- `src/chatui/settings/schema.rs` — delete `EditorKind::ModelBrowser`,
  `WhisperModelPicker`
- `src/chatui/settings/defs.rs` — delete `voice_stt_model`,
  `voice_stt_backend`, `voice_language` definitions (the *plugin*
  registers these now via Phase 4)
- `src/chatui/settings/{mod,draw,input}.rs` — delete
  `model_browser_rows`, `ActiveEditor::ModelBrowser`,
  `render_model_browser`, `render_whisper_model_picker`, related
  routing (~400 LOC removed)
- `src/chatui/app.rs` — delete `download_progress`, `download_filename`,
  `download_rx`, `voice_download_in_flight`, `cached_voice_compiled_backend`,
  `model_browser_selected`, `start_download`, `on_download_progress`,
  `on_download_complete` (replaced by generic `active_tasks` from Phase 3)
- `src/chatui/draw.rs` — `render_download_progress_line` becomes
  `render_task_line` (already done in Phase 3, but call sites updated)
- Config: deprecation removal of `voice_stt_*`, `voice_language` from
  global config keys (with a one-time migration on startup that copies
  to `~/.synaps-cli/plugins/local-voice/config`)

**Scope (additions to plugin):**

- `local-voice-plugin/src/models.rs` — the catalog (moved from core)
- `local-voice-plugin/src/download.rs` — the downloader (moved from core)
- `local-voice-plugin/src/rebuild.rs` — the rebuild orchestrator
- `local-voice-plugin/src/discovery.rs` — `detect_host_backend` +
  `probe_*` (moved from core)
- `local-voice-plugin/src/commands/{models,download,rebuild,help}.rs`
  — slash command handlers using Phase 2 contract
- `local-voice-plugin/src/settings/{model_browser,backend,language}.rs`
  — settings editors using Phase 4 contract
- `local-voice-plugin/src/info.rs` — capability advertisement (Phase 5)
- `local-voice-plugin/src/tasks.rs` — task event publisher (Phase 3)
- Updated `local-voice-plugin/plugin.toml` — declares all the
  commands/settings/config schema

**Acceptance gates (the big one):**

- [ ] **Net LOC**: synaps-cli loses ≥ 1,000 LOC; local-voice-plugin
      gains ≥ 1,500 LOC (plugin code can be more verbose than the
      tightly-coupled core code it replaces)
- [ ] **Smoke test parity**: every workflow from the model-manager
      Checkpoint 2 still works:
      - `/voice models` lists installed/uninstalled
      - `/voice download base` downloads with progress bar
      - `/voice rebuild cpu` rebuilds with streaming output
      - `/settings → Voice → STT model` opens browser, Enter downloads
      - `/settings → Voice → STT backend` cycler works
      - "Current build: cpu" annotation present
- [ ] **`grep -r whisper src/` in synaps-cli**: zero hits except
      `src/voice/protocol.rs` (which uses generic terms anyway)
- [ ] **`grep -r ggml src/` in synaps-cli**: zero hits
- [ ] **`grep -r huggingface src/` in synaps-cli**: zero hits
- [ ] **Reference-implementation smoke**: a deliberately broken plugin
      (e.g. one that only supports `info.get` and nothing else) loads
      and synaps degrades gracefully (Settings → Voice category just
      shows "(no fields registered)" instead of crashing)
- [ ] All 915+ existing tests still green
- [ ] Migration: old `~/.synaps-cli/config` voice keys auto-copy to
      plugin namespace on first launch; old keys logged as deprecated
      and removed after the next clean save

**Estimated scope:** ~−1,500 LOC core, +1,800 LOC plugin, +400 LOC migration
**Files touched (core):** ~12 files (mostly deletions)
**Files touched (plugin):** ~15 new files
**Risk:** **high** — large refactor; breaking change for power users
who edited their config directly. Mitigated by:
  - Migration shim on first launch
  - Bumping plugin protocol version (so plugins built against old
    contracts get a clear error)
  - Manual smoke gate before merge

---

### Phase 7 — Documentation & generalization (lightweight)

**Goal:** document the new contracts so a third party can write a
voice plugin without reverse-engineering local-voice-plugin.

**Scope:**

- `docs/extensions/contracts.md` — full API reference for Phases 1–5
  contracts with worked examples
- `docs/extensions/writing-a-voice-plugin.md` — tutorial walking
  through building a minimal voice plugin from scratch
- `docs/extensions/migration-pre-phase6.md` — what changed for
  existing plugin authors
- Update `CHANGELOG.md` with breaking-changes section
- *(Optional)* Stub `vosk-voice-plugin` in `synaps-skills/` that uses
  the same contracts but a different STT engine — proves the
  architecture isn't accidentally whisper-coupled. Not a full
  implementation; just enough to register `/voice models` (returns
  Vosk catalog) and prove the contracts are generic.

**Acceptance gates:**

- [ ] All public RPC methods documented with request/response examples
- [ ] Tutorial walks through writing a minimal "echo" voice plugin
- [ ] CHANGELOG cleanly summarizes the migration path
- [ ] *(Optional)* Vosk stub plugin builds and synaps recognizes it

**Estimated scope:** ~600 LOC docs (markdown) + optional ~300 LOC vosk stub
**Risk:** none

---

## Summary Table

| Phase | What | Core LOC | Plugin LOC | Test LOC | Risk |
|-------|------|----------|------------|----------|------|
| 1 | Plugin config + hot-reload | +450 | +50 | +150 | low |
| 2 | Interactive slash commands | +700 | +200 | +200 | medium |
| 3 | Long-running tasks + sticky bar | +600 | +500 | +250 | medium |
| 4 | Plugin settings categories | +900 | +400 | +300 | medium-high |
| 5 | Capability advertisement RPC | +350 | +150 | +150 | low |
| 6 | The Big Move (deletions) | −1,500 | +1,800 | +400 | **high** |
| 7 | Docs + (opt) vosk stub | 0 | (+300 opt) | 0 | none |
| **Total** | | **+1,500 net** | **+3,400** | **+1,450** | |

Net synaps-cli core *grows* by ~1,500 LOC despite the deletions in Phase 6
because Phases 1–5 add ~3,000 LOC of new contract infrastructure. That's
the cost of doing this properly. The contract code is reusable for any
future rich plugin (file-watcher, MCP servers with rich UIs, etc.).

## Order of Operations

```
Phase 1 (config) ─────────┐
                          ├──► Phase 4 (settings) ──┐
Phase 2 (commands) ───────┤                         │
                          ├──► Phase 6 (Big Move) ──► Phase 7 (docs)
Phase 3 (tasks) ──────────┤                         │
                          │                         │
Phase 5 (capabilities) ───┴─────────────────────────┘
```

Phases 1, 2, 3, 5 can ship in any order (independent). Phase 4 needs
Phase 1. Phase 6 needs all of 1–5. Phase 7 needs Phase 6.

**Recommended sequence (PR by PR):**

1. **Phase 1** — smallest, foundational, zero behavior change
2. **Phase 5** — also small; gets the build-info RPC out of the way
3. **Phase 2** — establishes the interactive-command pattern that
   Phase 3 builds on
4. **Phase 3** — finishes the streaming pattern; biggest UX win
   (proper non-blocking download + multi-task panel)
5. **Phase 4** — settings work; depends on Phase 1 config being live
6. **Phase 6** — the cleanup; depends on everything else
7. **Phase 7** — docs

## Per-Phase PR Discipline

Each phase ships as **one PR** to `feat/voice-integration` (or a new
`feat/extension-contracts-pX` branch off it). Each PR:

1. Has its own checkpoint doc under `docs/plans/`
2. Includes the phase's acceptance-gate checklist as a checkbox list
   in the PR description
3. Updates `CHANGELOG.md` under "Unreleased → Recent (Dev Branch)"
4. Tests must be green before merge (`cargo test --lib --bins -- --test-threads=1`)
5. Manual smoke pass for any user-visible behavior
6. Human reviewer approval

## Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Phase 6 refactor breaks existing user configs | Migration shim on first launch; deprecation warnings for one release before removal |
| New JSON-RPC streaming patterns introduce backpressure bugs | Phase 2/3 tests deliberately stress with high event rates; bounded mpsc channels everywhere |
| Plugin developers can't figure out the contracts without reading source | Phase 7 documentation; reference impl in local-voice-plugin acts as living example |
| Whisper-specific bugs we missed get baked into the plugin contract | Phases 2–5 are vertical slices; each one is exercised by the local-voice-plugin refactor before declaring done |
| Settings custom-editor protocol is hard to get right (Phase 4) | Allocate extra review time; consider `convergence: pair` for Phase 4 only |
| Loss of synaps-cli LOC in Phase 6 isn't matched 1:1 by gains in plugin | Acceptable — contract code is generic and reusable; plugin code is purpose-specific and that's fine |

## Open Questions

1. **Should Phase 3's expandable subprocess-output overlay be its own
   phase?** Probably yes if it grows beyond 200 LOC. Tracked as Phase 3a.
2. **Do we keep the `voice_toggle_key` config in the global keyspace
   (since it crosses any voice plugin) or move it under
   `local-voice/`?** Global. It's a synaps-side affordance for
   "whatever voice plugin is loaded, this is the toggle."
3. **Should Phase 5 also cover plugin → synaps capability *changes*
   (e.g., a model finished downloading, capabilities just changed)?**
   Yes; lump in with `info.get` as a server-push variant: `info.changed`.
4. **What about settings *outside* the Voice category that voice
   touches** (e.g., `model_health` for the LLM)? Out of scope; those
   stay in core.

## Done Definition (whole plan)

- A new developer can write `synaps-skills/my-voice-plugin/` with:
  - A `plugin.toml` declaring commands, settings, config schema
  - A Rust binary speaking the JSON-RPC protocol
  - **Zero edits to synaps-cli**
- ...and the resulting plugin has full feature parity with
  local-voice-plugin: `/voice models`, `/voice download`,
  `/voice rebuild`, `/settings → Voice → ...`, the tqdm bar, the
  "Current build" annotation, hot-reloading config — all of it.
- `grep -r whisper\|ggml\|huggingface src/` in synaps-cli returns
  zero hits.
- A CI check enforces that grep so we don't drift again.
