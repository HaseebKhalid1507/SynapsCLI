# Plugins & Skills Management UI — Design

**Date:** 2026-04-18
**Status:** Design approved; ready for implementation planning.

## Goal

Add a Claude-Code-parity plugin management experience on top of the existing
skills subsystem. Users can add marketplaces by URL, browse their plugins,
install/uninstall/update per plugin, and enable/disable installed plugins —
all from inside the TUI, with no restart required.

## Scope summary

Two surfaces:

1. **`/plugins`** — new full-screen modal. Heavy operations: add/remove
   marketplaces, browse, install, uninstall, refresh, update.
2. **`/settings → Plugins`** — new category in the existing settings modal.
   Lightweight only: enable/disable toggles for installed plugins.

One supporting command: **`/plugins reload`** — re-reads persisted state and
rebuilds the command registry without restart.

## Architecture

### New modules

- `src/skills/marketplace.rs` — HTTPS metadata fetcher, URL normalization
  (GitHub → raw), wraps the existing `manifest::MarketplaceManifest` parser.
- `src/skills/install.rs` — `git clone` / `git pull` / uninstall filesystem
  operations.
- `src/skills/state.rs` — the persisted `plugins.json` state file (marketplaces,
  installed plugins, trusted hosts, SHAs).
- `src/chatui/plugins/` — new directory for the `/plugins` modal:
  `mod.rs`, `draw.rs`, `input.rs`, `state.rs`. Mirrors the `settings/`
  subdirectory structure.

### Extended modules

- `src/chatui/settings/schema.rs` — add `Category::Plugins` to the
  `Category` enum and the `CATEGORIES` array.
- `src/chatui/settings/draw.rs` — special-case rendering when the Plugins
  category is selected (rows are dynamic, not from `ALL_SETTINGS`).
- `src/chatui/settings/input.rs` — new `InputOutcome::TogglePlugin`.
- `src/chatui/settings/mod.rs` — `RuntimeSnapshot::from_runtime` grows a
  second argument: `&Arc<RwLock<CommandRegistry>>`.
- `src/skills/registry.rs` — interior `RwLock` (or equivalent) so the
  registry can be rebuilt in place. New method `CommandRegistry::rebuild(config)`.
- `src/chatui/commands.rs` — new `CommandAction::OpenPlugins` variant (parallel
  to `OpenSettings`); add `/plugins` to `ALL_COMMANDS`.

## Data model

Single state file: **`~/.synaps-cli/plugins.json`**. Separate from the main
config so adding a marketplace doesn't churn `config.toml`, and so this file
can be reloaded independently.

```json
{
  "marketplaces": [
    {
      "name": "pi-skills",
      "url": "https://github.com/maha-media/pi-skills",
      "description": "…",
      "last_refreshed": "2026-04-18T12:00:00Z",
      "cached_plugins": [
        {
          "name": "web",
          "source": "https://github.com/maha-media/pi-web.git",
          "version": "1.0",
          "description": "…"
        }
      ]
    }
  ],
  "installed": [
    {
      "name": "web",
      "marketplace": "pi-skills",
      "source_url": "https://github.com/maha-media/pi-web.git",
      "installed_commit": "abc123…",
      "latest_commit": "abc123…",
      "installed_at": "2026-04-18T12:01:00Z"
    }
  ],
  "trusted_hosts": [
    "github.com/maha-media",
    "github.com/anthropics"
  ]
}
```

### Key decisions

- `marketplaces[].url` stores the user input verbatim; a URL-normalization step
  derives the raw metadata URL (`github.com/x/y` →
  `raw.githubusercontent.com/x/y/HEAD/.synaps-plugin/marketplace.json`). GitHub
  only for normalization; other hosts require a direct raw URL.
- `cached_plugins` persists the last-fetched marketplace metadata so Browse
  works offline. Refresh updates it.
- `installed[].installed_commit` is the SHA at clone time; `latest_commit`
  is updated by Refresh via `git ls-remote`, enabling a pure-field `↑` badge.
- `trusted_hosts` is **owner-scoped** (`github.com/maha-media`), not
  host-wide. Trusting the ecosystem ≠ trusting an author.
