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
use super::validation::validate_id_segment;
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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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
    /// Accepted and ignored. Built-in automatic memory, which this declared
    /// a provider for, was removed; the key stays so a manifest written for
    /// it still loads. Whatever JSON it holds is neither validated nor read:
    /// it cannot fail a load, classify the extension or grant anything
    /// (see [`ignored_context_provider_notice`]).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub context_providers: Value,
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

/// The line to log when a manifest still declares a context provider (the
/// `deferred.context_providers` key with anything in it, or the
/// `context_providers.register` permission), `None` when it declares
/// neither. The extension loads as if the declaration were absent: its
/// tools and hooks work, and nothing starts it "on a memory lease", because
/// no such lease exists any more.
pub fn ignored_context_provider_notice(id: &str, manifest: &ExtensionManifest) -> Option<String> {
    let declared_field = manifest.deferred.as_ref().is_some_and(|deferred| {
        match &deferred.context_providers {
            Value::Null => false,
            Value::Array(items) => !items.is_empty(),
            Value::Object(fields) => !fields.is_empty(),
            _ => true,
        }
    });
    let declared_permission = manifest
        .permissions
        .iter()
        .any(|permission| permission == "context_providers.register");
    (declared_field || declared_permission).then(|| {
        format!(
            "extension {id} declares a context provider; built-in automatic memory was removed \
             in {}, the declaration is ignored",
            agent_core::AXEL_BACKEND_REMOVED_IN
        )
    })
}

/// Classify a manifest from its passive declarations. Pure and local.
/// `deferred.context_providers` is not one of them: it is ignored, so a
/// manifest that declares only that classifies like one with an empty
/// `deferred` block.
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

