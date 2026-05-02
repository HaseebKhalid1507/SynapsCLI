# Whisper Model Manager & Backend Acceleration

**Status:** Draft
**Date:** 2026-05-02
**Author:** voice-integration squad
**Convergence mode:** `none` — incremental UX feature, low blast radius, human-reviewed at every checkpoint.

## Goal

Make the local voice plugin a self-service experience:

1. **Model manager.** Users can browse the canonical whisper.cpp model
   catalog, see what is installed, and download more (e.g. `ggml-base.bin`,
   `ggml-medium.bin`, `ggml-large-v3-turbo.bin`) directly from inside the
   `/settings → Voice` panel — no manual `curl` invocations.
2. **Model switching.** The existing `voice_stt_model` picker becomes the
   primary way to swap between installed models; the picker auto-refreshes
   after a download finishes, and current download progress is visible.
3. **Backend acceleration.** Users pick a whisper compute backend
   (CPU / CUDA / Metal / Vulkan / OpenBLAS) from settings. The plugin
   reports the backend it was *built* with; switching backends triggers a
   one-shot rebuild via the existing `scripts/setup.sh --features` path.
   The current build's backend is surfaced clearly so users know what they
   are running.

## Non-Goals

- **Runtime backend switching** without a rebuild. whisper-rs feature flags
  are compile-time. Switching backends requires re-running
  `scripts/setup.sh --features local-stt,<backend>`. We accept that.
- **Hardware probing as gating.** We *suggest* a backend based on detected
  hardware (CUDA, Metal, etc.) but never block the user from picking
  another. Builds may fail; we surface the error.
- **Quantized model uploads or training.** Catalog is read-only and
  matches the upstream whisper.cpp HF release artifacts.
- **Shipping models with the binary.** Users always download.
- **Network proxy / mirror configuration** (out of scope; we use the
  default system network like the rest of synaps).

## Convergence Decision

`convergence: none`. Rationale:

- Feature is UI sugar over an already-working voice subsystem.
- No security-critical code paths (downloads are validated by SHA, no
  arbitrary code execution beyond the rebuild step which the user explicitly
  triggers).
- Human will review every PR before merge.

If during implementation we touch trust/extension boundaries (e.g. invoking
`scripts/setup.sh` with user-supplied flags), we re-evaluate and stop for a
plan amendment.

## Architecture

### Two repos, two scopes

| Repo | Scope |
|---|---|
| `synaps-skills` (`local-voice-plugin`) | Sidecar reports `BuildInfo` (compiled backend + features). `setup.sh` accepts `--features` (already does). New helper `--print-build-info` flag on the binary. |
| `synaps-cli` (`voice-integration`) | Model catalog + downloader + backend rebuild orchestration. New settings UI, new `/voice` subcommands. |

The plugin owns *what it can run with*. Synaps owns *user choice and
download management*. Downloads land in `~/.synaps-cli/models/whisper/`,
the existing convention.

### Model catalog

Embedded constant in synaps-cli — `src/voice/models.rs`:

```rust
pub struct CatalogEntry {
    pub id: &'static str,        // "base", "base.en", "large-v3-turbo"
    pub filename: &'static str,  // "ggml-base.bin"
    pub size_mb: u32,            // 142
    pub multilingual: bool,      // false for *.en
    pub sha256: &'static str,    // upstream-published checksum
}

pub const CATALOG: &[CatalogEntry] = &[
    // tiny / tiny.en / base / base.en / small / small.en /
    // medium / medium.en / large-v3 / large-v3-turbo
];
```

Source of truth for SHA256 + sizes: <https://huggingface.co/ggerganov/whisper.cpp>.
We hard-code values and bump them when upstream rotates artifacts.
Download URL pattern: `https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{filename}`.

### Downloader

- Async streaming download via the existing `reqwest` client (already a
  dep in synaps-cli for OpenAI/Anthropic).
- Progress events: `bytes_downloaded / total_bytes` published to a
  `tokio::sync::watch` channel; UI subscribes and renders a progress
  line in the settings modal.
- Atomic install: write to `models/whisper/.{filename}.partial`, verify
  SHA256, rename into place. On verification failure, delete partial and
  surface error.
- Cancellable: dropping the receiver cancels.

### Backend selection

Backend choice is a string config: `voice_stt_backend ∈ { auto, cpu, cuda, metal, vulkan, openblas }`.

