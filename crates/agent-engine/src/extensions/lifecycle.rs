//! Task 20 — capability-driven extension lifecycle (spec §7.5).
//!
//! Typed inventory/classification of extension manifests plus OPTIONAL,
//! ADDITIVE passive `deferred` declarations. Absent declarations keep the
//! documented legacy EAGER lifecycle byte-for-byte (no protocol version
//! bump); present declarations are trusted local manifest expectations:
//! they enable spawn deferral and MUST match the runtime's initialize
//! declarations exactly (names, schemas, digests) before any registration
//! or grant — a mismatch shuts the child down and fails closed.
//!
//! Nothing in this module spawns a process or touches the network: it
//! reads bounded local manifest data and mints dormant descriptor tools.

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::Value;

use super::manifest::ExtensionManifest;
use super::runtime::process::RegisteredExtensionToolSpec;
use crate::tools::catalog::SchemaDigest;
use crate::tools::{Tool, ToolContext, ToolOrigin};
use agent_core::BoundedText;

/// Bounds for passive tool declarations (parity with the MCP descriptor
/// cache budgets).
pub const DECLARED_MAX_TOOLS: usize = 256;
pub const DECLARED_MAX_PROVIDERS: usize = 16;
pub const DECLARED_NAME_MAX_BYTES: usize = 128;
pub const DECLARED_DESCRIPTION_MAX_BYTES: usize = 1024;
pub const DECLARED_SCHEMA_MAX_BYTES: usize = 64 * 1024;

/// OPTIONAL additive manifest block. Absent => legacy eager lifecycle.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeferredDeclarations {
    /// EXACT tools the extension will register at initialize.
    #[serde(default)]
    pub tools: Vec<DeclaredExtensionTool>,
    /// TYPED `RegisteredProviderSpec`-compatible provider metadata,
    /// deeply bounded/validated here so malformed passive metadata can
    /// never reach provider routing later.
    #[serde(default)]
    pub providers: Vec<DeclaredExtensionProvider>,
    /// Optional lifecycle hint for extensions without tools/providers.
    #[serde(default)]
    pub lifecycle: Option<DeferredLifecycle>,
}

/// Passive provider declaration, field-compatible with the runtime's
/// `RegisteredProviderSpec` so a validated declaration can later back
/// metadata-only registration.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeclaredExtensionProvider {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub models: Vec<DeclaredExtensionProviderModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<Value>,
}

/// Field-compatible with `RegisteredProviderModelSpec`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeclaredExtensionProviderModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default)]
    pub capabilities: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
}

/// Bounds for provider declarations.
pub const DECLARED_MAX_PROVIDER_MODELS: usize = 64;
pub const DECLARED_DISPLAY_MAX_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeferredLifecycle {
    /// Start only on the first authorized subscribed hook event.
    Hook,
    /// Start only on explicit user action (UI/sidecar class).
    User,
}

/// One passively declared tool: exactly the fields the runtime will
/// register at initialize, pinned locally so activation can validate the
/// live declaration against operator-known expectations.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeclaredExtensionTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub input_schema: Value,
}

/// Typed extension lifecycle classes (spec §7.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionClass {
    /// No usable passive declarations: documented eager compatibility.
    LegacyEager,
    ToolOnly,
    Provider,
    HookLifecycle,
    UiSidecar,
    Mixed,
}

/// Classify a manifest from its passive declarations. Pure and local.
pub fn classify(manifest: &ExtensionManifest) -> ExtensionClass {
    let Some(deferred) = &manifest.deferred else {
        return ExtensionClass::LegacyEager;
    };
    let tools = !deferred.tools.is_empty();
    let providers = !deferred.providers.is_empty();
    let hooks = !manifest.hooks.is_empty() || deferred.lifecycle == Some(DeferredLifecycle::Hook);
    let user = deferred.lifecycle == Some(DeferredLifecycle::User);
    match (tools, providers, hooks, user) {
        (false, false, false, false) => ExtensionClass::LegacyEager,
        (true, false, false, false) => ExtensionClass::ToolOnly,
        (false, true, false, false) => ExtensionClass::Provider,
        (false, false, true, false) => ExtensionClass::HookLifecycle,
        (false, false, false, true) => ExtensionClass::UiSidecar,
        _ => ExtensionClass::Mixed,
    }
}

