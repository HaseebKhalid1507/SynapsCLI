//! Permission model for extensions.
//!
//! Permissions are declared in the plugin manifest and enforced before
//! delivering hook events. An extension without the required permission
//! cannot subscribe to the corresponding hook.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// Permission flags an extension can request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Can subscribe to before_tool_call / after_tool_call hooks.
    ToolsIntercept,
    /// Can override built-in tools.
    ToolsOverride,
    /// Can read LLM input/output (before_message hook).
    LlmContent,
    /// Can subscribe to session lifecycle hooks.
    SessionLifecycle,
    /// Can register new tools.
    ToolsRegister,
    /// Can register new providers.
    ProvidersRegister,
    /// Can read from the local memory store via `memory.query`.
    MemoryRead,
    /// Can append to the local memory store via `memory.append`.
    MemoryWrite,
    /// Can read/write its own plugin-namespaced config via `config.get`/`config.set`.
    ConfigWrite,
    /// Can subscribe to hot-reload notifications for its own plugin config.
    ConfigSubscribe,
    /// Can capture audio from input devices.
    AudioInput,
    /// Can produce audio through output devices.
    AudioOutput,
}

impl Permission {
    /// Wire-format string for this permission.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ToolsIntercept => "tools.intercept",
            Self::ToolsOverride => "tools.override",
            Self::LlmContent => "privacy.llm_content",
            Self::SessionLifecycle => "session.lifecycle",
            Self::ToolsRegister => "tools.register",
            Self::ProvidersRegister => "providers.register",
            Self::MemoryRead => "memory.read",
            Self::MemoryWrite => "memory.write",
            Self::ConfigWrite => "config.write",
            Self::ConfigSubscribe => "config.subscribe",
            Self::AudioInput => "audio.input",
            Self::AudioOutput => "audio.output",
        }
    }

    /// Parse from wire-format string.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "tools.intercept" => Some(Self::ToolsIntercept),
            "tools.override" => Some(Self::ToolsOverride),
            "privacy.llm_content" => Some(Self::LlmContent),
            "session.lifecycle" => Some(Self::SessionLifecycle),
            "tools.register" => Some(Self::ToolsRegister),
            "providers.register" => Some(Self::ProvidersRegister),
            "memory.read" => Some(Self::MemoryRead),
            "memory.write" => Some(Self::MemoryWrite),
            "config.write" => Some(Self::ConfigWrite),
            "config.subscribe" => Some(Self::ConfigSubscribe),
            "audio.input" => Some(Self::AudioInput),
            "audio.output" => Some(Self::AudioOutput),
            _ => None,
        }
    }
    /// Whether this permission is reserved for a future implementation.
    pub fn is_reserved(&self) -> bool {
        matches!(
            self,
            Self::ToolsOverride
        )
    }
}

/// A set of permissions granted to an extension.
#[derive(Debug, Clone, Default)]
pub struct PermissionSet {
    permissions: HashSet<Permission>,
}

impl PermissionSet {
    /// Empty permission set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse permission strings (from manifest) into a set.
    ///
    /// This lenient parser is kept for tests and internal callers that have
    /// already validated manifests. Extension manifests should use
    /// [`try_from_strings`](Self::try_from_strings) so typos fail loudly.
    pub fn from_strings(perms: &[String]) -> Self {
        let permissions = perms.iter().filter_map(|s| Permission::parse(s)).collect();
        Self { permissions }
    }

    /// Parse permission strings and reject unknown values.
    pub fn try_from_strings(perms: &[String]) -> Result<Self, String> {
        let mut permissions = HashSet::new();
        for perm in perms {
            let parsed = Permission::parse(perm)
                .ok_or_else(|| format!("Unknown extension permission: {perm}"))?;
            if parsed.is_reserved() {
                return Err(format!(
                    "Reserved extension permission is not implemented yet: {perm}"
                ));
            }
            permissions.insert(parsed);
        }
        Ok(Self { permissions })
    }

    /// Check if a permission is granted.
    pub fn has(&self, perm: Permission) -> bool {
        self.permissions.contains(&perm)
    }

    /// Grant a permission.
    pub fn grant(&mut self, perm: Permission) {
        self.permissions.insert(perm);
    }