- **`auto`** — pick based on host: macOS → metal; Linux/NVIDIA detected → cuda; otherwise cpu.
- Detection helpers (best-effort, no failures bubble):
  - `cuda`: `nvcc --version` exit 0 OR `/usr/local/cuda` exists OR `nvidia-smi` exit 0.
  - `metal`: `cfg!(target_os = "macos")`.
  - `vulkan`: `vulkaninfo` exit 0.
  - `openblas`: `pkg-config --exists openblas`.

Selecting a backend in settings does **not** apply immediately — it stages
a "rebuild required" notice and a `[Rebuild now]` action that invokes
`scripts/setup.sh --features local-stt,<backend>` (`local-stt` alone for
CPU).

The plugin binary, when run with `--print-build-info`, prints a single line
of JSON describing the features it was compiled with. Synaps stores the
last-seen build info and warns when configured backend ≠ compiled backend.

### Sidecar build-info protocol

New CLI flag `--print-build-info` on `synaps-voice-plugin`:

```json
{"backend": "cpu", "features": ["local-stt"], "version": "0.1.0"}
```

Backend resolution order in the sidecar (compile-time):
`cuda > metal > vulkan > openblas > cpu`. (At most one accelerator can be
linked at a time; if the user builds with two we pick the highest in the
list and warn.)

This is *separate* from runtime stdio protocol — it's a one-shot CLI mode,
exits immediately.

## Dependency Graph

```
[Catalog data]                          [Sidecar BuildInfo flag]
   └── [Downloader core]                   └── [Synaps build-info reader]
        ├── [/voice models CLI]                  └── [Settings: backend selector]
        ├── [/voice download CLI]                      └── [Settings: rebuild action]
        └── [Settings: model browser UI]
              └── [Settings: download progress UI]
```

Two parallel tracks (model manager / backend manager) converge in the
settings panel work.

## Tasks

### Track A — Sidecar Build-Info (synaps-skills)

#### Task A1: `--print-build-info` flag

**Description:** Add a CLI mode to the sidecar that prints compiled
features as JSON and exits 0.

**Acceptance criteria:**
- [ ] `synaps-voice-plugin --print-build-info` emits one JSON line and
      exits.
- [ ] JSON shape: `{"backend": "<name>", "features": [...], "version": "..."}`.
- [ ] Backend resolved at compile time via `cfg!(feature = "...")` checks.
- [ ] Unknown/missing accelerator → `"backend": "cpu"`.

**Verification:**
- [ ] `cargo build --release && ./target/release/synaps-voice-plugin --print-build-info`
- [ ] `cargo build --release --features local-stt && ... --print-build-info` reports `cpu`.
- [ ] `cargo test --features local-stt` (unit test on backend resolution helper).

**Dependencies:** None
**Files likely touched:** `local-voice-plugin/src/main.rs`, new
`local-voice-plugin/src/build_info.rs`, `tests/build_info.rs`.
**Scope:** S

---

#### Task A2: README + manifest doc for `--print-build-info`

**Description:** Document the flag and the supported feature combos in
the plugin README and bump the protocol_version note (no protocol_version
change — this is out-of-band CLI).

**Acceptance criteria:**
- [ ] `local-voice-plugin/README.md` lists supported backends and how to
      build each.
- [ ] `--print-build-info` documented with example output.
- [ ] `scripts/setup.sh` `--help` output mentions accelerator features
      already supported (already true — verify only).

**Verification:**
- [ ] Manual: `./scripts/setup.sh --help`.
- [ ] Markdown lint passes if applicable.

**Dependencies:** A1
**Files likely touched:** `README.md`.
**Scope:** XS

---

### Track B — Model Catalog & Downloader (synaps-cli)

#### Task B1: Embed catalog + tests

**Description:** Hard-code the whisper.cpp model catalog with sizes,
multilingual flag, and SHA256 in `src/voice/models.rs`.

**Acceptance criteria:**
- [ ] At least 8 catalog entries: `tiny`, `tiny.en`, `base`, `base.en`,
      `small`, `small.en`, `medium`, `medium.en`, `large-v3`,
      `large-v3-turbo`.
- [ ] Helpers: `find_by_filename()`, `find_by_id()`, `iter_multilingual()`.
- [ ] SHA256 values quoted from upstream HF (we copy from the model card;
      a comment links the source).