/// The earliest legitimately required activation trigger for a manifest
/// (spec 7.5). Mixed extensions use the EARLIEST of their components —
/// hook events fire passively during any turn, provider selection is an
/// explicit routing choice, exact tool activation comes last; tool SEARCH
/// alone never triggers a start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationTrigger {
    Eager,
    FirstAuthorizedHookEvent,
    ProviderSelection,
    ExactToolActivation,
    UserAction,
}

pub fn earliest_trigger(manifest: &ExtensionManifest) -> ActivationTrigger {
    match classify(manifest) {
        ExtensionClass::LegacyEager => ActivationTrigger::Eager,
        ExtensionClass::ToolOnly => ActivationTrigger::ExactToolActivation,
        ExtensionClass::Provider => ActivationTrigger::ProviderSelection,
        ExtensionClass::HookLifecycle => ActivationTrigger::FirstAuthorizedHookEvent,
        ExtensionClass::UiSidecar => ActivationTrigger::UserAction,
        ExtensionClass::Mixed => {
            let deferred = manifest.deferred.as_ref();
            let hooks = !manifest.hooks.is_empty()
                || deferred.and_then(|d| d.lifecycle) == Some(DeferredLifecycle::Hook);
            let providers = deferred.map(|d| !d.providers.is_empty()).unwrap_or(false);
            if hooks {
                ActivationTrigger::FirstAuthorizedHookEvent
            } else if providers {
                ActivationTrigger::ProviderSelection
            } else {
                ActivationTrigger::ExactToolActivation
            }
        }
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= DECLARED_NAME_MAX_BYTES && !name.chars().any(char::is_control)
}

/// Bounded validation of a `deferred` block. Static reasons only; any
/// violation fails the WHOLE manifest closed (validated before any spawn).
pub fn validate_deferred(deferred: &DeferredDeclarations) -> Result<(), &'static str> {
    if deferred.tools.len() > DECLARED_MAX_TOOLS {
        return Err("too_many_declared_tools");
    }
    let mut seen: HashSet<&str> = HashSet::new();
    for tool in &deferred.tools {
        validate_tool_shape(&tool.name, &tool.description, &tool.input_schema)?;
        if !seen.insert(tool.name.as_str()) {
            return Err("duplicate_declared_tool_name");
        }
    }
    if deferred.providers.len() > DECLARED_MAX_PROVIDERS {
        return Err("too_many_declared_providers");
    }
    let mut provider_ids: HashSet<&str> = HashSet::new();
    for provider in &deferred.providers {
        if !valid_name(&provider.id) || provider.id.contains(':') {
            return Err("invalid_declared_provider_id");
        }
        if !provider_ids.insert(provider.id.as_str()) {
            return Err("duplicate_declared_provider_id");
        }
        if provider.display_name.is_empty()
            || provider.display_name.len() > DECLARED_DISPLAY_MAX_BYTES
            || provider.display_name.chars().any(char::is_control)
        {
            return Err("invalid_declared_provider_display_name");
        }
        if provider.description.is_empty()
            || provider.description.len() > DECLARED_DESCRIPTION_MAX_BYTES
        {
            return Err("invalid_declared_provider_description");
        }
        if provider.models.is_empty() || provider.models.len() > DECLARED_MAX_PROVIDER_MODELS {
            return Err("invalid_declared_provider_model_count");
        }
        let mut model_ids: HashSet<&str> = HashSet::new();
        for model in &provider.models {
            if !valid_name(&model.id) {
                return Err("invalid_declared_provider_model_id");
            }
            if !model_ids.insert(model.id.as_str()) {
                return Err("duplicate_declared_provider_model_id");
            }
            if let Some(display) = &model.display_name {
                if display.len() > DECLARED_DISPLAY_MAX_BYTES
                    || display.chars().any(char::is_control)
                {
                    return Err("invalid_declared_provider_model_display_name");
                }
            }
            // Capabilities: absent (null) or an object of boolean flags.
            match &model.capabilities {
                Value::Null => {}
                Value::Object(map) => {
                    for (key, value) in map {
                        if !valid_name(key) || !value.is_boolean() {
                            return Err("invalid_declared_provider_model_capabilities");
                        }
                    }
                }
                _ => return Err("invalid_declared_provider_model_capabilities"),
            }
        }
        if let Some(config_schema) = &provider.config_schema {
            let len = serde_json::to_vec(config_schema)
                .map(|b| b.len())
                .unwrap_or(usize::MAX);
            if !config_schema.is_object() || len > DECLARED_SCHEMA_MAX_BYTES {
                return Err("invalid_declared_provider_config_schema");
            }
        }
    }
    // Lifecycle conflict policy: `user` means user-triggered ONLY — it must
    // never silently cover active tool/provider capabilities.
    if deferred.lifecycle == Some(DeferredLifecycle::User)
        && (!deferred.tools.is_empty() || !deferred.providers.is_empty())
    {
        return Err("user_lifecycle_conflicts_with_active_capabilities");
    }
    Ok(())
}