- Enable/disable state stays in `~/.synaps-cli/config.toml` as
  `disabled_plugins` / `disabled_skills` (no schema churn).
- Each installed plugin lives at `~/.synaps-cli/plugins/<name>/` — already
  the loader's discovery path.

## `/plugins` screen

Two-pane drill-down, mirroring `/settings`:

```
┌ Plugins ─────────────────────────────────────────────────┐
│ ▸ Installed (3)         │ web            ✓ enabled       │
│   pi-skills             │ grep-search    ✓ enabled  ↑1   │
│   anthropics-skills     │ file-search    ✗ disabled      │
│   ─────────────────     │                                │
│   + Add marketplace…    │                                │
├─────────────────────────────────────────────────────────┤
│ ↑↓ nav  Tab switch  Enter details  d disable  u update  │
└─────────────────────────────────────────────────────────┘
```

### Right-pane states

1. **Installed selected.** Rows = installed plugins.
   - Row shows: name, `✓ enabled` / `✗ disabled`, `↑N` update badge.
   - Actions: `e`/`d` toggle enabled, `u` update (if stale),
     `Shift+U` uninstall (confirm under row).

2. **Marketplace selected.** Rows = plugins from cached metadata.
   - Row shows: name, description, state badge (`installed` / `—`).
   - Actions: `i` install (TOFU flow if host untrusted).
   - Marketplace-level: `r` refresh, `Shift+R` remove (with confirm).

3. **"+ Add marketplace…" selected.** Right pane shows a one-line text
   editor (same pattern as `ActiveEditor::Text`). Errors render below.

### Detail view

`Enter` on any plugin row swaps the right pane list for a scrollable detail
panel: name, description, version, source URL, marketplace, install state
(`installed at <commit>` / `not installed` / `update available: <cur> → <new>`),
action hint. Esc returns to list; Esc from list closes `/plugins`. Mirrors
`/settings`' editor-vs-modal Esc hierarchy.

## Flows

### Add marketplace

1. User types URL, presses Enter.
2. Validate `https://`. Reject everything else.
3. Normalize GitHub URLs to raw metadata URL. Other hosts used as-is.
4. HTTP GET via `reqwest`, 10s timeout.
5. Parse `MarketplaceManifest`. Validate every `source` is `https://`.
   Reject relative paths.
6. Write `plugins.json`. New marketplace appears in left pane.

### Install plugin

1. User presses `i` on a plugin row.
2. Already installed → inline no-op error.
3. Derive owner-scoped host. If not in `trusted_hosts`, show TOFU prompt:
   ```
   ┌ Trust new host ─────────────────┐
   │ Install plugin "web"?           │
   │                                 │
   │ Source: github.com/maha-media   │
   │ First time installing from here.│
   │                                 │
   │ [Y] Trust and install           │
   │ [N] Cancel                      │
   └─────────────────────────────────┘
   ```
   `Y` adds to `trusted_hosts` and proceeds; `N` aborts.
4. `git clone --depth=1 <source_url> ~/.synaps-cli/plugins/<name>/`.
5. Capture `git rev-parse HEAD` → `installed_commit`.
6. Write `plugins.json`, trigger registry rebuild.

### Update plugin

When row shows `↑`: `git -C <install_path> pull --ff-only`, re-capture SHA,
trigger rebuild.

### Uninstall plugin

`Shift+U` → confirm under row → `rm -rf <install_path>`, remove from
`installed`, trigger rebuild.

### Refresh marketplace

`r` on a marketplace row:
1. Re-fetch metadata, update `cached_plugins` + `last_refreshed`.
2. For each installed plugin from this marketplace, `git ls-remote
   <source_url> HEAD` and store result in `installed[].latest_commit`.
3. Write `plugins.json`. Badges reflect state on next render, no network
   required to re-render.

### Remove marketplace

`Shift+R` → confirm → remove from `marketplaces`. Installed plugins from
it stay on disk and keep working; their `marketplace` field becomes
dangling, rendered as `(unknown)` in the UI.

### Registry hot-reload

Currently `Arc<CommandRegistry>` is constructed once at startup and cloned
into `LoadSkillTool`. For hot-reload:
- Add an interior `RwLock` inside `CommandRegistry` over the inner
  plugin/skill lists.
