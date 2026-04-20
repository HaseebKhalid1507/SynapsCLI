//! Static list of settings exposed in the /settings menu.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Category {
    Model,
    Agent,
    ToolLimits,
    Appearance,
    Plugins,
}

impl Category {
    pub fn label(&self) -> &'static str {
        match self {
            Category::Model => "Model",
            Category::Agent => "Agent",
            Category::ToolLimits => "Tool Limits",
            Category::Appearance => "Appearance",
            Category::Plugins => "Plugins",
        }
    }
}

pub(crate) const CATEGORIES: [Category; 5] = [
    Category::Model,
    Category::Agent,
    Category::ToolLimits,
    Category::Appearance,
    Category::Plugins,
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
    // Reserved for settings UI tooltip wiring (TODO).
    #[allow(dead_code)]
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
        editor: EditorKind::Cycler(&["low", "medium", "high", "xhigh", "adaptive"]),
        help: "Thinking depth — controls effort on adaptive models, budget on legacy.",
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
        help: "Seconds before a dispatched subagent is cancelled.",
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

    /// Parity check — every setting key must be one that `load_config()` recognizes.
    #[test]
    fn every_setting_key_is_known_to_load_config() {
        let valid = [
            "model", "thinking",
            "max_tool_output", "bash_timeout", "bash_max_timeout",
            "subagent_timeout", "api_retries", "theme",
        ];
        for def in ALL_SETTINGS {
            assert!(valid.contains(&def.key), "unknown setting key: {}", def.key);
        }
    }

    /// Parity check — every setting key has a handler in apply_setting().
    #[test]
    fn every_setting_key_handled_by_apply_setting() {
        let handled = super::super::super::APPLY_SETTING_KEYS;
        for def in ALL_SETTINGS {
            assert!(
                handled.contains(&def.key),
                "setting '{}' is in ALL_SETTINGS but not in APPLY_SETTING_KEYS — add it to apply_setting()",
                def.key
            );
        }
    }

    /// Reverse check — every key in APPLY_SETTING_KEYS exists in ALL_SETTINGS.
    #[test]
    fn apply_setting_keys_all_have_schema_entry() {
        let schema_keys: Vec<&str> = ALL_SETTINGS.iter().map(|d| d.key).collect();
        for key in super::super::super::APPLY_SETTING_KEYS {
            // "skills" is handled by apply_setting but not in schema (internal)
            if *key == "skills" { continue; }
            assert!(
                schema_keys.contains(key),
                "APPLY_SETTING_KEYS has '{}' but ALL_SETTINGS doesn't — remove it or add a schema entry",
                key
            );
        }
    }

    #[test]
    fn every_setting_belongs_to_known_category() {
        for def in ALL_SETTINGS {
            assert!(CATEGORIES.contains(&def.category));
        }
    }

    #[test]
    fn plugins_category_is_present() {
        assert!(CATEGORIES.contains(&Category::Plugins));
    }

    #[test]
    fn plugins_category_label() {
        assert_eq!(Category::Plugins.label(), "Plugins");
    }
}