/// Manifest-level coupling checks for a `deferred` block (review fix A1/
/// A2). Passive declarations are CAPABILITY claims: cataloging a dormant
/// tool descriptor (or provider metadata) grants the extension future
/// runtime reach, so it MUST be backed by the exact permission the live
/// registration would require — otherwise a manifest could obtain
/// discoverable/activatable surface it was never authorized to register.
/// Likewise `lifecycle = "hook"` without a real manifest hook subscription
/// has NO authorized trigger and can never legitimately start: it fails
/// closed here instead of lingering as unreachable-but-cataloged state.
///
/// Runs inside `ExtensionManifest::validate` — i.e. strictly BEFORE any
/// spawn or catalog registration on every load path.
pub fn validate_manifest_deferred(manifest: &ExtensionManifest) -> Result<(), &'static str> {
    let Some(deferred) = &manifest.deferred else {
        return Ok(());
    };
    validate_deferred(deferred)?;
    let has_permission = |wire: &str| manifest.permissions.iter().any(|p| p == wire);
    if !deferred.tools.is_empty() && !has_permission("tools.register") {
        return Err("deferred_tools_require_tools_register_permission");
    }
    if !deferred.providers.is_empty() && !has_permission("providers.register") {
        return Err("deferred_providers_require_providers_register_permission");
    }
    if deferred.lifecycle == Some(DeferredLifecycle::Hook) && manifest.hooks.is_empty() {
        return Err("hook_lifecycle_requires_hook_subscriptions");
    }
    Ok(())
}

/// Shared bounded shape check for a tool identity (declared OR runtime
/// registered). Declared tool names additionally must not contain ':'
/// so the `<plugin>:<tool>` runtime name stays reverse-parseable.
fn validate_tool_shape(
    name: &str,
    description: &str,
    input_schema: &Value,
) -> Result<(), &'static str> {
    if !valid_name(name) || name.contains(':') {
        return Err("invalid_declared_tool_name");
    }
    // Non-empty required: the runtime's initialize validation refuses
    // empty descriptions, so an empty declaration could never match a
    // live registration — fail closed at manifest time instead.
    if description.trim().is_empty() || description.len() > DECLARED_DESCRIPTION_MAX_BYTES {
        return Err("invalid_declared_tool_description");
    }
    if !input_schema.is_object() {
        return Err("declared_tool_schema_not_object");
    }
    let schema_len = serde_json::to_vec(input_schema)
        .map(|b| b.len())
        .unwrap_or(usize::MAX);
    if schema_len > DECLARED_SCHEMA_MAX_BYTES {
        return Err("oversized_declared_tool_schema");
    }
    Ok(())
}

/// Strict initialize-time declaration check: the runtime's registered tool
/// specs must match the manifest's passive declarations EXACTLY — same
/// name set (no missing, no undeclared extras) and identical canonical
/// schema digests. Static reasons only; callers shut the child down and
/// fail closed on any mismatch, before any registration or grant.
pub fn validate_runtime_tool_declarations(
    declared: &[DeclaredExtensionTool],
    registered: &[RegisteredExtensionToolSpec],
) -> Result<(), &'static str> {
    if declared.len() != registered.len() {
        return Err("declared_and_registered_tool_counts_differ");
    }
    for spec in registered {
        // The runtime's own declaration must pass the same bounded shape
        // policy BEFORE any comparison is trusted.
        validate_tool_shape(&spec.name, &spec.description, &spec.input_schema)?;
    }
    for decl in declared {
        let Some(live) = registered.iter().find(|spec| spec.name == decl.name) else {
            return Err("declared_tool_not_registered");
        };
        if SchemaDigest::of_schema(&live.input_schema)
            != SchemaDigest::of_schema(&decl.input_schema)
        {
            return Err("registered_tool_schema_digest_mismatch");
        }
        // Full-match policy: descriptions are part of the declared
        // contract (model-visible prompt surface), so they must be equal.
        if live.description != decl.description {
            return Err("registered_tool_description_mismatch");
        }
    }
    // Equal counts + every declared found => no undeclared extras (names
    // are unique per validate_deferred; duplicates in `registered` would
    // fail the count/find pass above).
    let mut live_names: HashSet<&str> = HashSet::new();
    for spec in registered {
        if !live_names.insert(spec.name.as_str()) {
            return Err("registered_tool_names_duplicate");
        }
    }
    Ok(())
}