/// Manifest-time validation for a name that BECOMES a runtime capability id
/// (declared tool names, provider ids, model ids).
///
/// Deliberately delegates to [`validate_id_segment`] — the exact function the
/// runtime applies at `initialize` — so the two checks cannot drift. They
/// previously did: manifest-time `valid_name` allowed 128 bytes and embedded
/// whitespace, while the runtime caps ids at
/// [`MAX_ID_LENGTH`][super::validation::MAX_ID_LENGTH] (64) and rejects
/// whitespace outright. A 65–128 byte or whitespace-bearing name passed
/// manifest validation and then died at `initialize` with an unrelated error.
///
/// No working extension is affected by tightening this: any name that this
/// now rejects was already guaranteed to fail at `initialize`. The only
/// change is that the failure is early, local, and says what is wrong.
fn valid_capability_id(name: &str) -> bool {
    validate_id_segment(name).is_ok()
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
        if !valid_capability_id(&provider.id) {
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
            if !valid_capability_id(&model.id) {
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
    if !valid_capability_id(name) {
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

    fn source_operation(&self) -> Option<&str> {
        Some(&self.tool_name)
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
        // Who calls: the author of the binding the context came with, the
        // one `forum_post` records for this run. No binding, no caller: the
        // stand-in made below is the process's, not a run's.
        let caller = ctx
            .capabilities
            .memory_backend
            .as_ref()
            .map(|binding| binding.forum_author().clone());
        let binding = ctx
            .capabilities
            .memory_backend
            .clone()
            .unwrap_or_else(crate::memory_backend::MemoryBinding::configured_current);
        if binding.exclusive() && crate::memory_backend::competing_memory_tool(self) {
            return Err(crate::RuntimeError::Tool("independent extension memory is disabled by the selected host backend; use short memory tools".into()));
        }
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
            .call_exact_as(
                &self.plugin_id,
                &self.tool_name,
                &self.expected_digest,
                params,
                ctx.capabilities.session_id.as_deref(),
                ctx.capabilities.cwd.as_deref(),
                caller.as_ref(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base_manifest(permissions: &[&str]) -> ExtensionManifest {
        serde_json::from_value(json!({
            "runtime": "process",
            "command": "/bin/false",
            "permissions": permissions,
        }))
        .unwrap()
    }

    fn declared_tool(name: &str) -> DeclaredExtensionTool {
        DeclaredExtensionTool {
            name: name.to_string(),
            description: format!("declared {name}"),
            input_schema: json!({"type": "object"}),
        }
    }

    #[test]
    fn a_deferred_extension_tool_names_its_registered_operation() {
        let tool = DeferredExtensionTool::new("plug", &declared_tool("memory_fetch"));
        assert_eq!(tool.name(), "plug:memory_fetch");
        assert_eq!(tool.source_operation(), Some("memory_fetch"));
    }

    #[test]
    fn absent_or_empty_deferred_blocks_classify_exactly_as_before() {
        // No deferred block at all.
        let legacy = base_manifest(&["tools.register"]);
        assert!(legacy.deferred.is_none());
        assert_eq!(classify(&legacy), ExtensionClass::LegacyEager);
        assert_eq!(earliest_trigger(&legacy), ActivationTrigger::Eager);
        legacy.validate("legacy").unwrap();

        // Empty deferred block.
        let mut empty = base_manifest(&["tools.register"]);
        empty.deferred = Some(DeferredDeclarations::default());
        assert!(ignored_context_provider_notice("empty", &empty).is_none());
        assert_eq!(classify(&empty), ExtensionClass::LegacyEager);
        assert_eq!(earliest_trigger(&empty), ActivationTrigger::Eager);
        empty.validate("empty").unwrap();

        // A deferred block WITHOUT the context_providers key deserializes
        // to null and the key does not appear on serialization.
        let d: DeferredDeclarations = serde_json::from_value(json!({
            "tools": [], "providers": []
        }))
        .unwrap();
        assert!(d.context_providers.is_null());
        let round = serde_json::to_value(&d).unwrap();
        assert!(round.get("context_providers").is_none());

        // Tool-only stays ToolOnly...
        let mut tool_only = base_manifest(&["tools.register"]);
        tool_only.deferred = Some(DeferredDeclarations {
            tools: vec![declared_tool("t")],
            ..DeferredDeclarations::default()
        });
        assert_eq!(classify(&tool_only), ExtensionClass::ToolOnly);
        // ...user lifecycle stays UiSidecar...
        let mut sidecar = base_manifest(&["tools.register"]);
        sidecar.deferred = Some(DeferredDeclarations {
            lifecycle: Some(DeferredLifecycle::User),
            ..DeferredDeclarations::default()
        });
        assert_eq!(classify(&sidecar), ExtensionClass::UiSidecar);
        // ...and hook subscriptions stay HookLifecycle.
        let mut hooky: ExtensionManifest = serde_json::from_value(json!({
            "runtime": "process",
            "command": "/bin/false",
            "permissions": ["tools.intercept"],
            "hooks": [{"hook": "before_tool_call"}]
        }))
        .unwrap();
        hooky.deferred = Some(DeferredDeclarations::default());
        assert_eq!(classify(&hooky), ExtensionClass::HookLifecycle);
    }

    // ── a declared context provider is accepted and ignored ─────────────

    fn provider_declaration() -> Value {
        json!([{
            "id": "project-memory",
            "capability": "project-memory",
            "description": "Project memory context contributions",
            "schema_version": 1
        }])
    }

    fn notice(id: &str) -> String {
        format!(
            "extension {id} declares a context provider; built-in automatic memory was removed \
             in {}, the declaration is ignored",
            agent_core::AXEL_BACKEND_REMOVED_IN
        )
    }

    /// The trigger that started such an extension (`/memory on`) is gone,
    /// so "start on user action" would mean "never". A manifest whose
    /// `deferred` block holds only a context provider classifies like one
    /// with an empty block, and the field changes no other class.
    #[test]
    fn a_declared_context_provider_does_not_classify_the_extension() {
        let only: ExtensionManifest = serde_json::from_value(json!({
            "runtime": "process",
            "command": "/bin/false",
            "permissions": ["context_providers.register"],
            "deferred": {"context_providers": provider_declaration()}
        }))
        .unwrap();
        only.validate("axel-memory-manager").unwrap();
        assert_eq!(classify(&only), ExtensionClass::LegacyEager);
        assert_eq!(earliest_trigger(&only), ActivationTrigger::Eager);
        assert_ne!(earliest_trigger(&only), ActivationTrigger::UserAction);

        // With tools beside it: a tool extension, started by its tool.
        let with_tools: ExtensionManifest = serde_json::from_value(json!({
            "runtime": "process",
            "command": "/bin/false",
            "permissions": ["tools.register", "context_providers.register"],
            "deferred": {
                "tools": [{"name": "t", "description": "declared t", "input_schema": {"type": "object"}}],
                "context_providers": provider_declaration()
            }
        }))
        .unwrap();
        with_tools.validate("plug").unwrap();
        assert_eq!(classify(&with_tools), ExtensionClass::ToolOnly);
        assert_eq!(earliest_trigger(&with_tools), ActivationTrigger::ExactToolActivation);

        // With a hook beside it: started by its first hook event.
        let with_hook: ExtensionManifest = serde_json::from_value(json!({
            "runtime": "process",
            "command": "/bin/false",
            "permissions": ["tools.intercept", "context_providers.register"],
            "hooks": [{"hook": "before_tool_call"}],
            "deferred": {"context_providers": provider_declaration()}
        }))
        .unwrap();
        with_hook.validate("plug").unwrap();
        assert_eq!(classify(&with_hook), ExtensionClass::HookLifecycle);
        assert_eq!(
            earliest_trigger(&with_hook),
            ActivationTrigger::FirstAuthorizedHookEvent
        );
    }

    /// Loading a manifest that declares the field or the permission says so
    /// in one line; one that declares neither says nothing.
    #[test]
    fn a_declared_context_provider_is_reported_as_ignored() {
        let with = |permissions: &[&str], deferred: Option<Value>| -> ExtensionManifest {
            let mut manifest = json!({
                "runtime": "process",
                "command": "/bin/false",
                "permissions": permissions,
            });
            if let Some(deferred) = deferred {
                manifest["deferred"] = deferred;
            }
            serde_json::from_value(manifest).unwrap()
        };
        // The field, with or without the permission.
        for permissions in [&["context_providers.register"][..], &["tools.register"][..]] {
            let m = with(permissions, Some(json!({"context_providers": provider_declaration()})));
            assert_eq!(ignored_context_provider_notice("mem", &m), Some(notice("mem")));
        }
        // The permission alone.
        let m = with(&["context_providers.register"], None);
        assert_eq!(ignored_context_provider_notice("mem", &m), Some(notice("mem")));
        // Neither, or an empty declaration: nothing to say.
        for deferred in [
            None,
            Some(json!({})),
            Some(json!({"context_providers": []})),
            Some(json!({"context_providers": null})),
        ] {
            let m = with(&["tools.register"], deferred);
            assert_eq!(ignored_context_provider_notice("plain", &m), None);
        }
    }

    /// The field is not validated any more, so nothing in it can fail a
    /// load: not a missing permission, not a count, not a shape that the
    /// old typed declaration would have refused to parse.
    #[test]
    fn a_declared_context_provider_can_never_fail_a_manifest() {
        let many: Vec<Value> = (0..64).map(|i| json!({"id": format!("p{i}")})).collect();
        for declaration in [
            provider_declaration(),
            json!([{"id": "has:colon", "capability": "", "schema_version": 0}]),
            json!([{"id": "dup"}, {"id": "dup"}]),
            Value::Array(many),
            json!("project-memory"),
            json!({"id": "project-memory"}),
            json!(7),
        ] {
            let m: ExtensionManifest = serde_json::from_value(json!({
                "runtime": "process",
                "command": "/bin/false",
                "permissions": ["tools.register"],
                "deferred": {"context_providers": declaration.clone()}
            }))
            .unwrap_or_else(|e| panic!("{declaration} must parse: {e}"));
            m.validate("plug")
                .unwrap_or_else(|e| panic!("{declaration} must validate: {e}"));
            assert_eq!(classify(&m), ExtensionClass::LegacyEager, "{declaration}");
        }
    }

    // ── T291 D4: manifest-time ids must be validated by the SAME function
    // the runtime uses at initialize, or names pass here and die there.

    /// Every name that manifest validation accepts for a declared tool MUST
    /// also be accepted by the runtime's `validate_id_segment`, and vice
    /// versa. This is the anti-drift invariant; the two checks used to
    /// disagree on length (128 vs 64) and on whitespace.
    #[test]
    fn declared_capability_ids_agree_with_runtime_validation() {
        let cases = [
            "ok",
            "with-dash",
            "with_underscore",
            "MiXedCase123",
            "",
            "has space",
            "has\ttab",
            "colon:name",
            "trailing ",
            &"a".repeat(super::super::validation::MAX_ID_LENGTH),
            &"a".repeat(super::super::validation::MAX_ID_LENGTH + 1),
            &"a".repeat(DECLARED_NAME_MAX_BYTES),
        ];
        for name in cases {
            let manifest_ok = validate_tool_shape(name, "d", &json!({"type": "object"})).is_ok();
            let runtime_ok = validate_id_segment(name).is_ok();
            assert_eq!(
                manifest_ok, runtime_ok,
                "manifest/runtime disagree on declared tool name {name:?}: \
                 manifest_ok={manifest_ok} runtime_ok={runtime_ok}"
            );
        }
    }

    /// The exact defect from the S287 audit: a 65-byte name sat in the gap
    /// between DECLARED_NAME_MAX_BYTES (128) and MAX_ID_LENGTH (64). It
    /// passed manifest validation and then failed at initialize with an
    /// unrelated error. It must now fail early, at the manifest.
    #[test]
    fn declared_tool_name_over_runtime_bound_fails_at_manifest() {
        let name = "a".repeat(super::super::validation::MAX_ID_LENGTH + 1);
        assert_eq!(
            validate_deferred(&DeferredDeclarations {
                tools: vec![declared_tool(&name)],
                ..Default::default()
            }),
            Err("invalid_declared_tool_name"),
        );
        // ...and it is genuinely inside the old accepted range, i.e. this
        // test would have failed before the fix.
        assert!(name.len() <= DECLARED_NAME_MAX_BYTES);
    }

    /// Whitespace was the second, unfiled half of the same defect.
    #[test]
    fn declared_capability_ids_reject_whitespace_at_manifest() {
        assert_eq!(
            validate_deferred(&DeferredDeclarations {
                tools: vec![declared_tool("my tool")],
                ..Default::default()
            }),
            Err("invalid_declared_tool_name"),
        );
    }

    // ── an exclusive memory binding refuses aliased memory tools ────────

    const MEMORY_REFUSAL: &str = "independent extension memory is disabled by the selected host backend; use short memory tools";

    async fn call_deferred(
        plugin: &str,
        operation: &str,
        kind: agent_core::config::MemoryBackendKind,
    ) -> String {
        let dir = tempfile::tempdir().unwrap();
        let binding = crate::tools::test_helpers::memory_binding(kind, dir.path());
        let tool = DeferredExtensionTool::new(plugin, &declared_tool(operation));
        match tool
            .execute(json!({}), crate::tools::test_helpers::context_with_memory(binding))
            .await
        {
            Err(crate::RuntimeError::Tool(message)) => message,
            other => panic!("expected a tool error, got {other:?}"),
        }
    }

    /// Regression: `memory:search` is offered as `memory_search`, yet only
    /// its operation `search` was checked.
    #[tokio::test]
    async fn an_exclusive_binding_refuses_deferred_extension_memory_aliases() {
        use agent_core::config::MemoryBackendKind::Axel;
        for (plugin, operation) in [
            ("memory", "search"),
            ("memory", "fetch"),
            ("Memory", "Search"),
            ("plug", "memory.store"),
            ("plug", "MEMORY_STORE"),
            ("axel-memory-manager", "memory_search"),
            ("axel-memory-manager", "memory_forget"),
        ] {
            assert_eq!(
                call_deferred(plugin, operation, Axel).await,
                MEMORY_REFUSAL,
                "{plugin}:{operation}"
            );
        }
    }

    /// The suffixed wire name the model sees beside the host's own tool
    /// reaches the deferred alias, which refuses.
    #[tokio::test]
    async fn the_suffixed_wire_name_of_a_deferred_alias_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let registry = crate::tools::ToolRegistry::from_tools_for_tests(vec![
            Arc::new(crate::tools::memory::MemorySearchTool),
            Arc::new(DeferredExtensionTool::new("memory", &declared_tool("search"))),
        ]);
        assert_eq!(registry.runtime_name_for_api("memory_search"), "memory_search");
        assert_eq!(registry.runtime_name_for_api("memory_search_2"), "memory:search");
        let alias = registry
            .get(registry.runtime_name_for_api("memory_search_2"))
            .expect("alias registered");
        let binding = crate::tools::test_helpers::memory_binding(
            agent_core::config::MemoryBackendKind::Axel,
            dir.path(),
        );
        match alias
            .execute(json!({}), crate::tools::test_helpers::context_with_memory(binding))
            .await
        {
            Err(crate::RuntimeError::Tool(message)) => assert_eq!(message, MEMORY_REFUSAL),
            other => panic!("expected the memory refusal, got {other:?}"),
        }
    }

    /// A nested `:` (`plug:memory:fetch`) cannot reach a deferred tool: the
    /// manifest refuses `:` in declared tool names, and a plugin id with `:`
    /// gets no deferred tools. The classifier still covers it for other
    /// routes (`memory_backend::competing`).
    #[test]
    fn a_nested_colon_name_never_becomes_a_deferred_tool() {
        assert_eq!(
            validate_deferred(&DeferredDeclarations {
                tools: vec![declared_tool("memory:fetch")],
                ..Default::default()
            }),
            Err("invalid_declared_tool_name"),
        );
        let mut manifest = base_manifest(&["tools.register"]);
        manifest.deferred = Some(DeferredDeclarations {
            tools: vec![declared_tool("fetch")],
            ..DeferredDeclarations::default()
        });
        assert_eq!(dormant_extension_tools("plug", &manifest).len(), 1);
        assert!(dormant_extension_tools("plug:memory", &manifest).is_empty());
    }

    /// Other tools, and every tool without an exclusive binding, go on to
    /// the lease check exactly as before (no lease here, so nothing starts).
    #[tokio::test]
    async fn other_deferred_tools_and_legacy_bindings_reach_the_lease_check() {
        use agent_core::config::MemoryBackendKind::{Axel, Legacy};
        for (plugin, operation, kind) in [
            ("weather", "search", Axel),
            ("memory", "summarize", Axel),
            ("memory", "search", Legacy),
            ("axel-memory-manager", "memory_search", Legacy),
        ] {
            let message = call_deferred(plugin, operation, kind).await;
            assert!(
                message.contains("is activation-deferred"),
                "{plugin}:{operation}: {message}"
            );
        }
    }
}
