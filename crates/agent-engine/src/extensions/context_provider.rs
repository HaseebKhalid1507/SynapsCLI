//! Task A3 (continuous-memory spec §7.1) — typed memory/context
//! contribution provider identities and DORMANT descriptors.
//!
//! A context provider is a passive, additive manifest capability
//! (`deferred.context_providers`) that lets an extension — e.g.
//! `extension:axel-memory-manager:project-memory` — offer bounded context
//! contributions to the host. Everything in this module is metadata only:
//! nothing here spawns a process, touches the network, or acquires a
//! lease. Descriptors mirror how [`super::lifecycle::DeferredExtensionTool`]
//! pins dormant deferred-tool metadata before any
//! [`super::lease::ExtensionRuntimeManager`] lease exists — activation
//! (exact memory-context lease acquisition) is task A6 and is NOT
//! implemented here.

use super::lifecycle::{
    validate_manifest_deferred, DeclaredExtensionContextProvider, DECLARED_DESCRIPTION_MAX_BYTES,
    DECLARED_NAME_MAX_BYTES,
};
use super::manifest::ExtensionManifest;
use agent_core::BoundedText;

/// Hard byte bound on a context provider id (parity with the declared-name
/// budget in [`super::lifecycle`]).
pub const CONTEXT_PROVIDER_ID_MAX_BYTES: usize = DECLARED_NAME_MAX_BYTES;

/// Validated context provider identity: the LOCAL id an extension declares
/// (the full runtime address is composed as `extension:<plugin>:<id>`).
///
/// Newtype invariants (enforced by [`ContextProviderId::parse`], the ONLY
/// constructor — fail closed on any violation):
/// - non-empty;
/// - at most [`CONTEXT_PROVIDER_ID_MAX_BYTES`] bytes;
/// - ASCII-safe: printable ASCII only (no control chars, no whitespace,
///   no non-ASCII);
/// - no `':'` so the composed runtime address stays reverse-parseable.
///
/// Comparison is EXACT byte equality; there is no public mutable access to
/// the inner string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContextProviderId(String);

impl ContextProviderId {
    /// Parse a candidate id, failing closed on invalid input. Static
    /// reasons only.
    pub fn parse(candidate: &str) -> Result<Self, &'static str> {
        if candidate.is_empty() {
            return Err("empty_context_provider_id");
        }
        if candidate.len() > CONTEXT_PROVIDER_ID_MAX_BYTES {
            return Err("oversized_context_provider_id");
        }
        if !candidate.chars().all(|c| c.is_ascii_graphic() && c != ':') {
            return Err("invalid_context_provider_id_characters");
        }
        Ok(Self(candidate.to_string()))
    }

    /// Read-only view of the exact validated id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ContextProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A DORMANT, manifest-declared context provider descriptor: bounded
/// metadata pinned at load, exactly like the dormant deferred-tool
/// descriptors the lease manager later validates against ([`super::
/// lifecycle::DeferredExtensionTool`] / `ExtensionRuntimeManager`).
/// Holding one grants NOTHING: discovery, schema export, and status
/// inspection read this metadata without ever starting the owning
/// extension. Lease acquisition (task A6) is the only activation path and
/// is not part of this type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredContextProviderDescriptor {
    id: ContextProviderId,
    capability: String,
    description: String,
    schema_version: u32,
    extension_id: String,
}

impl RegisteredContextProviderDescriptor {
    /// Pin one VALIDATED declaration for one owning extension. Fails
    /// closed (static reason) if either identity violates its bound —
    /// mirrors `DeferredExtensionTool::new`'s bounded pinning.
    pub fn new(
        extension_id: &str,
        declared: &DeclaredExtensionContextProvider,
    ) -> Result<Self, &'static str> {
        // Plugin ids with ':' would break `extension:<plugin>:<id>`
        // reverse identity (same rule as dormant_extension_tools).
        if extension_id.is_empty()
            || extension_id.contains(':')
            || extension_id.chars().any(char::is_control)
        {
            return Err("invalid_context_provider_extension_id");
        }
        let id = ContextProviderId::parse(&declared.id)?;
        if declared.capability.is_empty() || declared.capability.len() > DECLARED_NAME_MAX_BYTES {
            return Err("invalid_context_provider_capability");
        }
        if declared.schema_version == 0 {
            return Err("invalid_context_provider_schema_version");
        }
        Ok(Self {
            id,
            capability: BoundedText::new(&declared.capability, DECLARED_NAME_MAX_BYTES).text,
            description: BoundedText::new(&declared.description, DECLARED_DESCRIPTION_MAX_BYTES)
                .text,
            schema_version: declared.schema_version,
            extension_id: extension_id.to_string(),
        })
    }

    pub fn id(&self) -> &ContextProviderId {
        &self.id
    }

    pub fn capability(&self) -> &str {
        &self.capability
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// The owning extension (plugin) id.
    pub fn extension_id(&self) -> &str {
        &self.extension_id
    }

    /// The composed runtime address, e.g.
    /// `extension:axel-memory-manager:project-memory` (spec §7.1).
    pub fn runtime_address(&self) -> String {
        format!("extension:{}:{}", self.extension_id, self.id.as_str())
    }
}