**Verification:**
- [ ] `cargo test voice::models`.
- [ ] Snapshot test that catalog contains expected ids.

**Dependencies:** None
**Files likely touched:** `src/voice/models.rs`, `src/voice/mod.rs`.
**Scope:** S

---

#### Task B2: Streaming downloader with SHA verification

**Description:** Implement an async function
`download_model(entry, models_dir, progress_tx) -> Result<PathBuf>` that
streams from HF, verifies SHA256, and atomically renames into place.

**Acceptance criteria:**
- [ ] Writes to `<models_dir>/.<filename>.partial` while downloading.
- [ ] Emits progress on `tokio::sync::watch::Sender<DownloadProgress>`
      where `DownloadProgress = { bytes: u64, total: Option<u64>, done: bool }`.
- [ ] On EOF, computes SHA256; on mismatch, deletes partial and returns
      `Err(DownloadError::ChecksumMismatch)`.
- [ ] On success, renames `.partial` → final filename.
- [ ] Cancellation: dropping the future or the watch sender ends cleanly
      and removes the partial.
- [ ] Re-download of an already-installed file is a no-op (early-return)
      unless `force = true`.

**Verification:**
- [ ] `cargo test voice::download` with a tempdir + a local hyper test
      server serving a fixture file.
- [ ] Checksum-mismatch test (serve wrong bytes, expect error and partial
      cleanup).
- [ ] Cancellation test (drop future mid-stream, expect partial removed).

**Dependencies:** B1
**Files likely touched:** `src/voice/download.rs`, `Cargo.toml` (no new
deps; reuse `reqwest`, `sha2`).
**Scope:** M

---

#### Task B3: `/voice models` and `/voice download <id>` CLI subcommands

**Description:** Extend the existing `/voice` slash command with
`models` (list catalog + installed status) and `download <id>` (kick off
a download with progress in the chat log).

**Acceptance criteria:**
- [ ] `/voice models` prints a table: id, filename, size, multilingual,
      installed (✓/✗).
- [ ] `/voice download base` starts a download, streams progress lines
      to the chat, and prints a summary on completion.
- [ ] `/voice download <unknown>` errors with the list of valid ids.
- [ ] The model picker in `/settings` reflects newly-installed files.
- [ ] BUILTIN_COMMANDS help text updated.

**Verification:**
- [ ] `cargo test commands::voice` (unit tests on parser + dispatch).
- [ ] Manual: launch synaps, run `/voice models`, run `/voice download tiny`,
      observe progress, confirm file appears in `~/.synaps-cli/models/whisper/`.

**Dependencies:** B1, B2
**Files likely touched:** `src/chatui/commands.rs`,
`src/chatui/voice.rs`, `src/chatui/run.rs` (action dispatch),
`src/skills/registry.rs` (BUILTIN_COMMANDS).
**Scope:** M

---

### ✅ Checkpoint 1: After A1 + B1–B3

- [ ] `cargo test` passes (lib + integration).
- [ ] Manual: download tiny.en via `/voice download`, confirm file
      lands and picker shows it.
- [ ] Sidecar `--print-build-info` works and synaps can parse it.
- [ ] Human review before starting Track C.

---

### Track C — Settings UI: Model Browser

#### Task C1: Replace WhisperModelPicker with a model browser action

**Description:** The picker currently cycles installed `.bin` files.
Promote it to a richer browser: pressing Enter opens a list view with
catalog entries; installed entries are selectable; uninstalled entries
show `[download]`.

**Acceptance criteria:**
- [ ] New `EditorKind::ModelBrowser` variant in
      `chatui::settings::schema`.
- [ ] List rendering shows: `[●] base.en   142 MB   en   installed`
      and `[ ] base       142 MB   multi  [download]`.
- [ ] Up/Down navigates; Enter selects (sets `voice_stt_model_path`)
      OR triggers download for uninstalled rows.
- [ ] Esc closes the browser without changes.
- [ ] Existing `voice_stt_model_path` config key continues to be the
      persisted output (no migration needed).

**Verification:**
- [ ] `cargo test chatui::settings::input::model_browser`.
- [ ] Manual: open `/settings → Voice → STT model`, navigate, select an
      installed model, confirm config update.

