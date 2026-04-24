# Plugin Keybinds — Design Spec

**Goal:** Let plugins register custom keyboard shortcuts in the TUI. Users see keybinds in settings, plugins get a clean declaration surface, core binds are never overridable.

**Branch:** `feat/plugin-keybinds`

---

## Problem

All keybinds are hardcoded in `src/chatui/input.rs` as a static `match` block. Plugins and skills have no way to register keyboard shortcuts. Users can't customize binds.

## Design

### Declaration (plugin side)

Plugins declare keybinds in their `plugin.json` manifest:

```json
{
  "name": "dev-tools",
  "version": "0.1.0",
  "keybinds": [
    {
      "key": "C-S-s",
      "action": "slash_command",
      "command": "scholar",
      "description": "Scholar search"
    },
    {
      "key": "C-S-b",
      "action": "load_skill",
      "skill": "black-box-engineering",
      "description": "Load BBE pipeline"
    },
    {
      "key": "C-S-r",
      "action": "inject_prompt",
      "prompt": "Review the code I'm currently working on",
      "description": "Quick code review"
    }
  ]
}
```

### Key Format

Modifier-key notation, matching crossterm's model:

| Notation | Meaning |
|----------|---------|
| `C-x` | Ctrl+x |
| `S-x` | Shift+x (uppercase letter) |
| `A-x` | Alt+x |
| `C-S-x` | Ctrl+Shift+x |
| `C-A-x` | Ctrl+Alt+x |
| `F1`–`F12` | Function keys |

Examples: `C-S-s` = Ctrl+Shift+S, `A-p` = Alt+P, `F5` = F5

### Action Types

| Action | Fields | Behavior |
|--------|--------|----------|
| `slash_command` | `command: str` | Execute as if user typed `/{command}` |
| `load_skill` | `skill: str` | Load a skill by name |
| `inject_prompt` | `prompt: str` | Submit text as a user message |
| `run_script` | `script: str` | Execute `{plugin_dir}/{script}` via bash, inject output as system message |

### User Overrides

Users can override or disable plugin keybinds in `~/.synaps-cli/config`:

```
# Override a plugin keybind
keybind.C-S-s = /search
keybind.C-S-b = disabled

# Add custom keybinds (no plugin needed)
keybind.C-S-t = /theme cyberpunk
keybind.F5 = /compact
```

### Priority (conflict resolution)

```
1. Core keybinds (Ctrl+C, Esc, Enter, etc.)  →  NEVER overridable
2. User config keybinds                       →  override plugins
3. Plugin keybinds                            →  lowest priority
```

If two plugins register the same key: first-loaded wins, warning logged. User config always wins over plugins.

### Core Keybinds (reserved, non-overridable)

```
Ctrl+C          Quit
Esc             Abort stream
Enter           Submit
Shift+Enter     Newline
Tab             Autocomplete
Ctrl+A          Cursor start
Ctrl+E          Cursor end
Ctrl+U          Clear input
Ctrl+W          Delete word
Ctrl+O          Toggle output
Alt+Left/Right  Jump word
Shift+Up/Down   Scroll
Up/Down         History
```

Any plugin or user keybind matching these is silently ignored.

---

## Implementation

### New Files

| File | Purpose |
|------|---------|
| `src/skills/keybinds.rs` | `KeybindRegistry`, parsing, matching |

### Modified Files

| File | Change |
|------|--------|
| `src/skills/mod.rs` | Build keybind registry during plugin load |
| `src/skills/manifest.rs` | Parse `keybinds` from plugin.json |
| `src/chatui/input.rs` | Check registry before static match |
| `src/chatui/commands.rs` | Add `/keybinds` command |
| `src/chatui/settings/` | Show keybinds in settings modal |
| `src/core/config.rs` | Parse `keybind.*` from config |

### Data Structures

```rust
// src/skills/keybinds.rs

use crossterm::event::{KeyCode, KeyModifiers};

#[derive(Debug, Clone)]
pub struct KeyCombo {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

#[derive(Debug, Clone)]
pub enum KeybindAction {
    SlashCommand(String),
    LoadSkill(String),
    InjectPrompt(String),
    RunScript(String, PathBuf),  // (script path, plugin dir)
    Disabled,
}

#[derive(Debug, Clone)]
pub struct Keybind {
    pub key: KeyCombo,
    pub action: KeybindAction,
    pub description: String,
    pub source: KeybindSource,
}

#[derive(Debug, Clone)]
pub enum KeybindSource {
    Core,
    User,
    Plugin(String),  // plugin name
}

pub struct KeybindRegistry {
    binds: Vec<Keybind>,
    reserved: HashSet<(KeyCode, KeyModifiers)>,
}

impl KeybindRegistry {
    pub fn new() -> Self { ... }
    
    /// Register core (reserved) keybinds — called once at startup
    pub fn register_core(&mut self) { ... }
    
    /// Register plugin keybinds from manifest
    pub fn register_plugin(&mut self, plugin: &str, keybinds: &[ManifestKeybind]) { ... }
    
    /// Register user overrides from config
    pub fn register_user(&mut self, config_keybinds: &HashMap<String, String>) { ... }
    
    /// Match a key event — returns action if registered
    pub fn match_key(&self, code: KeyCode, modifiers: KeyModifiers) -> Option<&Keybind> { ... }
    
    /// List all registered keybinds (for /keybinds command and settings)
    pub fn all(&self) -> &[Keybind] { ... }
    
    /// Parse key notation ("C-S-s") into KeyCombo
    pub fn parse_key(notation: &str) -> Result<KeyCombo, String> { ... }
}
```