/// Shared handle to the PUBLISHED context-provider catalog snapshot (task
/// A6). Mirrors [`super::manager::SharedDeferredRecords`]: the
/// [`super::manager::ExtensionManager`] owns the catalog and republishes
/// this snapshot on every load/unload mutation; the
/// [`super::lease::ExtensionRuntimeManager`] (and, through it, the
/// `Runtime` memory-context enable path) reads the CURRENT snapshot with a
/// sync lock held only for slice operations — never across I/O, and never
/// spawning anything.
pub(crate) type SharedContextProviderCatalog =
    std::sync::Arc<std::sync::Mutex<Vec<RegisteredContextProviderDescriptor>>>;

/// Typed fail-closed context-provider resolution failure (task A6).
/// Static-reason style like every other identity gate in this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextProviderLookupError {
    /// No loaded extension declares the requested provider identity (or,
    /// for an id-less request, no loaded extension declares any context
    /// provider at all).
    NotRegistered,
    /// More than one loaded extension declaration matches: overlapping
    /// capability surfaces with no exact disambiguation. Fail closed —
    /// the host never picks one arbitrarily.
    Ambiguous,
}

/// Resolve one context-provider request against the loaded catalog.
/// EXACT-SCOPE, FAIL-CLOSED matching — the same discipline as
/// [`super::lifecycle::validate_manifest_deferred`] and
/// `crate::orchestration::validate_user_authorizable_model`: no partial,
/// prefix, or fuzzy matches, and no arbitrary tie-breaking.
///
/// * `requested = Some(id)` — Ok iff EXACTLY ONE catalog entry declares
///   that exact local id; zero matches fail `NotRegistered`, two or more
///   (the same id declared by multiple installed extensions) fail
///   `Ambiguous`.
/// * `requested = None` — the request names no explicit provider: Ok iff
///   the whole catalog holds EXACTLY ONE descriptor; an empty catalog
///   fails `NotRegistered` and overlapping declarations fail `Ambiguous`.
///
/// Pure slice read: never spawns, never touches lease state.
pub fn resolve_context_provider<'a>(
    catalog: &'a [RegisteredContextProviderDescriptor],
    requested: Option<&ContextProviderId>,
) -> Result<&'a RegisteredContextProviderDescriptor, ContextProviderLookupError> {
    let mut matches = catalog
        .iter()
        .filter(|descriptor| requested.is_none_or(|id| descriptor.id() == id));
    match (matches.next(), matches.next()) {
        (None, _) => Err(ContextProviderLookupError::NotRegistered),
        (Some(only), None) => Ok(only),
        (Some(_), Some(_)) => Err(ContextProviderLookupError::Ambiguous),
    }
}