/// Strict initialize-time PROVIDER declaration check (Task 20 Commit B):
/// the runtime's registered provider specs must match the manifest's
/// passive declarations EXACTLY — same id set, display names,
/// descriptions, model id sets, per-model display/context-window/
/// capability values, and config schemas as declared. A manifest that
/// declares NO providers must see NO registered providers (a tool-only
/// runtime sneaking provider metadata in is an undeclared capability).
/// Static reasons only; callers shut the child down and fail closed.
pub fn validate_runtime_provider_declarations(
    declared: &[DeclaredExtensionProvider],
    registered: &[crate::extensions::runtime::process::RegisteredProviderSpec],
) -> Result<(), &'static str> {
    if declared.len() != registered.len() {
        return Err("declared_and_registered_provider_counts_differ");
    }
    let mut live_ids: HashSet<&str> = HashSet::new();
    for spec in registered {
        if !live_ids.insert(spec.id.as_str()) {
            return Err("registered_provider_ids_duplicate");
        }
    }
    for decl in declared {
        let Some(live) = registered.iter().find(|spec| spec.id == decl.id) else {
            return Err("declared_provider_not_registered");
        };
        if live.display_name != decl.display_name {
            return Err("registered_provider_display_name_mismatch");
        }
        if live.description != decl.description {
            return Err("registered_provider_description_mismatch");
        }
        if live.config_schema != decl.config_schema {
            return Err("registered_provider_config_schema_mismatch");
        }
        if live.models.len() != decl.models.len() {
            return Err("registered_provider_model_counts_differ");
        }
        let mut live_model_ids: HashSet<&str> = HashSet::new();
        for model in &live.models {
            if !live_model_ids.insert(model.id.as_str()) {
                return Err("registered_provider_model_ids_duplicate");
            }
        }
        for declared_model in &decl.models {
            let Some(live_model) = live.models.iter().find(|m| m.id == declared_model.id) else {
                return Err("declared_provider_model_not_registered");
            };
            if live_model.display_name != declared_model.display_name {
                return Err("registered_provider_model_display_name_mismatch");
            }
            if live_model.context_window != declared_model.context_window {
                return Err("registered_provider_model_context_window_mismatch");
            }
            if live_model.capabilities != declared_model.capabilities {
                return Err("registered_provider_model_capabilities_mismatch");
            }
        }
    }
    Ok(())
}

impl DeclaredExtensionProvider {
    /// Convert a VALIDATED passive declaration into the runtime's
    /// `RegisteredProviderSpec` shape for metadata-only registration
    /// (Task 20 Commit C). Field-for-field: routing later re-validates the
    /// live runtime's registrations against these same declarations.
    pub fn to_registered_spec(
        &self,
    ) -> crate::extensions::runtime::process::RegisteredProviderSpec {
        crate::extensions::runtime::process::RegisteredProviderSpec {
            id: self.id.clone(),
            display_name: self.display_name.clone(),
            description: self.description.clone(),
            models: self
                .models
                .iter()
                .map(
                    |model| crate::extensions::runtime::process::RegisteredProviderModelSpec {
                        id: model.id.clone(),
                        display_name: model.display_name.clone(),
                        capabilities: model.capabilities.clone(),
                        context_window: model.context_window,
                    },
                )
                .collect(),
            config_schema: self.config_schema.clone(),
        }
    }
}

/// A dormant, manifest-declared extension tool. Registered like any live
/// tool (stable `ext:<plugin>:<name>` identity, schema, digest — so wire
/// resolution, discovery, exact grants, and the execution gate all work
/// unchanged), but its implementation is DEFERRED: executing it requires
/// the session extension-runtime lease capability (lease lifecycle
/// commit), which starts only the OWNING extension after the execution
/// gate has already authorized the call. Without that capability it fails
/// typed and spawns nothing.
pub struct DeferredExtensionTool {
    plugin_id: String,
    runtime_name: String,
    tool_name: String,
    description: String,
    input_schema: Value,
    expected_digest: SchemaDigest,
}