### Input Handler Integration

```rust
// In src/chatui/input.rs handle_key()

fn handle_key(
    code: KeyCode,
    modifiers: KeyModifiers,
    app: &mut App,
    streaming: bool,
    registry: &Arc<CommandRegistry>,
    keybinds: &KeybindRegistry,       // NEW
) -> InputAction {
    app.clear_selection();
    if !matches!(code, KeyCode::Tab) { app.tab_cycle = None; }
    
    // === PLUGIN/USER KEYBINDS (before core, after selection clear) ===
    // Skip during streaming — only core binds work while streaming
    if !streaming {
        if let Some(bind) = keybinds.match_key(code, modifiers) {
            return match &bind.action {
                KeybindAction::SlashCommand(cmd) => {
                    let parts: Vec<&str> = cmd.splitn(2, ' ').collect();
                    InputAction::SlashCommand(parts[0].to_string(), 
                        parts.get(1).unwrap_or(&"").to_string())
                }
                KeybindAction::InjectPrompt(text) => {
                    InputAction::Submit(text.clone())
                }
                KeybindAction::LoadSkill(skill) => {
                    InputAction::SlashCommand("load".to_string(), skill.clone())
                }
                KeybindAction::Disabled => InputAction::None,
                _ => InputAction::None,
            };
        }
    }
    
    // === CORE KEYBINDS (existing match block) ===
    match (code, modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => { ... }
        // ... rest unchanged
    }
}
```

### Key Notation Parser

```rust
/// Parse "C-S-s" → KeyCombo { code: KeyCode::Char('s'), modifiers: CTRL | SHIFT }
pub fn parse_key(notation: &str) -> Result<KeyCombo, String> {
    let parts: Vec<&str> = notation.split('-').collect();
    let mut modifiers = KeyModifiers::empty();
    
    for part in &parts[..parts.len()-1] {
        match *part {
            "C" => modifiers |= KeyModifiers::CONTROL,
            "S" => modifiers |= KeyModifiers::SHIFT,
            "A" => modifiers |= KeyModifiers::ALT,
            other => return Err(format!("Unknown modifier: {}", other)),
        }
    }
    
    let key_str = parts.last().unwrap();
    let code = match *key_str {
        k if k.len() == 1 => KeyCode::Char(k.chars().next().unwrap()),
        k if k.starts_with('F') => {
            let n: u8 = k[1..].parse().map_err(|_| format!("Invalid F-key: {}", k))?;
            KeyCode::F(n)
        }
        "Space" => KeyCode::Char(' '),
        "Tab" => KeyCode::Tab,
        "Enter" => KeyCode::Enter,
        "Esc" => KeyCode::Esc,
        other => return Err(format!("Unknown key: {}", other)),
    };
    
    Ok(KeyCombo { code, modifiers })
}
```

---

## Discoverability

### `/keybinds` command

```
Keybinds:
  Core:
    Ctrl+C           Quit
    Esc              Abort stream
    Enter            Submit
    ...
  
  Plugins:
    Ctrl+Shift+S     Scholar search (dev-tools)
    Ctrl+Shift+B     Load BBE (dev-tools)
  
  User:
    F5               /compact
```

### Settings modal

Add a "Keybinds" section to the settings modal showing all registered binds with source attribution.

---

## Edge Cases

1. **Keybind matches but plugin is disabled** → skip, fall through to next
2. **Key notation parse error in plugin.json** → log warning, skip that bind, don't crash
3. **Duplicate key across plugins** → first-loaded wins, log warning with both plugin names
4. **User sets `keybind.X = disabled`** → registers as `KeybindAction::Disabled`, blocks plugin bind
5. **Streaming mode** → plugin keybinds are suppressed, only core binds active
6. **Keybind fires slash command that doesn't exist** → standard "unknown command" error
7. **Terminal doesn't support Ctrl+Shift combos** → some terminals can't distinguish, document this

---

## Tasks

1. Add `KeybindRegistry` + `KeyCombo` + `KeybindAction` types (`src/skills/keybinds.rs`)
2. Implement `parse_key()` notation parser with tests
3. Parse `keybinds` from plugin.json manifest (`src/skills/manifest.rs`)
4. Build registry during plugin load (`src/skills/mod.rs`)
5. Parse `keybind.*` from user config (`src/core/config.rs`)
6. Integrate registry into `handle_key()` (`src/chatui/input.rs`)
7. Add `/keybinds` command (`src/chatui/commands.rs`)
8. Add keybinds section to settings modal (`src/chatui/settings/`)
9. Tests: parser, registry, conflict resolution, disabled binds
10. Documentation: AGENTS.md keybind section, README mention

**Estimated LOC:** ~400-500 Rust (keybinds.rs ~200, parser+tests ~100, wiring ~100-200)
