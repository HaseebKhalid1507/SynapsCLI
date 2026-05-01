//! Plugin keybinds — registry, parser, and matching for custom keyboard shortcuts.
//!
//! Plugins declare keybinds in `plugin.json`. Users override in config.
//! Core keybinds (Ctrl+C, Esc, etc.) are never overridable.

use crossterm::event::{KeyCode, KeyModifiers};
use std::collections::HashSet;
use std::path::PathBuf;

/// A key combination (modifiers + key).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

/// What happens when a keybind fires.
#[derive(Debug, Clone)]
pub enum KeybindAction {
    /// Execute a slash command (e.g. "scholar quantum")
    SlashCommand(String),
    /// Load a skill by name
    LoadSkill(String),
    /// Submit text as a user message
    InjectPrompt(String),
    /// Run a script and inject output as system message
    RunScript { script: String, plugin_dir: PathBuf },
    /// Explicitly disabled (user override)
    Disabled,
}

/// Where a keybind came from — for conflict resolution and display.
#[derive(Debug, Clone, PartialEq)]
pub enum KeybindSource {
    Core,
    User,
    Plugin(String),
}

/// A registered keybind.
#[derive(Debug, Clone)]
pub struct Keybind {
    pub key: KeyCombo,
    pub action: KeybindAction,
    pub description: String,
    pub source: KeybindSource,
}

/// Registry of all keybinds with conflict resolution.
#[derive(Debug, Clone)]
pub struct KeybindRegistry {
    binds: Vec<Keybind>,
    reserved: HashSet<KeyCombo>,
}

impl KeybindRegistry {
    pub fn new() -> Self {
        let mut registry = Self {
            binds: Vec::new(),
            reserved: HashSet::new(),
        };
        registry.register_core();
        registry
    }

    /// Register core keybinds that can never be overridden.
    fn register_core(&mut self) {
        let core_keys = vec![
            (KeyCode::Char('c'), KeyModifiers::CONTROL, "Quit"),
            (KeyCode::Esc, KeyModifiers::NONE, "Abort stream"),
            (KeyCode::Enter, KeyModifiers::NONE, "Submit"),
            (KeyCode::Enter, KeyModifiers::SHIFT, "Newline"),
            (KeyCode::Tab, KeyModifiers::NONE, "Autocomplete"),
            (KeyCode::Char('a'), KeyModifiers::CONTROL, "Cursor start"),
            (KeyCode::Char('e'), KeyModifiers::CONTROL, "Cursor end"),
            (KeyCode::Char('u'), KeyModifiers::CONTROL, "Clear input"),
            (KeyCode::Char('w'), KeyModifiers::CONTROL, "Delete word"),
            (KeyCode::Char('o'), KeyModifiers::CONTROL, "Toggle output"),
            (KeyCode::Left, KeyModifiers::ALT, "Jump word left"),
            (KeyCode::Right, KeyModifiers::ALT, "Jump word right"),
            (KeyCode::Up, KeyModifiers::SHIFT, "Scroll up"),
            (KeyCode::Down, KeyModifiers::SHIFT, "Scroll down"),
            (KeyCode::Up, KeyModifiers::NONE, "History up"),
            (KeyCode::Down, KeyModifiers::NONE, "History down"),
            (KeyCode::Left, KeyModifiers::NONE, "Cursor left"),
            (KeyCode::Right, KeyModifiers::NONE, "Cursor right"),
            (KeyCode::Backspace, KeyModifiers::NONE, "Backspace"),
            (KeyCode::Backspace, KeyModifiers::ALT, "Delete word"),
            (KeyCode::Home, KeyModifiers::NONE, "Cursor start"),
            (KeyCode::End, KeyModifiers::NONE, "Cursor end"),
        ];
        for (code, modifiers, desc) in core_keys {
            let combo = KeyCombo { code, modifiers };
            self.reserved.insert(combo.clone());
            self.binds.push(Keybind {
                key: combo,
                action: KeybindAction::Disabled, // core actions handled elsewhere
                description: desc.to_string(),
                source: KeybindSource::Core,
            });
        }
    }

    /// Register keybinds from a plugin manifest.
    pub fn register_plugin(&mut self, plugin_name: &str, keybinds: &[ManifestKeybind], plugin_dir: &std::path::Path) {
        for kb in keybinds {
            let combo = match parse_key(&kb.key) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("plugin '{}': invalid keybind '{}': {}", plugin_name, kb.key, e);
                    continue;
                }
            };

