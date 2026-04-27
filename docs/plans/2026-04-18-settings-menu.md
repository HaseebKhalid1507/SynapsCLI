# Settings Menu Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add an interactive `/settings` menu to the `chatui` TUI that exposes the config-file-backed settings (model, thinking, skills, tool limits, theme), persists changes to `~/.synaps-cli/config`, and applies them to the running `Runtime` where possible.

**Architecture:** New `src/chatui/settings/` module renders a full-screen modal overlay (ratatui `Clear` + two-pane layout). A new `write_config_value()` helper in `src/core/config.rs` writes a single key to the config file line-by-line (preserves comments and unknown keys). Scattered hardcoded model strings consolidate into a new `src/core/models.rs` with a `KNOWN_MODELS` list powering the dropdown picker. Theme changes persist but require restart (matches existing `/theme` behavior — LazyLock).

**Tech Stack:** Rust, ratatui 0.29, crossterm 0.28. No new dependencies.

**Reference design doc:** `docs/plans/2026-04-18-settings-menu-design.md` in this worktree.

---

## Phase 1 — Foundation (pure logic, TDD)

### Task 1: Create `src/core/models.rs` with `KNOWN_MODELS` list

**Files:**
- Create: `src/core/models.rs`
- Modify: `src/core/mod.rs`

**Step 1: Write the failing test**

Add at the bottom of `src/core/models.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_model_is_first_entry() {
        assert_eq!(default_model(), KNOWN_MODELS[0].0);
    }

    #[test]
    fn known_models_has_expected_ids() {
        let ids: Vec<&str> = KNOWN_MODELS.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&"claude-opus-4-7"));
        assert!(ids.contains(&"claude-sonnet-4-6"));
        assert!(ids.contains(&"claude-haiku-4-5-20251001"));
    }

    #[test]
    fn descriptions_are_non_empty() {
        for (_, desc) in KNOWN_MODELS {
            assert!(!desc.is_empty(), "empty description");
        }
    }
}
```

**Step 2: Run to confirm it fails to compile**

`cargo test -p synaps-cli core::models 2>&1 | tail -5` → expect "file not found" or compile error (module not declared).

**Step 3: Implement the module**

Write to `src/core/models.rs`:

```rust
//! Curated list of Claude models known to work with this CLI.
//! Centralized so the settings dropdown, defaults, and subagent hints agree.

pub const KNOWN_MODELS: &[(&str, &str)] = &[
    ("claude-opus-4-7",           "Opus 4.7 — most capable"),
    ("claude-sonnet-4-6",         "Sonnet 4.6 — balanced"),
    ("claude-haiku-4-5-20251001", "Haiku 4.5 — fast"),
];

pub fn default_model() -> &'static str {
    KNOWN_MODELS[0].0
}

#[cfg(test)]
mod tests {
    // tests from Step 1
}
```

Add to `src/core/mod.rs`:

```rust
pub mod models;
```

Re-export in the crate root if other modules use it without full path. Check `src/lib.rs` for existing re-export pattern first — if `pub use core::config` exists, add `pub use core::models`.

**Step 4: Run tests**

`cargo test -p synaps-cli core::models` → 3 passed.

**Step 5: Commit**

```bash
git add src/core/models.rs src/core/mod.rs src/lib.rs
git commit -m "feat(core): add KNOWN_MODELS registry and default_model()"
```

---

### Task 2: Replace hardcoded model strings with `default_model()`

**Files:**
- Modify: `src/runtime/mod.rs:63` — `model: "claude-opus-4-6".to_string()` → `model: crate::models::default_model().to_string()`
- Modify: `src/tools/subagent.rs:35` — update default in JSON schema description
- Modify: `src/tools/subagent.rs:70` — `model_override.unwrap_or_else(|| "claude-sonnet-4-20250514".to_string())` → use `default_model()`
- Modify: `src/core/watcher_types.rs:196` — `fn default_model()` (existing) to call `crate::models::default_model().to_string()`

**Step 1: Make the edits**

Open each file, replace the hardcoded string. Leave tests in `watcher_types.rs` (e.g. `:262, :292`) alone — they assert specific model names and are historical.

**Step 2: Verify compilation**

`cargo build 2>&1 | tail -5` → expect success.

**Step 3: Run all tests**

