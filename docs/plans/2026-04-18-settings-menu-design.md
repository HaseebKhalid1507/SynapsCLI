# Settings Menu — Design

**Date:** 2026-04-18
**Status:** Design validated, ready for implementation

## Goal

Add an interactive settings menu to `chatui`, opened via `/settings`, that both mutates runtime state and persists changes to `~/.synaps-cli/config`. Inspired by Claude Code's `/config` and Pi coding agent's settings UI.

## Scope

Expose the 8 config-file settings plus `theme`:

- **Model:** `model`, `thinking`
- **Agent:** `skills`, `api_retries`, `subagent_timeout`
- **Tool Limits:** `max_tool_output`, `bash_timeout`, `bash_max_timeout`
- **Appearance:** `theme`

Out of scope: system prompt editor (use `/system`), TOML migration, keybindings, MCP server management, profile switcher, per-session ephemeral settings.

## UX

**Entry/exit.** `/settings` opens a full-screen modal overlay (via `ratatui::widgets::Clear`). Esc closes, returning to chat with state preserved. Blocks input while open — same pattern as the gamba takeover.

**Two-pane layout.**

```
┌─ Settings ──────────────────────────────────────────┐
│ Categories       │ Model                            │
│ ▸ Model          │                                  │
│   Agent          │   Model:     claude-opus-4-7 ▾   │
│   Tool Limits    │   Thinking:  ◀ medium ▶          │
│   Appearance     │                                  │
│                  │                                  │
│                  │   ↑↓ navigate  Enter edit  Esc   │
└─────────────────────────────────────────────────────┘
```

- Left pane: category list.
- Right pane: settings in the selected category with current value inline.
- Tab switches focused pane. Active pane gets `THEME.border_active`.
- Footer line shows context-sensitive keybinds.

## Per-Setting Editors

Three editor types, assigned per setting:

1. **Cycler (inline ◀ ▶).** `thinking` — enum `low / medium / high / xhigh`. Left/right arrows on the focused row change value immediately. No "edit mode."
2. **Dropdown picker.** Enter opens popup list; ↑↓ select, Enter confirm, Esc cancel.
   - `model` — `KNOWN_MODELS` + "Custom…" (falls through to text input).
   - `theme` — built-in themes + custom themes from `~/.synaps-cli/themes/`.
3. **Text input.** Enter activates inline editor; type, Enter save, Esc cancel.
   - `skills` — comma-separated string.
   - `api_retries`, `subagent_timeout`, `max_tool_output`, `bash_timeout`, `bash_max_timeout` — numeric, rejected if non-digit.

Conventions: current value in `THEME.claude_text`; `▾` or `◀ ▶` affordance hints. Invalid numeric input shows inline error line until fixed.

## Persistence

**Save-on-change.** Every accepted edit (a) mutates `Runtime`/theme state in memory, (b) writes to `~/.synaps-cli/config`. No save button.

**Writer function** in `src/core/config.rs`:

```rust
pub fn write_config_value(key: &str, value: &str) -> io::Result<()>
```

Line-oriented algorithm — preserves comments and unknown keys:

1. Read `resolve_write_path("config")` into lines.
2. Find first non-comment line matching `^\s*{key}\s*=`.
3. Replace that line, or append `{key} = {value}` if absent.
4. Write to `config.tmp`, `fs::rename` to `config`.

Round-tripping through `SynapsConfig` would nuke comments and unknown keys; Claude Code's `/config` preserves them for this reason.

**Theme persistence.** New. Theme currently lives in memory only. Add `theme = <name>` as a recognized key in `load_config()`, default `"default"`. Chatui reads at startup and applies before first frame.

**Model list consolidation.** New module `src/core/models.rs`:

```rust
pub const KNOWN_MODELS: &[(&str, &str)] = &[
    ("claude-opus-4-7",           "Opus 4.7 — most capable"),
    ("claude-sonnet-4-6",         "Sonnet 4.6 — balanced"),
    ("claude-haiku-4-5-20251001", "Haiku 4.5 — fast"),
];
pub fn default_model() -> &'static str { KNOWN_MODELS[0].0 }
```

Replace scattered hardcoded strings in `runtime/mod.rs:63`, `tools/subagent.rs:35`, `tools/subagent.rs:70`, `core/watcher_types.rs:196` with `default_model()`. Small mechanical refactor.

## Module Layout

New module `src/chatui/settings/` (sibling to `theme/`):

```
src/chatui/settings/
├── mod.rs         — public entry: open(), types, SettingsState
├── schema.rs      — static list of SettingDef entries
├── draw.rs        — ratatui rendering of the modal
└── input.rs       — key event dispatch for the modal
```

**Core types (`mod.rs`):**

```rust
pub(crate) enum EditorKind {
    Cycler(&'static [&'static str]),
    ModelPicker,
    ThemePicker,
    Text { numeric: bool },
}

pub(crate) struct SettingDef {
    pub key: &'static str,
    pub label: &'static str,
    pub category: Category,
    pub editor: EditorKind,
    pub help: &'static str,
}

pub(crate) enum Category { Model, Agent, ToolLimits, Appearance }

pub(crate) struct SettingsState {
    category_idx: usize,
    setting_idx: usize,
    focus: Focus,                    // Left | Right
    edit_mode: Option<ActiveEditor>,
}
```

## Integration Points

1. **`commands.rs`** — add `"settings"` to `ALL_COMMANDS`; new match arm returns `CommandAction::OpenSettings`.
2. **`app.rs`** — add `settings: Option<SettingsState>` field on `App`. When `Some`, draw/input are routed to the settings module.
3. **`draw.rs`** — after normal draw, if `app.settings.is_some()`, overlay via `Clear` + `settings::draw::render`.
4. **`input.rs`** — top of event handler: if settings is open, dispatch to `settings::input::handle` and return early.
5. **`main.rs`** — on startup, apply persisted values (including new `theme` key) to runtime/theme before first frame.

New `CommandAction::OpenSettings` variant parallels existing `LaunchGamba` pattern.

## Error Handling

- **Config write failure** (disk full, permission denied): inline error below the setting row (red, 1 line, auto-clears on next edit). Runtime value still applied — the menu never rolls back state. Matches `/system save` behavior.
- **Invalid number input:** rejected at the editor layer, never reaches the writer. Inline error until fixed or Esc.
- **Missing config file on write:** `resolve_write_path` creates parent dirs — no special handling.
- **Theme load failure** (bad name in config): silent fall back to `"default"`, log warning via `tracing`. Same as unknown config keys.
- **Unknown config keys preserved** — intentional, enabled by line-oriented writer.

## Testing

**Unit tests:**

- `schema.rs`: every `SettingDef.key` must be recognized by `load_config()` (parity check, prevents settings that silently do nothing).
- `config::write_config_value`:
  - replaces existing key
  - appends new key
  - preserves comments
  - preserves unknown keys
  - happy-path atomic write

**Manual test checklist** (in implementation plan, not automated):

- Open menu, change each setting type, verify file on disk, verify runtime effect, close and reopen to verify persistence.

**No TUI snapshot tests** — chatui has none; not introducing here.

## Dependencies

No new crates required. Uses existing `ratatui`, `crossterm`, `serde_json`, `tracing`.

## Open Questions

None — all resolved during brainstorm.