            // Skip if reserved (core)
            if self.reserved.contains(&combo) {
                tracing::warn!("plugin '{}': keybind '{}' conflicts with core — skipped", plugin_name, kb.key);
                continue;
            }

            // Skip if already registered by another plugin
            if self.binds.iter().any(|b| b.key == combo && b.source != KeybindSource::Core) {
                tracing::warn!("plugin '{}': keybind '{}' already registered — skipped", plugin_name, kb.key);
                continue;
            }

            let action = match kb.action.as_str() {
                "slash_command" => {
                    KeybindAction::SlashCommand(kb.command.clone().unwrap_or_default())
                }
                "load_skill" => {
                    KeybindAction::LoadSkill(kb.skill.clone().unwrap_or_default())
                }
                "inject_prompt" => {
                    KeybindAction::InjectPrompt(kb.prompt.clone().unwrap_or_default())
                }
                "run_script" => KeybindAction::RunScript {
                    script: kb.script.clone().unwrap_or_default(),
                    plugin_dir: plugin_dir.to_path_buf(),
                },
                other => {
                    tracing::warn!("plugin '{}': unknown keybind action '{}'", plugin_name, other);
                    continue;
                }
            };

            self.binds.push(Keybind {
                key: combo,
                action,
                description: kb.description.clone().unwrap_or_default(),
                source: KeybindSource::Plugin(plugin_name.to_string()),
            });
        }
    }

    /// Register user keybind overrides from config.
    pub fn register_user(&mut self, config_keybinds: &std::collections::HashMap<String, String>) {
        for (key_str, value) in config_keybinds {
            let combo = match parse_key(key_str) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("config: invalid keybind '{}': {}", key_str, e);
                    continue;
                }
            };

            // Skip core — even users can't override these
            if self.reserved.contains(&combo) {
                tracing::warn!("config: keybind '{}' is a core bind — skipped", key_str);
                continue;
            }

            // Remove any existing plugin bind for this key
            self.binds.retain(|b| b.key != combo || b.source == KeybindSource::Core);

            let action = if value == "disabled" {
                KeybindAction::Disabled
            } else if value.starts_with('/') {
                let cmd = value[1..].to_string();
                KeybindAction::SlashCommand(cmd)
            } else {
                KeybindAction::InjectPrompt(value.clone())
            };

            self.binds.push(Keybind {
                key: combo,
                action,
                description: format!("User: {}", value),
                source: KeybindSource::User,
            });
        }
    }

    /// Live-replace the keybind that fires `slash_command`.
    ///
    /// Removes every existing user/plugin bind whose action is the same
    /// slash command, then registers `new_key → /slash_command` as a User
    /// bind. Used by /settings to hot-swap the voice toggle key without
    /// requiring a restart.
    pub fn set_slash_command_key(&mut self, slash_command: &str, new_key: &str) -> Result<(), String> {
        let combo = parse_key(new_key)?;
        if self.reserved.contains(&combo) {
            return Err(format!("'{}' is reserved by core — cannot rebind", new_key));
        }
        // Drop any existing bind for this exact command (any source ≠ Core).
        self.binds.retain(|b| {
            if b.source == KeybindSource::Core { return true; }
            !matches!(&b.action, KeybindAction::SlashCommand(c) if c == slash_command)
        });
        // Drop any existing non-core bind sitting on the new key (avoid
        // collision with another plugin bind).
        self.binds.retain(|b| b.key != combo || b.source == KeybindSource::Core);
        self.binds.push(Keybind {
            key: combo,
            action: KeybindAction::SlashCommand(slash_command.to_string()),
            description: format!("User: /{}", slash_command),
            source: KeybindSource::User,
        });
        Ok(())
    }

    /// Match a key event against registered keybinds.
    /// Returns None for core binds (handled by the existing match block).
    pub fn match_key(&self, code: KeyCode, modifiers: KeyModifiers) -> Option<&Keybind> {
        let combo = KeyCombo { code, modifiers };

        // Skip core — those are handled by the static match in input.rs
        if self.reserved.contains(&combo) {
            return None;
        }

        self.binds.iter().find(|b| b.key == combo && !matches!(b.source, KeybindSource::Core))
    }

    /// All registered keybinds (for display in /keybinds and settings).
    pub fn all(&self) -> &[Keybind] {
        &self.binds
    }

    /// Non-core keybinds only (plugin + user).
    pub fn custom_binds(&self) -> Vec<&Keybind> {
        self.binds.iter().filter(|b| !matches!(b.source, KeybindSource::Core)).collect()
    }
}