**Dependencies:** B3
**Files likely touched:** `src/chatui/settings/schema.rs`,
`src/chatui/settings/input.rs`, `src/chatui/settings/draw.rs`,
`src/chatui/settings/mod.rs` (extend `whisper_model_options` to merge
catalog + installed).
**Scope:** M

---

#### Task C2: Inline download progress in settings modal

**Description:** When the user triggers download from the browser,
render a live progress bar inside the settings modal until completion.
Picker refreshes automatically when the download finishes.

**Acceptance criteria:**
- [ ] `App` holds an `Option<DownloadProgress>` for the in-flight model
      download; rendered as `Downloading ggml-base.bin: 42% (60/142 MB)`.
- [ ] On completion, the row in the browser flips to `installed`.
- [ ] On error, an error line appears in the modal and is dismissible.
- [ ] Only one concurrent download supported (queue or block additional
      requests with a "download in progress" message).

**Verification:**
- [ ] `cargo test chatui::voice::download_state` (state machine).
- [ ] Manual: trigger a download from settings, confirm bar updates and
      picker refreshes without leaving the modal.

**Dependencies:** C1
**Files likely touched:** `src/chatui/app.rs`,
`src/chatui/settings/draw.rs`, `src/chatui/settings/input.rs`,
`src/chatui/voice.rs`.
**Scope:** M

---

### Track D — Settings UI: Backend Selection

#### Task D1: Read & cache sidecar build-info

**Description:** On voice startup, run the sidecar with
`--print-build-info` once, parse the JSON, and cache it on
`VoiceUiState`. Add `voice_stt_backend` config key (default `auto`).