- New method `CommandRegistry::rebuild(config: &SynapsConfig)` re-runs
  `loader::load_all` + `filter_disabled`, takes the write lock, swaps
  contents.
- Both chatui and `LoadSkillTool` keep their existing `Arc` handles —
  no Arc swap required.

## `/settings → Plugins` category

### Rendering

Branch in `render_settings` on `state.category_idx == Plugins`. Iterate
live plugin list from `CommandRegistry::plugins()` instead of
`ALL_SETTINGS`.

Row format:
```
  pi-skills               ✓ enabled
  web-tools               ✗ disabled
  anthropics-skills       ✓ enabled  (3 skills)
```

### Input

When Plugins category is selected and right pane is focused, `Enter` or
`Space` on a row flips membership in `config.disabled_plugins`. Returns
`InputOutcome::TogglePlugin { name, enabled }`. The outer chatui event
loop handles: config mutation, persistence, and
`CommandRegistry::rebuild()`.

### Non-UI per-skill toggles

Per-skill disable is **not** in this UI. Users who want finer granularity
edit `disabled_skills` in `config.toml` directly.

### Empty state

`No plugins installed. Open /plugins to add a marketplace.`

## Error handling

Errors render inline under the triggering row (same pattern as
`row_error` in `settings/draw.rs`). No popups.

| Operation | Failure | Behavior |
| --- | --- | --- |
| Add marketplace | not `https://` | `"only https:// URLs are supported"` |
| | HTTP fetch error | `"failed to fetch marketplace.json: <status>"` |
| | JSON parse error | `"invalid marketplace.json: <serde error>"` |
| | `source` relative path | `"plugin '<n>' uses unsupported relative source path"` |
| | timeout (10s) | `"timed out fetching marketplace.json"` |
| Install | `git clone` fails | surface `git` stderr |
| | target dir exists | `"already present on disk; uninstall first"` |
| | git not on PATH | `"git not found on PATH"` |
| Update | `git pull --ff-only` refuses | `"local changes; uninstall and reinstall"` |
| Refresh | network error | cache untouched; `"refresh failed: <err>"` |
| Reload | `plugins.json` missing | treat as empty, no error |
| | `plugins.json` malformed | `"corrupt plugins.json; fix manually"` — don't rewrite |

## Offline behavior

- `/plugins` opens and browses against `cached_plugins` without network.
- Refresh / Install / Update fail cleanly with clear messages.
- `↑N` badges computed from persisted `latest_commit` fields.

## Testing

### Unit tests (in-module `#[cfg(test)]`)

- URL normalization: GitHub rewrite, `http://` rejection, raw URL passthrough.
- `marketplace.json` validation: reject relative sources, reject `http://`
  sources, accept minimal valid docs, surface parse errors clearly.
- TOFU host derivation from a source URL (owner-scoped).
- Update-available comparison (installed vs. latest SHA).
- `plugins.json` round-trip (read/write/reload).

### Integration tests

- `tests/plugins_manage.rs` (new): local HTTP stub + local git repo on
  disk; exercise add-marketplace → install → uninstall end-to-end. No
  real network.
- Extend `tests/skills_plugin.rs`: registry hot-reload — build registry,
  add a SKILL.md on disk, call `rebuild()`, assert new command resolves.

### UI logic tests

State-machine tests on `PluginsState` — cursor movement, detail-view
entry/exit, TOFU-prompt gating. No ratatui rendering assertions; follow
the pattern used for `input.rs` elsewhere.

## Explicit non-goals (MVP)

1. Per-skill disable from the `/settings` UI.
2. Revoking trust from the UI.
3. Private-repo auth (SSH keys, PATs).
4. Compat shim for Claude Code's `.claude-plugin` directory name —
   marketplaces must use `.synaps-plugin`.
5. Background polling for updates; Refresh is always manual.
6. Jumping from `/settings → Plugins` into `/plugins` with focus on a
   specific plugin.

## Follow-ups (not blocking)

- Detail-view "open repo in browser" action.
- Per-skill disable UI.
- `.claude-plugin` compat shim if the ecosystem demands it.
- Authentication (SSH / PAT) for private marketplaces.
