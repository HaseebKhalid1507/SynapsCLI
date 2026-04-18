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
            "model", "thinking", "skills",
            "max_tool_output", "bash_timeout", "bash_max_timeout",
            "subagent_timeout", "api_retries", "theme",
        ];
        for def in ALL_SETTINGS {
            assert!(valid.contains(&def.key), "unknown setting key: {}", def.key);
        }
    }

    #[test]
    fn every_setting_belongs_to_known_category() {
        for def in ALL_SETTINGS {
            assert!(CATEGORIES.contains(&def.category));
        }
    }
}