/// Keybind declaration from plugin.json manifest.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ManifestKeybind {
    pub key: String,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub skill: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// Parse key notation string into a KeyCombo.
///
/// Format: `[modifier-]*key`
/// Modifiers: `C` (Ctrl), `S` (Shift), `A` (Alt)
/// Keys: single char, `F1`–`F12`, `Space`, `Tab`, `Enter`, `Esc`
///
/// Examples:
/// - `C-s` → Ctrl+S
/// - `C-S-s` → Ctrl+Shift+S
/// - `A-p` → Alt+P
/// - `F5` → F5
/// - `C-Space` → Ctrl+Space
pub fn parse_key(notation: &str) -> Result<KeyCombo, String> {
    let notation = notation.trim();
    if notation.is_empty() {
        return Err("empty key notation".to_string());
    }

    let parts: Vec<&str> = notation.split('-').collect();
    let mut modifiers = KeyModifiers::empty();

    // All parts except the last are modifiers
    for part in &parts[..parts.len().saturating_sub(1)] {
        match *part {
            "C" => modifiers |= KeyModifiers::CONTROL,
            "S" => modifiers |= KeyModifiers::SHIFT,
            "A" => modifiers |= KeyModifiers::ALT,
            other => return Err(format!("unknown modifier: '{}' (expected C, S, or A)", other)),
        }
    }

    let key_str = parts.last().ok_or("missing key")?;
    let code = match *key_str {
        k if k.len() == 1 => {
            let ch = k.chars().next().unwrap();
            KeyCode::Char(ch.to_ascii_lowercase())
        }
        k if k.starts_with('F') && k.len() <= 3 => {
            let n: u8 = k[1..].parse().map_err(|_| format!("invalid F-key: '{}'", k))?;
            if !(1..=12).contains(&n) {
                return Err(format!("F-key out of range: F{} (expected F1–F12)", n));
            }
            KeyCode::F(n)
        }
        "Space" => KeyCode::Char(' '),
        "Tab" => KeyCode::Tab,
        "Enter" => KeyCode::Enter,
        "Esc" => KeyCode::Esc,
        "Backspace" | "BS" => KeyCode::Backspace,
        "Delete" | "Del" => KeyCode::Delete,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PageUp" | "PgUp" => KeyCode::PageUp,
        "PageDown" | "PgDn" => KeyCode::PageDown,
        "Up" => KeyCode::Up,
        "Down" => KeyCode::Down,
        "Left" => KeyCode::Left,
        "Right" => KeyCode::Right,
        other => return Err(format!("unknown key: '{}'" , other)),
    };

    Ok(KeyCombo { code, modifiers })
}