impl DeferredExtensionTool {
    fn new(plugin_id: &str, declared: &DeclaredExtensionTool) -> Self {
        Self {
            plugin_id: plugin_id.to_string(),
            // Naming parity with live registration ("<plugin>:<tool>").
            runtime_name: format!("{}:{}", plugin_id, declared.name),
            tool_name: declared.name.clone(),
            description: BoundedText::new(&declared.description, DECLARED_DESCRIPTION_MAX_BYTES)
                .text,
            input_schema: declared.input_schema.clone(),
            expected_digest: SchemaDigest::of_schema(&declared.input_schema),
        }
    }

    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    /// Digest of the pinned declaration — equal by construction to this
    /// tool's catalog record digest.
    pub fn expected_digest(&self) -> &SchemaDigest {
        &self.expected_digest
    }
}

#[async_trait::async_trait]
impl Tool for DeferredExtensionTool {
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Extension {
            extension_id: self.plugin_id.clone(),
        }
    }

    fn name(&self) -> &str {
        &self.runtime_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.input_schema.clone()
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> crate::Result<String> {
        // Runs strictly AFTER the ExecutionGate authorized this exact call.
        // Without the typed session lease capability, a deferred extension
        // tool NEVER starts a process.
        let Some(leases) = ctx.capabilities.extension_leases.clone() else {
            return Err(crate::RuntimeError::Tool(format!(
                "extension tool '{}' (plugin '{}') is activation-deferred and no extension \
                 runtime lease capability is available in this context; no process was started",
                self.runtime_name, self.plugin_id
            )));
        };
        match leases
            .call_exact(
                &self.plugin_id,
                &self.tool_name,
                &self.expected_digest,
                params,
            )
            .await
        {
            Ok(value) => Ok(render_tool_result(&value)),
            Err(err) => {
                // Grant invalidation (spec §7.5): record removal, manifest/
                // permission re-validation failure, launch-record (config)
                // drift, catalog drift, and runtime declaration mismatches
                // poison the pinned declaration, so the EXACT session grant
                // must fall with the lease. Transport/capacity/revocation-
                // race and extension-reported tool errors are transient and
                // must NOT revoke.
                if err.revokes_exact_grant() {
                    if let Some(activation) = ctx.capabilities.tool_activation.as_ref() {
                        if activation.revoke_exact_extension_grant(
                            leases.session(),
                            &self.plugin_id,
                            &self.tool_name,
                        ) {
                            tracing::debug!(
                                "revoked exact extension activation after declaration invalidation"
                            );
                        }
                    }
                }
                Err(crate::RuntimeError::Tool(err.to_string()))
            }
        }
    }
}

/// Render an extension tool-call result value exactly like the eager
/// `ExtensionTool` path: plain string, `content` string field, or the
/// serialized value.
fn render_tool_result(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        text.to_string()
    } else if let Some(text) = value.get("content").and_then(Value::as_str) {
        text.to_string()
    } else {
        value.to_string()
    }
}

impl DeclaredExtensionTool {
    /// Stable canonical schema digest of this declaration. VISIBILITY-ONLY
    /// metadata (diagnostics/inventory); grants and gate checks always
    /// recompute from catalog records, never trust this accessor.
    pub fn schema_digest(&self) -> SchemaDigest {
        SchemaDigest::of_schema(&self.input_schema)
    }
}

/// Build the dormant deferred tools for one manifest. Deterministic
/// (declaration order); returns nothing for manifests without validated
/// declarations — descriptors are never invented. Never spawns.
pub fn dormant_extension_tools(
    plugin_id: &str,
    manifest: &ExtensionManifest,
) -> Vec<Arc<dyn Tool>> {
    // Plugin ids with ':' would break `<plugin>:<tool>` reverse identity.
    if plugin_id.is_empty() || plugin_id.contains(':') || plugin_id.chars().any(char::is_control) {
        return Vec::new();
    }
    let Some(deferred) = &manifest.deferred else {
        return Vec::new();
    };
    // Full manifest-level policy (bounds + permission/hook coupling), not
    // just block bounds: dormant descriptors minted for a manifest that
    // lacks `tools.register` would be an unauthorized catalog grant.
    if validate_manifest_deferred(manifest).is_err() {
        return Vec::new();
    }
    deferred
        .tools
        .iter()
        .map(|declared| Arc::new(DeferredExtensionTool::new(plugin_id, declared)) as Arc<dyn Tool>)
        .collect()
}