**Acceptance criteria:**
- [ ] `VoiceUiState` gains `compiled_backend: Option<String>`.
- [ ] Discovery cache is populated before the first `press`.
- [ ] If the binary is missing or fails, `compiled_backend = None` and
      a warning is logged, but voice still works (we don't gate startup).
- [ ] `voice_stt_backend` config defaults to `auto` when unset.

**Verification:**
- [ ] `cargo test voice::discovery::build_info`.
- [ ] Manual: launch synaps, run `/voice status`, confirm new line
      `compiled backend: cpu`.

**Dependencies:** A1
**Files likely touched:** `src/voice/discovery.rs`,
`src/voice/manager.rs`, `src/chatui/voice.rs`.
**Scope:** S

---

#### Task D2: Backend cycler in settings

**Description:** Add a `voice_stt_backend` setting with a Cycler editor
listing `auto / cpu / cuda / metal / vulkan / openblas`. Below it, a
status line: `Current build: <compiled_backend>`. If selected ≠
compiled, show `⚠ rebuild required` and a `[Rebuild now]` button-row
(Enter triggers rebuild).

**Acceptance criteria:**
- [ ] Setting persists to config but does not change runtime behavior
      until rebuild.
- [ ] `auto` resolves at *display time* via host detection; the rebuild
      action expands `auto` to a concrete backend.
- [ ] If host detection fails, `auto` resolves to `cpu` and a tooltip
      explains why.
- [ ] Rebuild action is a stub in this task (next task implements it).

**Verification:**
- [ ] `cargo test chatui::settings::voice_backend`.
- [ ] Manual: open settings, switch backend, confirm warning appears.

**Dependencies:** D1
**Files likely touched:** `src/chatui/settings/defs.rs`,
`src/chatui/settings/schema.rs`, `src/chatui/settings/draw.rs`,
`src/chatui/settings/input.rs`, `src/voice/discovery.rs` (host probe).
**Scope:** M

---

#### Task D3: Rebuild action invokes `scripts/setup.sh`

**Description:** When the user activates `[Rebuild now]`, synaps spawns
`scripts/setup.sh --features local-stt[,<backend>]` from the discovered
plugin directory and streams stdout/stderr into a modal panel. On
success, re-runs `--print-build-info` and updates the cached backend.

**Acceptance criteria:**
- [ ] Build runs in a child process, output streamed line-by-line into
      a scrollable modal.
- [ ] Cancellable (Esc kills the child process).
- [ ] On exit code 0: re-discover sidecar build info, show
      `✓ rebuild succeeded; now running on <backend>`.
- [ ] On nonzero exit: show error and last 20 lines of output.
- [ ] During rebuild, voice toggle is disabled with a status hint.

**Verification:**
- [ ] `cargo test voice::rebuild::spawn` (mocked subprocess).
- [ ] Manual: rebuild with `cpu`, confirm sidecar restarts and reports
      `cpu`. (CUDA/Metal manual tests done by users with matching
      hardware — out of CI scope.)

**Dependencies:** D2, A1
**Files likely touched:** `src/voice/rebuild.rs`,
`src/chatui/voice.rs`, `src/chatui/settings/input.rs`,
`src/chatui/settings/draw.rs`, `src/chatui/app.rs`.
**Scope:** M

---

### ✅ Checkpoint 2: After C1–C2 + D1–D3

- [ ] `cargo test` passes.
- [ ] Manual end-to-end: install `base.en` via settings; switch to it;
      verify `/voice toggle` uses the new model.
- [ ] Manual: switch backend to `cpu` (or current), trigger rebuild,
      confirm sidecar reports the rebuilt backend afterward.
- [ ] Human review before Track E.

---

### Track E — Polish & Docs

#### Task E1: `/voice` help text + CHANGELOG + plan checkpoints

**Description:** Update help, CHANGELOG, and roll forward V9 docs to
cover model manager + backends.

**Acceptance criteria:**
- [ ] `/voice` with no args shows help including `models`, `download`,
      `backend`.
- [ ] `CHANGELOG.md` entry under voice integration.
- [ ] This plan moved to `Status: Done` with closing notes.

**Verification:**
- [ ] Manual: `/voice` help.
- [ ] `cargo test --doc` passes.

**Dependencies:** all prior tasks
**Files likely touched:** `CHANGELOG.md`, `src/chatui/commands.rs`,
this plan, `docs/plans/2026-05-02-voice-integration.md`.
**Scope:** S

---

#### Task E2: Plugin manifest catalog hint (optional)

**Description:** Extend the plugin manifest `provides.voice_sidecar`
with a `supported_backends: ["cpu", "cuda", "metal", "vulkan", "openblas"]`
string array. Synaps reads it to scope the cycler to what the *plugin*
claims to support (future-proofing for other voice plugins). Default
when missing: full list.

**Acceptance criteria:**
- [ ] Optional `supported_backends` field parsed in `PluginManifest`.
- [ ] Backend cycler intersects manifest list with synaps's hardcoded
      list.
- [ ] Backwards compatible with manifests that don't declare it.

**Verification:**
- [ ] `cargo test skills::manifest::voice_supported_backends`.

**Dependencies:** D2
**Files likely touched:** `src/skills/manifest.rs`,
`local-voice-plugin/.synaps-plugin/plugin.json`,
`src/chatui/settings/defs.rs`.
**Scope:** S — punt to follow-up if scope tightens.

---

## Risk & Open Questions

| Risk | Mitigation |
|---|---|
| HF rotates model artifacts → SHAs become stale | Comment in `models.rs` linking to upstream model card; CI test that `tiny.en` (smallest) downloads + verifies fresh in nightly job. |
| Rebuild during active voice session corrupts state | Force `release()` and shutdown sidecar before rebuild; block voice until rebuild completes or is cancelled. |
| User without GPU picks `cuda` → build fails | Surface stderr; offer "fall back to CPU" button on failure; never auto-rewrite their config. |
| Large model downloads (~3GB for large-v3) on slow links | Already handled — progress + cancellable; log shows ETA. |
| `setup.sh` not present on every plugin install (e.g. user copied prebuilt binary) | Detect missing script before showing rebuild button; otherwise hide it with an explanatory tooltip. |
| Concurrent download race | Mutex on `~/.synaps-cli/models/whisper/.<filename>.partial`; second downloader sees partial and refuses. |

## Estimated Effort

- Track A: ~half day
- Track B: 1.5 days
- Track C: 1.5 days
- Track D: 2 days
- Track E: half day
- **Total:** 5–6 working days end-to-end with checkpoints.

## Worktree

Stay on existing `feat/voice-integration` worktree at
`/home/jr/Projects/Maha-Media/.worktrees/SynapsCLI-voice-integration`.
Plugin work goes on existing `feat/local-voice-plugin` worktree at
`/home/jr/Projects/Maha-Media/.worktrees/synaps-skills-local-voice-plugin`.
No new branches needed — both already exist and are tracking remote.

## Approval

**Approver:** human
**Approved at:** _pending_

Once approved, start with Task A1 (sidecar build-info) and Task B1
(catalog) in parallel — they're independent — then converge at the
Checkpoint 1.