`cargo test 2>&1 | tail -20` → expect green (the `watcher_types.rs` tests at `:262, :292` check `claude-sonnet-4-20250514` but that's what the literal file content uses — unaffected by our refactor of the runtime default).

If any test fails because it asserted on the old default model, update the test to use `default_model()`.

**Step 4: Commit**

```bash
git add -u
git commit -m "refactor: route hardcoded model strings through default_model()"
```

---

### Task 3: Add `theme` to `SynapsConfig` and `load_config()`

**Files:**
- Modify: `src/core/config.rs`

**Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `src/core/config.rs`:

```rust
#[test]
fn load_config_parses_theme_key() {
    let dir = std::path::PathBuf::from("/tmp/synaps-config-test-theme/.synaps-cli");
    let _ = std::fs::create_dir_all(&dir);
    std::fs::write(dir.join("config"), "theme = dracula\n").unwrap();

    let original_home = std::env::var("HOME").ok();
    std::env::set_var("HOME", "/tmp/synaps-config-test-theme");

    let config = load_config();

    if let Some(home) = original_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    let _ = std::fs::remove_dir_all("/tmp/synaps-config-test-theme");

    assert_eq!(config.theme.as_deref(), Some("dracula"));
}
```

**Step 2: Run to confirm it fails**

`cargo test -p synaps-cli core::config::tests::load_config_parses_theme_key` → fails compile ("no field `theme`").

**Step 3: Implement**

In `SynapsConfig` struct, add:

```rust
pub theme: Option<String>,
```

In `Default`:

```rust
theme: None,
```

In `load_config`, add a match arm alongside the existing keys:

```rust
"theme" => config.theme = Some(val.to_string()),
```

Update `test_synaps_config_default` to assert `config.theme` is `None`.

**Step 4: Run tests**

`cargo test -p synaps-cli core::config` → all green.

**Step 5: Commit**

```bash
git add src/core/config.rs
git commit -m "feat(core): recognize theme key in SynapsConfig"
```

---

### Task 4: Implement `write_config_value()` — the line-oriented writer

**Files:**
- Modify: `src/core/config.rs`

**Step 1: Write failing tests**

Append to the test module in `src/core/config.rs`:

```rust
fn make_test_home(subdir: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(format!("/tmp/synaps-write-test-{}", subdir));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".synaps-cli")).unwrap();
    dir
}

fn with_home<F: FnOnce()>(home: &std::path::Path, f: F) {
    let original = std::env::var("HOME").ok();
    std::env::set_var("HOME", home);
    f();
    if let Some(h) = original {
        std::env::set_var("HOME", h);
    } else {
        std::env::remove_var("HOME");
    }
}

#[test]
fn write_config_value_replaces_existing_key() {
    let home = make_test_home("replace");
    let cfg = home.join(".synaps-cli/config");
    std::fs::write(&cfg, "model = claude-opus-4-6\nthinking = low\n").unwrap();

    with_home(&home, || {
        write_config_value("model", "claude-sonnet-4-6").unwrap();
    });

    let contents = std::fs::read_to_string(&cfg).unwrap();
    assert!(contents.contains("model = claude-sonnet-4-6"));
    assert!(contents.contains("thinking = low"));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn write_config_value_appends_when_missing() {
    let home = make_test_home("append");
    let cfg = home.join(".synaps-cli/config");
    std::fs::write(&cfg, "model = claude-opus-4-6\n").unwrap();

    with_home(&home, || {
        write_config_value("theme", "dracula").unwrap();
    });

    let contents = std::fs::read_to_string(&cfg).unwrap();
    assert!(contents.contains("model = claude-opus-4-6"));
    assert!(contents.contains("theme = dracula"));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn write_config_value_preserves_comments() {
    let home = make_test_home("comments");
    let cfg = home.join(".synaps-cli/config");
    std::fs::write(&cfg, "# user comment\nmodel = claude-opus-4-6\n# another\n").unwrap();

    with_home(&home, || {
        write_config_value("model", "claude-sonnet-4-6").unwrap();
    });

    let contents = std::fs::read_to_string(&cfg).unwrap();
    assert!(contents.contains("# user comment"));
    assert!(contents.contains("# another"));
    assert!(contents.contains("model = claude-sonnet-4-6"));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn write_config_value_preserves_unknown_keys() {
    let home = make_test_home("unknown");
    let cfg = home.join(".synaps-cli/config");
    std::fs::write(&cfg, "custom_thing = 42\nmodel = claude-opus-4-6\n").unwrap();

    with_home(&home, || {
        write_config_value("model", "claude-sonnet-4-6").unwrap();
    });

    let contents = std::fs::read_to_string(&cfg).unwrap();
    assert!(contents.contains("custom_thing = 42"));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn write_config_value_creates_file_if_absent() {
    let home = make_test_home("create");
    let cfg = home.join(".synaps-cli/config");
    assert!(!cfg.exists());

    with_home(&home, || {
        write_config_value("model", "claude-sonnet-4-6").unwrap();
    });

    let contents = std::fs::read_to_string(&cfg).unwrap();
    assert!(contents.contains("model = claude-sonnet-4-6"));
    let _ = std::fs::remove_dir_all(&home);
}
```

**Step 2: Run to confirm they fail**

`cargo test -p synaps-cli write_config_value 2>&1 | tail -10` → expect compile error ("cannot find function").

**Step 3: Implement**

Add to `src/core/config.rs` (below `load_config`):

```rust
/// Write a single `key = value` pair to `~/.synaps-cli/config` (or profile config).
/// Replaces the first existing line that matches the key, or appends if absent.
/// Preserves comments and unknown keys. Writes atomically via temp file + rename.
pub fn write_config_value(key: &str, value: &str) -> std::io::Result<()> {
    let path = resolve_write_path("config");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();

    let key_trimmed = key.trim();
    let replacement = format!("{} = {}", key_trimmed, value);

    let mut found = false;
    let mut new_lines: Vec<String> = existing.lines().map(|line| {
        if found { return line.to_string(); }
        let t = line.trim_start();
        if t.starts_with('#') || t.is_empty() { return line.to_string(); }
        if let Some((k, _)) = t.split_once('=') {
            if k.trim() == key_trimmed {
                found = true;
                return replacement.clone();
            }
        }
        line.to_string()
    }).collect();

    if !found {
        new_lines.push(replacement);
    }

    let mut out = new_lines.join("\n");
    if !out.ends_with('\n') { out.push('\n'); }

    // Atomic write
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}
```

**Step 4: Run tests**

`cargo test -p synaps-cli write_config_value` → 5 passed.

**Step 5: Commit**

```bash
git add src/core/config.rs
git commit -m "feat(core): add write_config_value() with comment/unknown-key preservation"
```

---

### Task 5: Refactor `/theme` command to use `write_config_value()`

**Files:**
- Modify: `src/chatui/app.rs:420-457` (the replace-or-append block in `handle_theme_command`)

**Step 1: Replace the ad-hoc writer**

Find the block starting around line 420 that builds `new_content` and calls `std::fs::write`. Replace it with:

```rust
match synaps_cli::config::write_config_value("theme", name) {
    Ok(_) => {
        self.push_msg(ChatMessage::System(
            format!("theme set to: {}. Restart to apply.", name)
        ));
    }
    Err(e) => {
        self.push_msg(ChatMessage::Error(
            format!("failed to write config: {}", e)
        ));
    }
}
```

Delete the now-unused `content`, `found`, `new_content`, `final_content` locals and the `create_dir_all` call.

**Step 2: Verify compilation and tests**

`cargo build && cargo test 2>&1 | tail -5` → expect green.

**Step 3: Commit**

```bash
git add src/chatui/app.rs
git commit -m "refactor(chatui): use write_config_value in /theme command"
```

---

## Phase 2 — Settings module scaffolding (no UI yet)

### Task 6: Create settings module skeleton with types

**Files:**
- Create: `src/chatui/settings/mod.rs`
- Create: `src/chatui/settings/schema.rs`
- Modify: `src/chatui/main.rs` (add `mod settings;`)

**Step 1: Write `src/chatui/settings/mod.rs`**

```rust
//! Settings modal — full-screen overlay opened via /settings.
//! Persists changes to ~/.synaps-cli/config and mutates Runtime where possible.

pub(super) mod schema;
mod draw;
mod input;

pub(super) use draw::render;
pub(super) use input::{handle_event, InputOutcome};

use schema::{Category, SettingDef};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Focus {
    Left,
    Right,
}

pub(super) enum ActiveEditor {
    Text { buffer: String, setting_key: &'static str, numeric: bool, error: Option<String> },
    Picker { setting_key: &'static str, options: Vec<String>, cursor: usize },
    CustomModel { buffer: String },  // "Custom…" branch of the model picker
}

pub(super) struct SettingsState {
    pub category_idx: usize,
    pub setting_idx: usize,
    pub focus: Focus,
    pub edit_mode: Option<ActiveEditor>,
    /// Transient error shown under a row if the last write failed.
    pub row_error: Option<(String, String)>, // (setting_key, error text)
}

impl SettingsState {
    pub fn new() -> Self {
        Self {
            category_idx: 0,
            setting_idx: 0,
            focus: Focus::Left,
            edit_mode: None,
            row_error: None,
        }
    }

    /// Settings in the currently selected category.
    pub fn current_settings(&self) -> Vec<&'static SettingDef> {
        let cat = schema::CATEGORIES[self.category_idx];
        schema::ALL_SETTINGS.iter().filter(|s| s.category == cat).collect()
    }

    pub fn current_setting(&self) -> Option<&'static SettingDef> {
        self.current_settings().get(self.setting_idx).copied()
    }
}
```

Create `src/chatui/settings/draw.rs` and `src/chatui/settings/input.rs` as empty stubs:

```rust
// draw.rs
use ratatui::Frame;
use ratatui::layout::Rect;
use super::SettingsState;

pub(super) fn render(_frame: &mut Frame, _area: Rect, _state: &SettingsState) {
    // Implemented in Task 10.
}
```

```rust
// input.rs
use crossterm::event::KeyEvent;
use super::SettingsState;

pub(super) enum InputOutcome {
    None,
    Close,
}

pub(super) fn handle_event(_state: &mut SettingsState, _key: KeyEvent) -> InputOutcome {
    // Implemented in Task 11.
    InputOutcome::None
}
```

**Step 2: Add `mod settings;` to `src/chatui/main.rs`**

Insert after `mod commands;`:

```rust
mod settings;
```

**Step 3: Write `src/chatui/settings/schema.rs`**

See Task 7.

**Step 4: Commit when Task 7 is done.**

---

### Task 7: Define the settings schema

**Files:**
- Create: `src/chatui/settings/schema.rs`

**Step 1: Write the schema**

```rust
//! Static list of settings exposed in the /settings menu.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Category {
    Model,
    Agent,
    ToolLimits,
    Appearance,
}

impl Category {
    pub fn label(&self) -> &'static str {
        match self {
            Category::Model => "Model",
            Category::Agent => "Agent",
            Category::ToolLimits => "Tool Limits",
            Category::Appearance => "Appearance",
        }
    }
}

pub(crate) const CATEGORIES: [Category; 4] = [
    Category::Model,
    Category::Agent,
    Category::ToolLimits,
    Category::Appearance,
];

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

pub(crate) const ALL_SETTINGS: &[SettingDef] = &[
    SettingDef {
        key: "model",
        label: "Model",
        category: Category::Model,
        editor: EditorKind::ModelPicker,
        help: "Which Claude model to use.",
    },
    SettingDef {
        key: "thinking",
        label: "Thinking",
        category: Category::Model,
        editor: EditorKind::Cycler(&["low", "medium", "high", "xhigh"]),
        help: "Extended thinking budget level.",
    },
    SettingDef {
        key: "skills",
        label: "Skills",
        category: Category::Agent,
        editor: EditorKind::Text { numeric: false },
        help: "Comma-separated skills to auto-load at startup.",
    },
    SettingDef {
        key: "api_retries",
        label: "API retries",
        category: Category::Agent,
        editor: EditorKind::Text { numeric: true },
        help: "Retries on transient API errors.",
    },
    SettingDef {
        key: "subagent_timeout",
        label: "Subagent timeout",
        category: Category::Agent,
        editor: EditorKind::Text { numeric: true },
        help: "Seconds before a dispatched subagent is canceled.",
    },
    SettingDef {
        key: "max_tool_output",
        label: "Max tool output",
        category: Category::ToolLimits,
        editor: EditorKind::Text { numeric: true },
        help: "Bytes to capture from a tool before truncating.",
    },
    SettingDef {
        key: "bash_timeout",
        label: "Bash timeout",
        category: Category::ToolLimits,
        editor: EditorKind::Text { numeric: true },
        help: "Default seconds allowed for a bash command.",
    },
    SettingDef {
        key: "bash_max_timeout",
        label: "Bash max timeout",
        category: Category::ToolLimits,
        editor: EditorKind::Text { numeric: true },
        help: "Upper bound on requested bash timeouts.",
    },
    SettingDef {
        key: "theme",
        label: "Theme",
        category: Category::Appearance,
        editor: EditorKind::ThemePicker,
        help: "Color theme (restart required).",
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use synaps_cli::config::SynapsConfig;

    /// Parity check — every setting key must be a field `load_config()` recognizes,
    /// so writes via the menu round-trip back through `SynapsConfig`. If this
    /// fails, either add a parser arm in `core/config.rs` or remove the setting.
    #[test]
    fn every_setting_key_is_known_to_load_config() {
        let valid = [
            "model", "thinking", "skills",
            "max_tool_output", "bash_timeout", "bash_max_timeout",
            "subagent_timeout", "api_retries", "theme",
        ];
        for def in ALL_SETTINGS {
            assert!(valid.contains(&def.key), "unknown setting key: {}", def.key);
        }
        // Conceptual round-trip: SynapsConfig default exists.
        let _ = SynapsConfig::default();
    }

    #[test]
    fn every_setting_belongs_to_known_category() {
        for def in ALL_SETTINGS {
            assert!(CATEGORIES.contains(&def.category));
        }
    }
}
```

**Step 2: Run tests**

`cargo test -p synaps-cli --bin chatui settings::schema 2>&1 | tail -10`

Note: tests inside a binary crate need `--bin chatui`. If that doesn't work, move the parity tests to `tests/settings_schema.rs` (integration tests).

**Step 3: Commit**

```bash
git add src/chatui/settings/
git add src/chatui/main.rs
git commit -m "feat(chatui): add settings module skeleton and schema"
```

---

## Phase 3 — Command wiring and empty modal

### Task 8: Add `CommandAction::OpenSettings` and `/settings` command

**Files:**
- Modify: `src/chatui/commands.rs`

**Step 1: Add variant to enum**

In the `CommandAction` enum, add:

```rust
OpenSettings,
```

**Step 2: Add `"settings"` to `ALL_COMMANDS`**

```rust
pub(super) const ALL_COMMANDS: &[&str] = &[
    "clear", "model", "system", "thinking", "sessions",
    "resume", "theme", "gamba", "help", "quit", "exit",
    "settings",
];
```

**Step 3: Handle the command**

In `handle_command`, add an arm:

```rust
"settings" => {
    return CommandAction::OpenSettings;
}
```

Also add `"settings"` to the help list in the `"help"` arm.

**Step 4: Verify compilation**

`cargo build 2>&1 | tail -5` → expect a warning that `OpenSettings` is unused (we'll wire it next).

**Step 5: Commit**

```bash
git add src/chatui/commands.rs
git commit -m "feat(chatui): add /settings command and OpenSettings action"
```

---

### Task 9: Wire `OpenSettings` in main event loop; add `settings` field to `App`

**Files:**
- Modify: `src/chatui/app.rs`
- Modify: `src/chatui/main.rs`

**Step 1: Add field to `App` struct**

In `src/chatui/app.rs`, add to the `App` struct:

```rust
pub(crate) settings: Option<super::settings::SettingsState>,
```

Add to `App::new`:

```rust
settings: None,
```

**Step 2: Handle `CommandAction::OpenSettings` in main.rs**

In the match around line 253-272 of `src/chatui/main.rs` (non-streaming command handler), add:

```rust
CommandAction::OpenSettings => {
    app.settings = Some(settings::SettingsState::new());
}
```

If the streaming-command handler doesn't already support `/settings`, don't add it there — settings is not in `STREAMING_COMMANDS`.

**Step 3: Verify compilation**

`cargo build 2>&1 | tail -5` → expect green.

**Step 4: Commit**

```bash
git add src/chatui/app.rs src/chatui/main.rs
git commit -m "feat(chatui): wire /settings to open modal state"
```

---

### Task 10: Render empty modal overlay

**Files:**
- Modify: `src/chatui/settings/draw.rs`
- Modify: `src/chatui/draw.rs`

**Step 1: Implement `settings::draw::render` — empty shell**

Replace `src/chatui/settings/draw.rs` with:

```rust
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders, BorderType, Clear, Paragraph};
use super::{SettingsState, Focus};
use super::schema::{CATEGORIES, SettingDef};
use crate::theme::THEME;

pub(super) fn render(frame: &mut Frame, area: Rect, state: &SettingsState) {
    // Centered modal — 80% width, 70% height, min 60x20
    let w = area.width.saturating_mul(8) / 10;
    let h = area.height.saturating_mul(7) / 10;
    let w = w.max(60).min(area.width);
    let h = h.max(20).min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let modal = Rect { x, y, width: w, height: h };

    frame.render_widget(Clear, modal);
    let block = Block::default()
        .title(" Settings ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(THEME.border_active))
        .style(Style::default().bg(THEME.bg));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    // Split into left (categories) and right (settings) with a footer hint line
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(20), Constraint::Min(1)])
        .split(outer[0]);

    render_categories(frame, panes[0], state);
    render_settings(frame, panes[1], state);
    render_footer(frame, outer[1], state);
}

fn render_categories(frame: &mut Frame, area: Rect, state: &SettingsState) {
    let mut lines = Vec::new();
    for (i, cat) in CATEGORIES.iter().enumerate() {
        let marker = if i == state.category_idx { "▸ " } else { "  " };
        let style = if i == state.category_idx && state.focus == Focus::Left {
            Style::default().fg(THEME.claude_label)
        } else if i == state.category_idx {
            Style::default().fg(THEME.claude_text)
        } else {
            Style::default().fg(THEME.help_fg)
        };
        lines.push(ratatui::text::Line::from(vec![
            ratatui::text::Span::styled(format!("{}{}", marker, cat.label()), style),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_settings(frame: &mut Frame, area: Rect, state: &SettingsState) {
    let settings = state.current_settings();
    let mut lines = Vec::new();
    for (i, def) in settings.iter().enumerate() {
        let selected = i == state.setting_idx && state.focus == Focus::Right;
        let style = if selected {
            Style::default().fg(THEME.claude_label)
        } else {
            Style::default().fg(THEME.claude_text)
        };
        let current_value = current_value_for(def);
        lines.push(ratatui::text::Line::from(vec![
            ratatui::text::Span::styled(format!("  {:<20} {}", def.label, current_value), style),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_footer(frame: &mut Frame, area: Rect, _state: &SettingsState) {
    let hint = "↑↓ navigate  Tab switch pane  Enter edit  Esc close";
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(THEME.help_fg)),
        area,
    );
}

/// Read the persisted value for a setting from the config file.
/// Returns "(default)" if not set. Implemented fully in Task 12.
fn current_value_for(_def: &SettingDef) -> String {
    String::from("(...)")
}
```

**Step 2: Overlay in main draw function**

In `src/chatui/draw.rs`, find the main `draw` function. After all existing widgets render (near end of the function body), add:

```rust
if let Some(ref state) = app.settings {
    crate::settings::render(frame, frame.size(), state);
}
```

The exact location depends on `draw()`'s structure — it should be the last thing drawn so it appears on top. If `draw` uses `terminal.draw(|f| {...})`, add it inside the closure at the end.

**Step 3: Run manually**

```bash
cargo run --bin chatui
```

Type `/settings`. Expect: modal overlay appears with "Settings" title, "Model/Agent/Tool Limits/Appearance" on left, settings list on right (with "(...)" placeholder values), footer hint. Esc won't close yet (Task 11).

Type `/quit` to exit. If modal looks wrong, fix before proceeding.

**Step 4: Commit**

```bash
git add src/chatui/settings/draw.rs src/chatui/draw.rs
git commit -m "feat(chatui): render empty settings modal overlay"
```

---

### Task 11: Close modal with Esc, navigate with arrows/Tab

**Files:**
- Modify: `src/chatui/settings/input.rs`
- Modify: `src/chatui/input.rs`

**Step 1: Implement input handling**

Replace `src/chatui/settings/input.rs`:

```rust
use crossterm::event::{KeyCode, KeyModifiers, KeyEvent};
use super::{SettingsState, Focus};
use super::schema::CATEGORIES;

pub(super) enum InputOutcome {
    None,
    Close,
}

pub(super) fn handle_event(state: &mut SettingsState, key: KeyEvent) -> InputOutcome {
    // TODO Task 13-16: route to active editor first if one is open
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => InputOutcome::Close,
        (KeyCode::Tab, _) | (KeyCode::Char('h'), KeyModifiers::CONTROL) => {
            state.focus = match state.focus { Focus::Left => Focus::Right, Focus::Right => Focus::Left };
            state.row_error = None;
            InputOutcome::None
        }
        (KeyCode::Up, _) => {
            match state.focus {
                Focus::Left => {
                    if state.category_idx > 0 { state.category_idx -= 1; state.setting_idx = 0; }
                }
                Focus::Right => {
                    if state.setting_idx > 0 { state.setting_idx -= 1; }
                }
            }
            state.row_error = None;
            InputOutcome::None
        }
        (KeyCode::Down, _) => {
            match state.focus {
                Focus::Left => {
                    if state.category_idx + 1 < CATEGORIES.len() {
                        state.category_idx += 1;
                        state.setting_idx = 0;
                    }
                }
                Focus::Right => {
                    let n = state.current_settings().len();
                    if state.setting_idx + 1 < n { state.setting_idx += 1; }
                }
            }
            state.row_error = None;
            InputOutcome::None
        }
        _ => InputOutcome::None,
    }
}
```

**Step 2: Route events to settings module in main input handler**

At the very top of `handle_event` in `src/chatui/input.rs` (before the normal routing), check for the settings modal:

```rust
pub(super) fn handle_event(
    event: Event,
    app: &mut App,
    streaming: bool,
) -> InputAction {
    // Route keys to the settings modal while it's open.
    if app.settings.is_some() {
        if let Event::Key(key) = event {
            let state = app.settings.as_mut().expect("just checked");
            match crate::settings::handle_event(state, key) {
                crate::settings::InputOutcome::Close => { app.settings = None; }
                crate::settings::InputOutcome::None => {}
            }
        }
        // Swallow all events while settings is open.
        return InputAction::None;
    }
    // ... existing code below unchanged
    match event {
        // ...
    }
}
```

Be careful to swallow *all* event types, not just keys (so mouse scroll doesn't scroll chat while the modal is open).

**Step 3: Manual test**

```bash
cargo run --bin chatui
```

- Type `/settings` → modal opens
- Press ↓ / ↑ → category selection moves (left pane marker `▸` moves)
- Press Tab → focus jumps to right pane (setting row highlighted)
- Press ↓ / ↑ → setting row selection moves
- Press Esc → modal closes, chat returns

If any step fails, fix before committing.

**Step 4: Commit**

```bash
git add src/chatui/settings/input.rs src/chatui/input.rs
git commit -m "feat(chatui): navigate settings modal with arrows/Tab, Esc to close"
```

---

## Phase 4 — Reading current values and editors

### Task 12: Read current values for display

**Files:**
- Modify: `src/chatui/settings/draw.rs`
- Modify: `src/chatui/app.rs` (pass runtime snapshot to draw)

**Step 1: Thread a runtime snapshot into the draw call**

The `draw::render` in `chatui/draw.rs` already receives `runtime.model()` and `runtime.thinking_level()`. Look at the existing signature and pass the same values into `settings::render`. Minimal snapshot struct is easiest:

In `src/chatui/settings/mod.rs`:

```rust
pub(super) struct RuntimeSnapshot<'a> {
    pub model: &'a str,
    pub thinking: &'a str,
    pub max_tool_output: usize,
    pub bash_timeout: u64,
    pub bash_max_timeout: u64,
    pub subagent_timeout: u64,
    pub api_retries: u32,
    pub skills: Option<&'a [String]>,
    pub theme_name: String, // read from config
}
```

Update `draw::render` signature to take `&RuntimeSnapshot`. Update the caller in `chatui/draw.rs` to build and pass one.

The `Runtime` struct likely needs getters for fields that don't already have them (`max_tool_output`, etc.). Add them if missing (trivial one-liners).

**Step 2: Implement `current_value_for`**

```rust
fn current_value_for(def: &SettingDef, snap: &RuntimeSnapshot) -> String {
    match def.key {
        "model" => snap.model.to_string(),
        "thinking" => snap.thinking.to_string(),
        "skills" => snap.skills.map(|s| s.join(",")).unwrap_or_else(|| "(none)".into()),
        "api_retries" => snap.api_retries.to_string(),
        "subagent_timeout" => format!("{}s", snap.subagent_timeout),
        "max_tool_output" => snap.max_tool_output.to_string(),
        "bash_timeout" => format!("{}s", snap.bash_timeout),
        "bash_max_timeout" => format!("{}s", snap.bash_max_timeout),
        "theme" => snap.theme_name.clone(),
        _ => "?".into(),
    }
}
```

**Step 3: Manual test**

Run `cargo run --bin chatui`, open `/settings`, verify real values appear on the right pane (not "(...)").

**Step 4: Commit**

```bash
git add -u
git commit -m "feat(chatui): show current values in settings modal"
```

---

### Task 13: Cycler editor (`thinking: low ◀ ▶ xhigh`)

**Files:**
- Modify: `src/chatui/settings/input.rs`
- Modify: `src/chatui/settings/draw.rs`
- Modify: `src/chatui/main.rs` (to apply changes to `runtime`)

**Design for apply:** `input::handle_event` returns a richer outcome — e.g.:

```rust
pub(super) enum InputOutcome {
    None,
    Close,
    Apply { key: &'static str, value: String },
}
```

The caller in `main.rs` pattern-matches `Apply` and: (a) calls `runtime.set_*`, (b) calls `write_config_value(...)`, (c) on write error stores error into `state.row_error`.

**Step 1: Update `InputOutcome` and routing in main.rs**

- Add `Apply { key, value }` variant.
- In `main.rs`, after `handle_event` on settings, pattern-match the outcome. For each key, call appropriate runtime setter. Then call `write_config_value`. On `Err`, set `state.row_error`. Capture the `state` via `app.settings.as_mut()`.

**Step 2: Handle ◀ / ▶ in settings input**

In `settings/input.rs` add, before the catch-all:

```rust
(KeyCode::Left, _) | (KeyCode::Right, _) if state.focus == Focus::Right => {
    if let Some(def) = state.current_setting() {
        if let EditorKind::Cycler(options) = def.editor {
            let current = /* read current value from snapshot — but input.rs has none */ 
            // ...
        }
    }
    InputOutcome::None
}
```

Because `settings/input.rs` doesn't have a runtime snapshot, pass one as an argument to `handle_event`. Alternatively, pre-compute the new value in `main.rs` by reading `runtime.thinking_level()` and writing the adjusted value. Simplest: **pass `&RuntimeSnapshot` into `handle_event`**.

Update signature:

```rust
pub(super) fn handle_event(
    state: &mut SettingsState,
    key: KeyEvent,
    snap: &RuntimeSnapshot,
) -> InputOutcome { ... }
```

Then implement cycler logic: find current index of `snap.thinking` in `options`, step left/right (clamp at ends), emit `Apply { key: "thinking", value: options[new_idx].to_string() }`.

**Step 3: Apply `thinking` in main.rs**

Map `value` to budget:

```rust
let budget = match value.as_str() {
    "low" => 2048,
    "medium" => 4096,
    "high" => 16384,
    "xhigh" => 32768,
    _ => return, // ignore
};
runtime.set_thinking_budget(budget);
if let Err(e) = synaps_cli::config::write_config_value("thinking", &value) {
    if let Some(st) = app.settings.as_mut() {
        st.row_error = Some(("thinking".into(), e.to_string()));
    }
}
```

**Step 4: Render ◀ ▶ affordance**

In `draw::render_settings`, when the current setting is a `Cycler` and the row is selected+Right focus, render `◀ {value} ▶` instead of plain value.

**Step 5: Manual test**

- Open `/settings`, select "Thinking" row, press ◀ / ▶
- Value cycles through `low/medium/high/xhigh`
- Send a message → header shows new thinking level persisted
- Quit and relaunch → open `/settings` → value is the one you set (confirms persistence)

**Step 6: Commit**

```bash
git add -u
git commit -m "feat(chatui): cycler editor for thinking level with apply+persist"
```

---

### Task 14: Text input editor (numeric + free-form)

**Files:**
- Modify: `src/chatui/settings/input.rs`
- Modify: `src/chatui/settings/draw.rs`

**Step 1: Enter opens editor**

In `handle_event`, when key is Enter and the selected setting is `Text { numeric }`:

```rust
state.edit_mode = Some(ActiveEditor::Text {
    buffer: current_value_for(def, snap),
    setting_key: def.key,
    numeric: *numeric,
    error: None,
});
return InputOutcome::None;
```

**Step 2: Route keys to active editor when one is open**

At the top of `handle_event`, before other matching:

```rust
if let Some(editor) = state.edit_mode.as_mut() {
    return handle_editor_key(editor, key);
}
```

Implement `handle_editor_key`:

```rust
fn handle_editor_key(editor: &mut ActiveEditor, key: KeyEvent) -> InputOutcome {
    match editor {
        ActiveEditor::Text { buffer, setting_key, numeric, error } => {
            match key.code {
                KeyCode::Esc => { /* outer closes editor */ InputOutcome::None }  // see below
                KeyCode::Enter => {
                    if *numeric && buffer.parse::<u64>().is_err() {
                        *error = Some("must be a number".to_string());
                        return InputOutcome::None;
                    }
                    InputOutcome::Apply { key: setting_key, value: buffer.clone() }
                }
                KeyCode::Backspace => { buffer.pop(); *error = None; InputOutcome::None }
                KeyCode::Char(c) => { buffer.push(c); *error = None; InputOutcome::None }
                _ => InputOutcome::None,
            }
        }
        _ => InputOutcome::None, // pickers in Task 15-16
    }
}
```

Subtle: Esc needs to close the editor *without* closing the whole modal. Re-structure: if editor is open and key is Esc, clear `state.edit_mode = None` and return `None` — don't bubble up to the modal-close path.

Adjust: handle Esc-closes-editor at the top before dispatching:

```rust
if let Some(editor) = state.edit_mode.as_mut() {
    if key.code == KeyCode::Esc {
        state.edit_mode = None;
        return InputOutcome::None;
    }
    return handle_editor_key(editor, key);
}
```

**Step 3: Main.rs applies edits for each text-input key**

Extend the `Apply { key, value }` handler:

```rust
match key {
    "thinking" => { ... existing ... }
    "api_retries" => {
        if let Ok(n) = value.parse::<u32>() { runtime.set_api_retries(n); }
        // persist
    }
    "bash_timeout" => { if let Ok(n) = value.parse::<u64>() { runtime.set_bash_timeout(n); } }
    "bash_max_timeout" => { if let Ok(n) = value.parse::<u64>() { runtime.set_bash_max_timeout(n); } }
    "subagent_timeout" => { if let Ok(n) = value.parse::<u64>() { runtime.set_subagent_timeout(n); } }
    "max_tool_output" => { if let Ok(n) = value.parse::<usize>() { runtime.set_max_tool_output(n); } }
    "skills" => {
        // Persist only — re-loading skills in-session is out of scope (matches existing behavior)
    }
    _ => {}
}
// Then: write_config_value(key, &value) in one place, set row_error on Err.
// Also: state.edit_mode = None on success.
```

Add setters to `Runtime` if missing.

**Step 4: Render editor in draw**

When the selected row has an active Text editor, render it with a box or underline and a visible cursor (e.g. append `_`). Show the error line below the row if present.

Minimal implementation: replace the value column with `[{buffer}_]`. No separate Rect — inline in the list.

**Step 5: Manual test**

- Open `/settings` → navigate to "API retries" → Enter → type `abc` → Enter → red "must be a number" error shows, still in editor
- Backspace to clear, type `5`, Enter → editor closes, value shows `5`
- Relaunch → value persisted

**Step 6: Commit**

```bash
git add -u
git commit -m "feat(chatui): text input editor with numeric validation"
```

---

### Task 15: Model dropdown picker with "Custom…" fallthrough

**Files:**
- Modify: `src/chatui/settings/input.rs`
- Modify: `src/chatui/settings/draw.rs`

**Step 1: Enter on Model row opens picker**

```rust
ActiveEditor::Picker {
    setting_key: "model",
    options: {
        let mut opts: Vec<String> = synaps_cli::models::KNOWN_MODELS
            .iter().map(|(id, desc)| format!("{}  — {}", id, desc)).collect();
        opts.push("Custom…".to_string());
        opts
    },
    cursor: 0, // or position at current model
}
```

Preselect the current model if it matches a `KNOWN_MODELS` entry.

**Step 2: Handle picker keys**

```rust
ActiveEditor::Picker { setting_key, options, cursor } => {
    match key.code {
        KeyCode::Up => { if *cursor > 0 { *cursor -= 1; } InputOutcome::None }
        KeyCode::Down => { if *cursor + 1 < options.len() { *cursor += 1; } InputOutcome::None }
        KeyCode::Enter => {
            let selection = &options[*cursor];
            if setting_key == &"model" && selection == "Custom…" {
                // Transition to CustomModel text input
                return InputOutcome::SwitchEditor(ActiveEditor::CustomModel { buffer: String::new() });
            }
            // Pull just the ID (before the "  — ")
            let value = selection.split("  —").next().unwrap_or(selection).trim().to_string();
            InputOutcome::Apply { key: setting_key, value }
        }
        _ => InputOutcome::None,
    }
}
```

Add `InputOutcome::SwitchEditor(ActiveEditor)` and handle it in the top-level dispatcher (replaces `state.edit_mode`).

**Step 3: Handle `CustomModel`**

```rust
ActiveEditor::CustomModel { buffer } => {
    match key.code {
        KeyCode::Enter => InputOutcome::Apply { key: "model", value: buffer.clone() },
        KeyCode::Backspace => { buffer.pop(); InputOutcome::None }
        KeyCode::Char(c) => { buffer.push(c); InputOutcome::None }
        _ => InputOutcome::None,
    }
}
```

**Step 4: Render picker**

When Model row is open:
- Dim the settings list behind
- Draw a small inner box listing options, highlight at `cursor`
- Custom… gets a text input when selected

Simplest rendering: a Rect 30×10 centered in the right pane, with each option as a Paragraph line.

**Step 5: Manual test**

- `/settings` → Model row → Enter → dropdown appears with `claude-opus-4-7`, `claude-sonnet-4-6`, `claude-haiku-4-5-20251001`, `Custom…`
- Select Sonnet → Enter → value updates, editor closes
- Re-open → Enter → pick Custom… → Enter → type `claude-something-else` → Enter → value persists
- Relaunch → `/settings` shows `claude-something-else`

**Step 6: Commit**

```bash
git add -u
git commit -m "feat(chatui): model picker with custom fallthrough"
```

---

### Task 16: Theme dropdown picker

**Files:**
- Modify: `src/chatui/settings/input.rs`
- Modify: `src/chatui/settings/draw.rs`

**Step 1: Enumerate themes**

Build the options list = the 18 built-in names (copy from `src/chatui/app.rs:384-402` descriptions array) + any files in `~/.synaps-cli/themes/`. Factor the built-in list into a shared constant in `src/chatui/theme/mod.rs` if it's still hardcoded inline — otherwise duplicate is fine (YAGNI).

**Step 2: Enter opens theme picker**

Same pattern as model picker. `Apply { key: "theme", value: name }`.

**Step 3: Main.rs theme apply**

Theme is read via LazyLock and can't be mutated at runtime. For consistency with `/theme`, just persist:

```rust
"theme" => {
    if let Err(e) = synaps_cli::config::write_config_value("theme", &value) {
        /* set row_error */
    } else {
        if let Some(st) = app.settings.as_mut() {
            st.row_error = Some(("theme".into(), "saved — restart to apply".into()));
        }
    }
}
```

(Overloading `row_error` for a success-notice is slightly ugly. If it bothers you, add `row_note: Option<(String, String)>` alongside. YAGNI for now.)

**Step 4: Manual test**

- `/settings` → Appearance → Theme → Enter → picker shows all themes
- Select `dracula` → Enter → notice "saved — restart to apply"
- Quit, relaunch → theme is dracula

**Step 5: Commit**

```bash
git add -u
git commit -m "feat(chatui): theme picker with built-ins and custom themes"
```

---

## Phase 5 — Polish and verification

### Task 17: Display row errors and notes

**Files:**
- Modify: `src/chatui/settings/draw.rs`

**Step 1: Render error/note below the selected row**

After the settings list, if `state.row_error` matches the currently-selected row's key, draw a 1-line message below it:

```rust
if let Some((key, msg)) = &state.row_error {
    if Some(*key) == state.current_setting().map(|d| d.key) {
        // Draw msg on the next line after the selected row.
        // Use THEME.error_color or THEME.help_fg based on whether it's error or note.
    }
}
```

**Step 2: Manual test write failure**

- Temporarily `chmod 444 ~/.synaps-cli/config`, open `/settings`, change a value
- Error line appears under that row
- `chmod 600` to restore

**Step 3: Commit**

```bash
git add src/chatui/settings/draw.rs
git commit -m "feat(chatui): show per-row write errors in settings modal"
```

---

### Task 18: Full manual verification pass

**No code changes. Checklist:**

```bash
cargo build --release
cargo test 2>&1 | tail -5   # all green
```

Backup your config: `cp ~/.synaps-cli/config ~/.synaps-cli/config.bak`

Then walk through every editor type in a live run of `cargo run --bin chatui`:

- [ ] `/settings` opens modal; chat preserved underneath
- [ ] Esc closes cleanly; `/settings` reopens
- [ ] Tab toggles pane focus; arrows navigate within pane
- [ ] Thinking cycler: ◀ ▶ steps through all four levels; persists after restart
- [ ] Model picker: select Sonnet → applies to next message (check header); persists
- [ ] Model Custom…: arbitrary string accepted and persisted
- [ ] Text editor (api_retries): "abc" shows error, "5" accepted and persisted
- [ ] Text editor (bash_timeout): same flow with seconds unit rendered
- [ ] Skills: comma-separated string persists; at restart, those skills load
- [ ] Theme picker: selection persists; restart applies new theme
- [ ] Config file inspection: `cat ~/.synaps-cli/config` shows clean key=value pairs, comments preserved
- [ ] Unknown keys preserved: add `foo = bar` to config, change any setting via menu, foo still present
- [ ] `/help` lists `/settings`

Restore config: `cp ~/.synaps-cli/config.bak ~/.synaps-cli/config`

**Commit none** — this is verification only.

If any checklist item fails, open a follow-up commit before merging.

---

### Task 19: Update CHANGELOG

**Files:**
- Modify: `CHANGELOG.md`

Add an entry under the unreleased section:

```markdown
- feat: interactive `/settings` menu with persistent config writes
```

Commit:

```bash
git add CHANGELOG.md
git commit -m "docs: changelog for /settings menu"
```

---

## Done

Branch ready for review / merge via `superpowers:finishing-a-development-branch`.

## Notes

- Theme changes require restart (matches existing `/theme` behavior — YAGNI on runtime theme reload, which would require dropping the LazyLock).
- System prompt is intentionally not in this menu — use the existing `/system` command.
- The line-oriented config writer is deliberately simple; a TOML migration is tracked as a separate concern (see design doc).