/// Build the dormant context provider descriptors for one manifest.
/// Deterministic (declaration order); returns nothing for manifests
/// without validated declarations — descriptors are never invented, and a
/// manifest that fails the full deferred policy (bounds + the
/// `context_providers.register` permission gate) yields NOTHING. Mirrors
/// [`super::lifecycle::dormant_extension_tools`]. Never spawns.
pub fn dormant_context_provider_descriptors(
    plugin_id: &str,
    manifest: &ExtensionManifest,
) -> Vec<RegisteredContextProviderDescriptor> {
    let Some(deferred) = &manifest.deferred else {
        return Vec::new();
    };
    if validate_manifest_deferred(manifest).is_err() {
        return Vec::new();
    }
    let mut descriptors = Vec::with_capacity(deferred.context_providers.len());
    for declared in &deferred.context_providers {
        match RegisteredContextProviderDescriptor::new(plugin_id, declared) {
            Ok(descriptor) => descriptors.push(descriptor),
            // Any single invalid declaration poisons the set: partial
            // capability surfaces are never minted.
            Err(_) => return Vec::new(),
        }
    }
    descriptors
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared(id: &str) -> DeclaredExtensionContextProvider {
        DeclaredExtensionContextProvider {
            id: id.to_string(),
            capability: "project-memory".to_string(),
            description: "Project memory context contributions".to_string(),
            schema_version: 1,
        }
    }

    // ── ContextProviderId::parse ───────────────────────────────────────

    #[test]
    fn parse_accepts_bounded_ascii_ids_exactly() {
        let id = ContextProviderId::parse("project-memory").unwrap();
        assert_eq!(id.as_str(), "project-memory");
        assert_eq!(id, ContextProviderId::parse("project-memory").unwrap());
        // Exact equality: near-misses are different identities.
        assert_ne!(id, ContextProviderId::parse("project-memory2").unwrap());
        assert_ne!(id, ContextProviderId::parse("Project-memory").unwrap());
        // Display renders the exact id.
        assert_eq!(id.to_string(), "project-memory");
        // Max-length id is accepted; one past the bound is not.
        let max = "a".repeat(CONTEXT_PROVIDER_ID_MAX_BYTES);
        assert!(ContextProviderId::parse(&max).is_ok());
    }

    #[test]
    fn parse_rejects_empty_and_over_bound() {
        assert_eq!(
            ContextProviderId::parse(""),
            Err("empty_context_provider_id")
        );
        let over = "a".repeat(CONTEXT_PROVIDER_ID_MAX_BYTES + 1);
        assert_eq!(
            ContextProviderId::parse(&over),
            Err("oversized_context_provider_id")
        );
    }

    #[test]
    fn parse_rejects_unsafe_characters_fail_closed() {
        for bad in [
            "has:colon",
            "has space",
            "ctrl\u{7}char",
            "tab\there",
            "new\nline",
            "unicodé",
            "emoji💾",
        ] {
            assert!(
                ContextProviderId::parse(bad).is_err(),
                "must reject {bad:?}"
            );
        }
    }

    // ── dormant descriptors ────────────────────────────────────────────

    #[test]
    fn descriptor_pins_bounded_dormant_metadata_only() {
        let descriptor = RegisteredContextProviderDescriptor::new(
            "axel-memory-manager",
            &declared("project-memory"),
        )
        .unwrap();
        assert_eq!(descriptor.id().as_str(), "project-memory");
        assert_eq!(descriptor.capability(), "project-memory");
        assert_eq!(
            descriptor.description(),
            "Project memory context contributions"
        );
        assert_eq!(descriptor.schema_version(), 1);
        assert_eq!(descriptor.extension_id(), "axel-memory-manager");
        // Spec §7.1 runtime address composition.
        assert_eq!(
            descriptor.runtime_address(),
            "extension:axel-memory-manager:project-memory"
        );
    }

    #[test]
    fn descriptor_fails_closed_on_invalid_identities() {
        // Bad owning extension ids.
        for bad in ["", "has:colon", "ctrl\u{7}"] {
            assert!(RegisteredContextProviderDescriptor::new(bad, &declared("p")).is_err());
        }
        // Bad declared id flows through ContextProviderId::parse.
        assert!(RegisteredContextProviderDescriptor::new("plug", &declared("bad:id")).is_err());
        // Zero schema version is invalid.
        let mut zero = declared("p");
        zero.schema_version = 0;
        assert!(RegisteredContextProviderDescriptor::new("plug", &zero).is_err());
    }

    // ── task A6: exact-scope, fail-closed catalog resolution ───────────

    fn descriptor(plugin: &str, id: &str) -> RegisteredContextProviderDescriptor {
        RegisteredContextProviderDescriptor::new(plugin, &declared(id)).unwrap()
    }

    #[test]
    fn resolve_empty_catalog_fails_not_registered() {
        let id = ContextProviderId::parse("project-memory").unwrap();
        assert_eq!(
            resolve_context_provider(&[], Some(&id)).unwrap_err(),
            ContextProviderLookupError::NotRegistered
        );
        assert_eq!(
            resolve_context_provider(&[], None).unwrap_err(),
            ContextProviderLookupError::NotRegistered
        );
    }

    #[test]
    fn resolve_exactly_one_match_succeeds_with_and_without_explicit_id() {
        let catalog = vec![descriptor("axel-memory-manager", "project-memory")];
        let id = ContextProviderId::parse("project-memory").unwrap();
        for requested in [Some(&id), None] {
            let found = resolve_context_provider(&catalog, requested).unwrap();
            assert_eq!(
                found.runtime_address(),
                "extension:axel-memory-manager:project-memory"
            );
        }
        // Exact matching only: a near-miss id is NotRegistered, never a
        // partial match.
        let near_miss = ContextProviderId::parse("project-memory2").unwrap();
        assert_eq!(
            resolve_context_provider(&catalog, Some(&near_miss)).unwrap_err(),
            ContextProviderLookupError::NotRegistered
        );
    }

    #[test]
    fn resolve_overlapping_declarations_fail_closed_ambiguous() {
        // Two installed extensions declare the SAME local id.
        let same_id = vec![
            descriptor("mem-a", "project-memory"),
            descriptor("mem-b", "project-memory"),
        ];
        let id = ContextProviderId::parse("project-memory").unwrap();
        assert_eq!(
            resolve_context_provider(&same_id, Some(&id)).unwrap_err(),
            ContextProviderLookupError::Ambiguous
        );
        assert_eq!(
            resolve_context_provider(&same_id, None).unwrap_err(),
            ContextProviderLookupError::Ambiguous
        );

        // Distinct ids: an id-less request is still ambiguous (the host
        // never picks one arbitrarily), but an exact id resolves uniquely.
        let distinct = vec![descriptor("mem-a", "alpha"), descriptor("mem-b", "beta")];
        assert_eq!(
            resolve_context_provider(&distinct, None).unwrap_err(),
            ContextProviderLookupError::Ambiguous
        );
        let beta = ContextProviderId::parse("beta").unwrap();
        let found = resolve_context_provider(&distinct, Some(&beta)).unwrap();
        assert_eq!(found.extension_id(), "mem-b");
    }
}