/// Format a KeyCombo back to notation string (for display).
pub fn format_key(combo: &KeyCombo) -> String {
    let mut parts = Vec::new();
    if combo.modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("Ctrl");
    }
    if combo.modifiers.contains(KeyModifiers::ALT) {
        parts.push("Alt");
    }
    if combo.modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("Shift");
    }

    let key = match combo.code {
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char(c) => c.to_uppercase().to_string(),
        KeyCode::F(n) => format!("F{}", n),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Delete => "Delete".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PageUp".to_string(),
        KeyCode::PageDown => "PageDown".to_string(),
        KeyCode::Up => "↑".to_string(),
        KeyCode::Down => "↓".to_string(),
        KeyCode::Left => "←".to_string(),
        KeyCode::Right => "→".to_string(),
        _ => "?".to_string(),
    };
    parts.push(&key);
    // Need to own the string for the key
    let key_owned = parts.join("+");
    key_owned
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_key tests ──

    #[test]
    fn parse_single_char() {
        let k = parse_key("s").unwrap();
        assert_eq!(k.code, KeyCode::Char('s'));
        assert_eq!(k.modifiers, KeyModifiers::NONE);
    }

    #[test]
    fn parse_ctrl_char() {
        let k = parse_key("C-s").unwrap();
        assert_eq!(k.code, KeyCode::Char('s'));
        assert_eq!(k.modifiers, KeyModifiers::CONTROL);
    }

    #[test]
    fn parse_ctrl_shift() {
        let k = parse_key("C-S-s").unwrap();
        assert_eq!(k.code, KeyCode::Char('s'));
        assert_eq!(k.modifiers, KeyModifiers::CONTROL | KeyModifiers::SHIFT);
    }

    #[test]
    fn parse_alt() {
        let k = parse_key("A-p").unwrap();
        assert_eq!(k.code, KeyCode::Char('p'));
        assert_eq!(k.modifiers, KeyModifiers::ALT);
    }

    #[test]
    fn parse_ctrl_alt() {
        let k = parse_key("C-A-x").unwrap();
        assert_eq!(k.code, KeyCode::Char('x'));
        assert_eq!(k.modifiers, KeyModifiers::CONTROL | KeyModifiers::ALT);
    }

    #[test]
    fn parse_f_keys() {
        let k = parse_key("F5").unwrap();
        assert_eq!(k.code, KeyCode::F(5));
        assert_eq!(k.modifiers, KeyModifiers::NONE);

        let k = parse_key("C-F12").unwrap();
        assert_eq!(k.code, KeyCode::F(12));
        assert_eq!(k.modifiers, KeyModifiers::CONTROL);
    }

    #[test]
    fn parse_special_keys() {
        assert_eq!(parse_key("Space").unwrap().code, KeyCode::Char(' '));
        assert_eq!(parse_key("Tab").unwrap().code, KeyCode::Tab);
        assert_eq!(parse_key("Enter").unwrap().code, KeyCode::Enter);
        assert_eq!(parse_key("Esc").unwrap().code, KeyCode::Esc);
        assert_eq!(parse_key("Backspace").unwrap().code, KeyCode::Backspace);
        assert_eq!(parse_key("Home").unwrap().code, KeyCode::Home);
        assert_eq!(parse_key("End").unwrap().code, KeyCode::End);
    }

    #[test]
    fn parse_ctrl_space() {
        let k = parse_key("C-Space").unwrap();
        assert_eq!(k.code, KeyCode::Char(' '));
        assert_eq!(k.modifiers, KeyModifiers::CONTROL);
    }

    #[test]
    fn parse_uppercase_normalized_to_lower() {
        let k = parse_key("C-S").unwrap();
        assert_eq!(k.code, KeyCode::Char('s'));
    }

    #[test]
    fn parse_empty_errors() {
        assert!(parse_key("").is_err());
        assert!(parse_key("  ").is_err());
    }

    #[test]
    fn parse_unknown_modifier_errors() {
        assert!(parse_key("X-s").is_err());
    }

    #[test]
    fn parse_unknown_key_errors() {
        assert!(parse_key("C-FooBar").is_err());
    }

    #[test]
    fn parse_f_key_out_of_range() {
        assert!(parse_key("F0").is_err());
        assert!(parse_key("F13").is_err());
    }

    // ── format_key tests ──

    #[test]
    fn format_ctrl_shift_s() {
        let k = KeyCombo {
            code: KeyCode::Char('s'),
            modifiers: KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        };
        assert_eq!(format_key(&k), "Ctrl+Shift+S");
    }

    #[test]
    fn format_f5() {
        let k = KeyCombo {
            code: KeyCode::F(5),
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(format_key(&k), "F5");
    }

    #[test]
    fn format_alt_space() {
        let k = KeyCombo {
            code: KeyCode::Char(' '),
            modifiers: KeyModifiers::ALT,
        };
        assert_eq!(format_key(&k), "Alt+Space");
    }

    // ── registry tests ──

    #[test]
    fn core_binds_are_reserved() {
        let reg = KeybindRegistry::new();
        // Ctrl+C should not match (it's core)
        assert!(reg.match_key(KeyCode::Char('c'), KeyModifiers::CONTROL).is_none());
    }

    #[test]
    fn plugin_bind_matches() {
        let mut reg = KeybindRegistry::new();
        reg.register_plugin("test", &[ManifestKeybind {
            key: "C-S-s".to_string(),
            action: "slash_command".to_string(),
            command: Some("scholar".to_string()),
            skill: None, prompt: None, script: None,
            description: Some("Search papers".to_string()),
        }], std::path::Path::new("/tmp"));

        let result = reg.match_key(KeyCode::Char('s'), KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        assert!(result.is_some());
        assert_eq!(result.unwrap().description, "Search papers");
    }

    #[test]
    fn plugin_cannot_override_core() {
        let mut reg = KeybindRegistry::new();
        reg.register_plugin("evil", &[ManifestKeybind {
            key: "C-c".to_string(),
            action: "inject_prompt".to_string(),
            command: None, skill: None,
            prompt: Some("hacked".to_string()),
            script: None,
            description: Some("evil".to_string()),
        }], std::path::Path::new("/tmp"));

        // Ctrl+C should still not match (core)
        assert!(reg.match_key(KeyCode::Char('c'), KeyModifiers::CONTROL).is_none());
    }

    #[test]
    fn user_overrides_plugin() {
        let mut reg = KeybindRegistry::new();
        reg.register_plugin("test", &[ManifestKeybind {
            key: "F5".to_string(),
            action: "slash_command".to_string(),
            command: Some("scholar".to_string()),
            skill: None, prompt: None, script: None,
            description: Some("Scholar".to_string()),
        }], std::path::Path::new("/tmp"));

        let mut overrides = std::collections::HashMap::new();
        overrides.insert("F5".to_string(), "/compact".to_string());
        reg.register_user(&overrides);

        let result = reg.match_key(KeyCode::F(5), KeyModifiers::NONE);
        assert!(result.is_some());
        assert_eq!(result.unwrap().description, "User: /compact");
    }

    #[test]
    fn user_can_disable_bind() {
        let mut reg = KeybindRegistry::new();
        reg.register_plugin("test", &[ManifestKeybind {
            key: "F5".to_string(),
            action: "slash_command".to_string(),
            command: Some("scholar".to_string()),
            skill: None, prompt: None, script: None,
            description: Some("Scholar".to_string()),
        }], std::path::Path::new("/tmp"));

        let mut overrides = std::collections::HashMap::new();
        overrides.insert("F5".to_string(), "disabled".to_string());
        reg.register_user(&overrides);

        let result = reg.match_key(KeyCode::F(5), KeyModifiers::NONE);
        assert!(result.is_some());
        assert!(matches!(result.unwrap().action, KeybindAction::Disabled));
    }

    #[test]
    fn duplicate_plugin_binds_first_wins() {
        let mut reg = KeybindRegistry::new();
        reg.register_plugin("first", &[ManifestKeybind {
            key: "F5".to_string(),
            action: "slash_command".to_string(),
            command: Some("first".to_string()),
            skill: None, prompt: None, script: None,
            description: Some("First".to_string()),
        }], std::path::Path::new("/tmp"));

        reg.register_plugin("second", &[ManifestKeybind {
            key: "F5".to_string(),
            action: "slash_command".to_string(),
            command: Some("second".to_string()),
            skill: None, prompt: None, script: None,
            description: Some("Second".to_string()),
        }], std::path::Path::new("/tmp"));

        let result = reg.match_key(KeyCode::F(5), KeyModifiers::NONE);
        assert!(result.is_some());
        assert_eq!(result.unwrap().description, "First");
    }

    #[test]
    fn custom_binds_excludes_core() {
        let reg = KeybindRegistry::new();
        let custom = reg.custom_binds();
        assert!(custom.is_empty()); // No plugins registered = no custom binds
    }

    #[test]
    fn set_slash_command_key_replaces_existing_voice_toggle() {
        let mut reg = KeybindRegistry::new();
        let mut overrides = std::collections::HashMap::new();
        overrides.insert("F8".to_string(), "/voice toggle".to_string());
        reg.register_user(&overrides);
        let f8 = parse_key("F8").unwrap();
        assert!(reg.match_key(f8.code, f8.modifiers).is_some());

        // Move voice toggle from F8 → C-G
        reg.set_slash_command_key("voice toggle", "C-G").unwrap();

        // F8 no longer fires
        assert!(reg.match_key(f8.code, f8.modifiers).is_none());
        // C-G now does
        let cg = parse_key("C-G").unwrap();
        let bind = reg.match_key(cg.code, cg.modifiers).expect("C-G bind missing");
        assert!(matches!(&bind.action, KeybindAction::SlashCommand(c) if c == "voice toggle"));
    }

    #[test]
    fn set_slash_command_key_rejects_core_chord() {
        let mut reg = KeybindRegistry::new();
        // Esc is reserved core
        let err = reg.set_slash_command_key("voice toggle", "Esc").unwrap_err();
        assert!(err.contains("reserved"), "expected reserved error, got: {err}");
    }
}
