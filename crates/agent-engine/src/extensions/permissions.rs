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
    /// Can rewrite tool OUTPUT via `after_tool_call` → `HookResult::Replace`.
    /// Distinct from (and additional to) `tools.intercept`: observing a tool's
    /// output is a weaker capability than silently rewriting what the model
    /// believes the tool returned, so it requires explicit, separate consent.
    ToolsTransformOutput,
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
    /// Can declare dormant memory/context contribution providers
    /// (continuous-memory spec §7.1). Distinct from `ProvidersRegister`
    /// (model/LLM providers): this gates only passive context-provider
    /// capability metadata; activation additionally requires an exact
    /// memory-context lease (task A6).
    ContextProvidersRegister,
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
    /// Every permission variant, in declaration order.
    ///
    /// Exhaustiveness is enforced by `all_covers_every_variant` below, which
    /// pattern-matches every variant explicitly: adding one to the enum
    /// breaks that test's match at compile time, and its length assertion
    /// then forces this list to be updated too.
    ///
    /// This list is what `docs/extensions/contract.json` is drift-checked
    /// against (see `crates/agent-tui/tests/extensions_contract.rs`). Before
    /// that check existed the contract had fallen eight permissions behind
    /// the engine while `STABILITY.md` advertised it as CI-verified.
    pub const ALL: &'static [Permission] = &[
        Self::ToolsIntercept,
        Self::ToolsTransformOutput,
        Self::ToolsOverride,
        Self::LlmContent,
        Self::SessionLifecycle,
        Self::ToolsRegister,
        Self::ProvidersRegister,
        Self::ContextProvidersRegister,
        Self::MemoryRead,
        Self::MemoryWrite,
        Self::ConfigWrite,
        Self::ConfigSubscribe,
        Self::AudioInput,
        Self::AudioOutput,
    ];

    /// Wire-format string for this permission.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ToolsIntercept => "tools.intercept",
            Self::ToolsTransformOutput => "tools.transform_output",
            Self::ToolsOverride => "tools.override",
            Self::LlmContent => "privacy.llm_content",
            Self::SessionLifecycle => "session.lifecycle",
            Self::ToolsRegister => "tools.register",
            Self::ProvidersRegister => "providers.register",
            Self::ContextProvidersRegister => "context_providers.register",
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
            "tools.transform_output" => Some(Self::ToolsTransformOutput),
            "tools.override" => Some(Self::ToolsOverride),
            "privacy.llm_content" => Some(Self::LlmContent),
            "session.lifecycle" => Some(Self::SessionLifecycle),
            "tools.register" => Some(Self::ToolsRegister),
            "providers.register" => Some(Self::ProvidersRegister),
            "context_providers.register" => Some(Self::ContextProvidersRegister),
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
        matches!(self, Self::ToolsOverride)
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
        assert_eq!(
            Permission::parse("tools.intercept"),
            Some(Permission::ToolsIntercept)
        );
        assert_eq!(
            Permission::parse("privacy.llm_content"),
            Some(Permission::LlmContent)
        );
        assert_eq!(
            Permission::parse("session.lifecycle"),
            Some(Permission::SessionLifecycle)
        );
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
        assert_eq!(
            Permission::parse("memory.read"),
            Some(Permission::MemoryRead)
        );
        assert_eq!(
            Permission::parse("memory.write"),
            Some(Permission::MemoryWrite)
        );
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
        assert_eq!(
            Permission::parse("audio.input"),
            Some(Permission::AudioInput)
        );
        assert_eq!(
            Permission::parse("audio.output"),
            Some(Permission::AudioOutput)
        );
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
    fn context_providers_register_parses_and_is_not_reserved() {
        assert_eq!(
            Permission::parse("context_providers.register"),
            Some(Permission::ContextProvidersRegister)
        );
        assert!(!Permission::ContextProvidersRegister.is_reserved());
        let perms =
            PermissionSet::try_from_strings(&["context_providers.register".to_string()]).unwrap();
        assert!(perms.has(Permission::ContextProvidersRegister));
        // Distinct from the model/LLM provider permission.
        assert!(!perms.has(Permission::ProvidersRegister));
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
            Permission::ContextProvidersRegister,
            Permission::MemoryRead,
            Permission::MemoryWrite,
            Permission::AudioInput,
            Permission::AudioOutput,
        ] {
            assert_eq!(Permission::parse(perm.as_str()), Some(perm));
        }
    }

    /// `Permission::ALL` must list every variant exactly once.
    ///
    /// The match below is deliberately written out variant by variant: adding
    /// a variant to the enum makes it non-exhaustive and this test stops
    /// COMPILING, which is the point. The author is then forced to the length
    /// assertion, which sends them to `ALL`. Without this, `ALL` is just a
    /// hand-maintained list that silently falls behind -- the exact failure
    /// mode that let contract.json drift eight permissions out of date.
    #[test]
    fn all_covers_every_variant() {
        for perm in Permission::ALL {
            match perm {
                Permission::ToolsIntercept
                | Permission::ToolsTransformOutput
                | Permission::ToolsOverride
                | Permission::LlmContent
                | Permission::SessionLifecycle
                | Permission::ToolsRegister
                | Permission::ProvidersRegister
                | Permission::ContextProvidersRegister
                | Permission::MemoryRead
                | Permission::MemoryWrite
                | Permission::ConfigWrite
                | Permission::ConfigSubscribe
                | Permission::AudioInput
                | Permission::AudioOutput => {}
            }
        }
        assert_eq!(
            Permission::ALL.len(),
            14,
            "a Permission variant was added or removed -- update Permission::ALL \
             and docs/extensions/contract.json to match"
        );
        let unique: HashSet<Permission> = Permission::ALL.iter().copied().collect();
        assert_eq!(
            unique.len(),
            Permission::ALL.len(),
            "Permission::ALL contains a duplicate"
        );
    }

    /// Every listed permission must round-trip through the wire format.
    #[test]
    fn all_permissions_round_trip_through_wire_format() {
        for perm in Permission::ALL {
            assert_eq!(
                Permission::parse(perm.as_str()),
                Some(*perm),
                "{} does not round-trip",
                perm.as_str()
            );
        }
    }
}