    /// Check if this set allows subscribing to the given hook.
    pub fn allows_hook(&self, kind: crate::extensions::hooks::events::HookKind) -> bool {
        self.has(kind.required_permission())
    }

    /// Number of permissions.
    pub fn len(&self) -> usize {
        self.permissions.len()
    }

    /// Whether no permissions are granted.
    pub fn is_empty(&self) -> bool {
        self.permissions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::hooks::events::HookKind;

    #[test]
    fn parse_valid_permissions() {
        assert_eq!(Permission::parse("tools.intercept"), Some(Permission::ToolsIntercept));
        assert_eq!(Permission::parse("privacy.llm_content"), Some(Permission::LlmContent));
        assert_eq!(Permission::parse("session.lifecycle"), Some(Permission::SessionLifecycle));
    }

    #[test]
    fn parse_invalid_returns_none() {
        assert_eq!(Permission::parse("invalid"), None);
        assert_eq!(Permission::parse(""), None);
    }

    #[test]
    fn from_strings_skips_invalid() {
        let perms = PermissionSet::from_strings(&[
            "tools.intercept".into(),
            "bogus".into(),
            "session.lifecycle".into(),
        ]);
        assert_eq!(perms.len(), 2);
        assert!(perms.has(Permission::ToolsIntercept));
        assert!(perms.has(Permission::SessionLifecycle));
        assert!(!perms.has(Permission::LlmContent));
    }

    #[test]
    fn allows_hook_checks_required_permission() {
        let mut perms = PermissionSet::new();
        assert!(!perms.allows_hook(HookKind::BeforeToolCall));

        perms.grant(Permission::ToolsIntercept);
        assert!(perms.allows_hook(HookKind::BeforeToolCall));
        assert!(perms.allows_hook(HookKind::AfterToolCall));
        assert!(!perms.allows_hook(HookKind::BeforeMessage)); // needs LlmContent
    }

    #[test]
    fn empty_set() {
        let perms = PermissionSet::new();
        assert!(perms.is_empty());
        assert_eq!(perms.len(), 0);
    }

    #[test]
    fn providers_register_is_active_but_tools_override_remains_reserved() {
        let perms = PermissionSet::try_from_strings(&["providers.register".to_string()]).unwrap();
        assert!(perms.has(Permission::ProvidersRegister));

        let err = PermissionSet::try_from_strings(&["tools.override".to_string()]).unwrap_err();
        assert!(err.contains("Reserved extension permission"));
    }

    #[test]
    fn memory_permissions_parse_and_are_not_reserved() {
        assert_eq!(Permission::parse("memory.read"), Some(Permission::MemoryRead));
        assert_eq!(Permission::parse("memory.write"), Some(Permission::MemoryWrite));
        assert!(!Permission::MemoryRead.is_reserved());
        assert!(!Permission::MemoryWrite.is_reserved());
        let perms = PermissionSet::try_from_strings(&[
            "memory.read".to_string(),
            "memory.write".to_string(),
        ])
        .unwrap();
        assert!(perms.has(Permission::MemoryRead));
        assert!(perms.has(Permission::MemoryWrite));
    }

    #[test]
    fn audio_permissions_parse_and_are_not_reserved() {
        assert_eq!(Permission::parse("audio.input"), Some(Permission::AudioInput));
        assert_eq!(Permission::parse("audio.output"), Some(Permission::AudioOutput));
        assert!(!Permission::AudioInput.is_reserved());
        assert!(!Permission::AudioOutput.is_reserved());
        let perms = PermissionSet::try_from_strings(&[
            "audio.input".to_string(),
            "audio.output".to_string(),
        ])
        .unwrap();
        assert!(perms.has(Permission::AudioInput));
        assert!(perms.has(Permission::AudioOutput));
    }

    #[test]
    fn round_trip_as_str() {
        for perm in [
            Permission::ToolsIntercept,
            Permission::ToolsOverride,
            Permission::LlmContent,
            Permission::SessionLifecycle,
            Permission::ToolsRegister,
            Permission::ProvidersRegister,
            Permission::MemoryRead,
            Permission::MemoryWrite,
            Permission::AudioInput,
            Permission::AudioOutput,
        ] {
            assert_eq!(Permission::parse(perm.as_str()), Some(perm));
        }
    }
}
