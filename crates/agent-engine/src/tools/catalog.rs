//! `ToolCatalog` — all locally known capabilities (spec §7.1, Task 14).
//!
//! The catalog is a passive inventory integrated into [`super::ToolRegistry`]:
//! every registry construction and mutation path (initial construction,
//! subagent variants, dynamic registration, replacement, extension merge,
//! disable) keeps exactly one truthful catalog alongside the exposed schema
//! without changing what is exposed or executable. The registry remains the
//! active behavior projection until later tasks introduce `DiscoveryIndex`,
//! `SessionToolSet`, and `ExecutionGate`.
//!
//! Insertion invariants (spec §4.2):
//! - no implementation is constructed (factories are stored, never invoked);
//! - no process is started and no network is touched;
//! - no extension handler or MCP connection method is called;
//! - no schema is exposed to the model;
//! - no execution grant is issued.
//!
//! Mutation invariants:
//! - every successful mutation advances the generation by checked (never
//!   saturating) arithmetic — a mutation cannot succeed without a new
//!   generation, so stale activation grants can never keep validating;
//! - duplicate and no-op failures do not advance the generation.

use crate::tools::{Tool, ToolOrigin};
use agent_core::BoundedText;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;
use thiserror::Error;

pub use agent_core::orchestration::capability::{
    CatalogGeneration, CatalogGenerationOverflow, SchemaDigest, SessionActivationGrant, ToolId,
    ToolIdError,
};

/// Deferred implementation constructor. Stored at insertion, invoked only by
/// explicit later acquisition (execution-gate territory, not catalog).
pub type ToolFactory = Arc<dyn Fn() -> Arc<dyn Tool> + Send + Sync>;

/// Byte budget for raw identity fragments retained inside
/// [`CapabilitySource`] (extension/plugin/server ids and tool names).
const SOURCE_IDENTITY_MAX_BYTES: usize = 256;

/// Hex characters of SHA-256 appended to a truncated identity fragment
/// (160 bits, matching the `ToolId` digest-segment strength).
const SOURCE_TRUNCATION_DIGEST_HEX_LEN: usize = 40;

/// Bound a trust-relevant identity fragment without letting two distinct
/// oversized identities collapse into one displayed value: when truncation
/// occurs, an explicit `…#sha-<hex>` marker of the FULL raw bytes is
/// appended, so the result both records that it was cut and stays distinct
/// per original identity (up to SHA-256 collision). The source-aware
/// [`ToolId`] is always derived from the raw identity, never this bounded
/// form, so id collision resistance is unaffected.
fn bounded_identity(raw: &str) -> String {
    let bounded = BoundedText::new(raw, SOURCE_IDENTITY_MAX_BYTES);
    if !bounded.truncated {
        return bounded.text;
    }
    let digest = Sha256::digest(raw.as_bytes());
    let mut hex = String::with_capacity(SOURCE_TRUNCATION_DIGEST_HEX_LEN);
    for byte in digest.iter().take(SOURCE_TRUNCATION_DIGEST_HEX_LEN / 2) {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    let marker = format!("\u{2026}#sha-{hex}");
    let keep = SOURCE_IDENTITY_MAX_BYTES.saturating_sub(marker.len());
    format!("{}{marker}", BoundedText::new(raw, keep).text)
}

/// Where a capability comes from, carrying the exact runtime identities
/// (extension/plugin/server ids and per-source tool names). Drives the ID
/// namespace and, later, per-source permission policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapabilitySource {
    Builtin,
    Extension {
        extension_id: String,
        tool_name: String,
    },
    Mcp {
        server_id: String,
        server_tool_name: String,
    },
    Plugin {
        plugin_id: String,
        tool_name: String,
    },
    /// Dynamically registered with no declared origin. Kept explicit and
    /// conservative — never classified as builtin.
    Unknown {
        runtime_name: String,
    },
}

/// Permission/trust provenance recorded at catalog time. Evaluated (not
/// merely trusted) by the future execution gate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrustProvenance {
    /// Compiled into this runtime; trusted as the binary itself.
    BuiltinRuntime,
    /// Declared by a locally installed extension manifest.
    ExtensionManifest { extension_id: String },
    /// Declared by local MCP server configuration.
    McpConfig { server_id: String },
    /// Declared by a plugin definition.
    PluginConfig { plugin_id: String },
    /// No verifiable provenance. Conservative fail-closed default for
    /// unclassified dynamic registrations.
    Unverified,
}

/// Side-effect classification placeholder until Phase 4 (spec §8) introduces
/// real effect classes. Unknown capabilities stay explicitly unclassified so
/// later policy can treat them conservatively.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideEffectClass {
    Unclassified,
}

/// Where the full JSON schema for a capability lives. The catalog may hold
/// the schema locally, but never exposes it to the model.
#[derive(Clone, Debug)]
pub enum SchemaLocator {
    /// Full schema already known in-process (builtins, loaded extensions).
    Inline(Value),
}

impl SchemaLocator {
    fn schema(&self) -> &Value {
        match self {
            Self::Inline(schema) => schema,
        }
    }
}

/// One locally known capability. Construction is pure: it computes the
/// deterministic schema digest and bounds the compact descriptor, but never
/// invokes the implementation factory.
#[derive(Clone)]
pub struct CapabilityRecord {
    id: ToolId,
    source: CapabilitySource,
    summary: String,
    tags: Vec<String>,
    schema: SchemaLocator,
    schema_digest: SchemaDigest,
    factory: ToolFactory,
    provenance: TrustProvenance,
    side_effect: SideEffectClass,
}

impl CapabilityRecord {
    /// Byte budget for the compact summary (descriptor, not full schema).
    pub const SUMMARY_MAX_BYTES: usize = 256;
    /// Maximum number of retained tags.
    pub const MAX_TAGS: usize = 8;
    /// Byte budget per retained tag.
    pub const TAG_MAX_BYTES: usize = 32;

    pub fn new(
        id: ToolId,
        source: CapabilitySource,
        summary: &str,
        tags: Vec<String>,
        schema: SchemaLocator,
        factory: ToolFactory,
        provenance: TrustProvenance,
    ) -> Self {
        let schema_digest = SchemaDigest::of_schema(schema.schema());
        let summary = BoundedText::new(summary, Self::SUMMARY_MAX_BYTES).text;
        let tags = tags
            .into_iter()
            .take(Self::MAX_TAGS)
            .map(|tag| BoundedText::new(&tag, Self::TAG_MAX_BYTES).text)
            .collect();
        Self {
            id,
            source,
            summary,
            tags,
            schema,
            schema_digest,
            factory,
            provenance,
            side_effect: SideEffectClass::Unclassified,
        }
    }

    /// Catalog an already-constructed registry tool passively from its
    /// declared [`ToolOrigin`]. Reads only in-process metadata (`name`,
    /// `description`, `parameters`, `origin`): no execution, no extension
    /// handler call, no MCP connection/process/network activity, no grant.
    ///
    /// Runtime identities are encoded through the source-aware [`ToolId`]
    /// constructors, so existing uppercase/Unicode/colon-bearing names are
    /// represented exactly rather than rejected, and undeclared origins are
    /// cataloged conservatively as unknown — never invented as builtin.
    pub fn for_registered_tool(tool: &Arc<dyn Tool>) -> Self {
        let (id, source, provenance) = identity_for_tool(tool.as_ref());
        let implementation = Arc::clone(tool);
        let factory: ToolFactory = Arc::new(move || Arc::clone(&implementation));
        Self::new(
            id,
            source,
            tool.description(),
            Vec::new(),
            SchemaLocator::Inline(tool.parameters()),
            factory,
            provenance,
        )
    }

    pub fn id(&self) -> &ToolId {
        &self.id
    }

    pub fn source(&self) -> &CapabilitySource {
        &self.source
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn tags(&self) -> &[String] {
        &self.tags
    }

    pub fn schema_locator(&self) -> &SchemaLocator {
        &self.schema
    }

    pub fn schema_digest(&self) -> &SchemaDigest {
        &self.schema_digest
    }

    pub fn provenance(&self) -> &TrustProvenance {
        &self.provenance
    }

    pub fn side_effect(&self) -> SideEffectClass {
        self.side_effect
    }

    /// Construct the implementation. This is the ONLY place the stored
    /// factory is invoked; the catalog itself never calls it.
    pub fn implementation(&self) -> Arc<dyn Tool> {
        (self.factory)()
    }
}

impl std::fmt::Debug for CapabilityRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapabilityRecord")
            .field("id", &self.id)
            .field("source", &self.source)
            .field("summary", &self.summary)
            .field("tags", &self.tags)
            .field("schema_digest", &self.schema_digest)
            .field("provenance", &self.provenance)
            .field("side_effect", &self.side_effect)
            .finish_non_exhaustive()
    }
}

/// Catalog identity of a registered tool: its exact runtime name for
/// extension tools is `<extension_id>:<tool_name>`, so the extension prefix
/// is stripped before encoding to keep the id keyed on (extension, tool).
pub(crate) fn tool_id_for(tool: &dyn Tool) -> ToolId {
    identity_for_tool(tool).0
}

fn identity_for_tool(tool: &dyn Tool) -> (ToolId, CapabilitySource, TrustProvenance) {
    match tool.origin() {
        ToolOrigin::Builtin => (
            ToolId::builtin(tool.name()),
            CapabilitySource::Builtin,
            TrustProvenance::BuiltinRuntime,
        ),
        ToolOrigin::Extension { extension_id } => {
            let runtime_name = tool.name();
            let tool_name = runtime_name
                .strip_prefix(&format!("{extension_id}:"))
                .unwrap_or(runtime_name);
            (
                ToolId::extension(&extension_id, tool_name),
                CapabilitySource::Extension {
                    extension_id: bounded_identity(&extension_id),
                    tool_name: bounded_identity(tool_name),
                },
                TrustProvenance::ExtensionManifest {
                    extension_id: bounded_identity(&extension_id),
                },
            )
        }
        ToolOrigin::Mcp {
            server_id,
            server_tool_name,
        } => (
            ToolId::mcp(&server_id, &server_tool_name),
            CapabilitySource::Mcp {
                server_id: bounded_identity(&server_id),
                server_tool_name: bounded_identity(&server_tool_name),
            },
            TrustProvenance::McpConfig {
                server_id: bounded_identity(&server_id),
            },
        ),
        ToolOrigin::Plugin {
            plugin_id,
            tool_name,
        } => (
            ToolId::plugin(&plugin_id, &tool_name),
            CapabilitySource::Plugin {
                plugin_id: bounded_identity(&plugin_id),
                tool_name: bounded_identity(&tool_name),
            },
            TrustProvenance::PluginConfig {
                plugin_id: bounded_identity(&plugin_id),
            },
        ),
        ToolOrigin::Unknown => (
            ToolId::unclassified(tool.name()),
            CapabilitySource::Unknown {
                runtime_name: bounded_identity(tool.name()),
            },
            TrustProvenance::Unverified,
        ),
    }
}

/// Typed catalog mutation failures. All fail closed without advancing the
/// catalog generation or partially applying the mutation.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CatalogError {
    #[error("duplicate tool id in catalog: {0}")]
    DuplicateToolId(ToolId),
    #[error("invalid tool id: {0}")]
    InvalidToolId(#[from] ToolIdError),
    #[error("catalog mutation rejected: {0}")]
    GenerationExhausted(#[from] CatalogGenerationOverflow),
}

/// Inventory of all locally known capabilities, keyed by stable [`ToolId`]
/// in deterministic order, with a generation that advances on every mutation.
#[derive(Clone, Debug)]
pub struct ToolCatalog {
    entries: BTreeMap<ToolId, CapabilityRecord>,
    generation: CatalogGeneration,
}

impl ToolCatalog {
    pub fn empty() -> Self {
        Self {
            entries: BTreeMap::new(),
            generation: CatalogGeneration::initial(),
        }
    }

    /// Boundary-test support: an empty catalog resumed at an explicit
    /// generation. Not used by production paths.
    #[doc(hidden)]
    pub fn resume_at_generation_for_tests(generation: CatalogGeneration) -> Self {
        Self {
            entries: BTreeMap::new(),
            generation,
        }
    }

    /// Boundary-test support: overwrite the generation counter in place.
    /// Grants nothing, exposes nothing, and mutates no entries; only the
    /// counter used by fail-closed advancement checks is changed.
    #[doc(hidden)]
    pub fn set_generation_for_tests(&mut self, generation: CatalogGeneration) {
        self.generation = generation;
    }

    /// Read-only snapshot of the catalog integrated into a live registry.
    ///
    /// Kept as a compatibility shim over [`super::ToolRegistry::catalog`];
    /// the registry maintains its catalog through every construction and
    /// mutation path, so this is a pure read with no process start, network
    /// access, schema exposure, or execution grant.
    pub fn from_registry(registry: &super::ToolRegistry) -> Result<Self, CatalogError> {
        Ok(registry.catalog().clone())
    }

    pub fn generation(&self) -> CatalogGeneration {
        self.generation
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, id: &ToolId) -> Option<&CapabilityRecord> {
        self.entries.get(id)
    }

    /// Iterate records in deterministic `ToolId` order.
    pub fn iter(&self) -> impl Iterator<Item = &CapabilityRecord> {
        self.entries.values()
    }

    /// Insert one capability. Duplicate IDs fail closed (no silent
    /// replacement, no generation advance); generation exhaustion fails
    /// closed (no entry change). On success the catalog generation advances
    /// by exactly one. The record's factory is stored, never invoked.
    pub fn insert(&mut self, record: CapabilityRecord) -> Result<(), CatalogError> {
        if self.entries.contains_key(&record.id) {
            return Err(CatalogError::DuplicateToolId(record.id.clone()));
        }
        let next = self.generation.checked_next()?;
        self.entries.insert(record.id.clone(), record);
        self.generation = next;
        Ok(())
    }

    /// Insert-or-replace one capability as a single mutation, optionally
    /// dropping the entry it shadows (registry replacement by runtime name
    /// can change the capability identity; the stale identity must not
    /// linger and keep validating old grants).
    ///
    /// Occupied-identity safety: if the incoming id is already cataloged and
    /// it is NOT the unchanged identity of the same-runtime-name tool being
    /// replaced (`replaced == Some(record.id)`), the mutation fails typed
    /// with [`CatalogError::DuplicateToolId`] before any entry or generation
    /// change — two distinct runtime names must never silently share one
    /// capability identity, or live tools and catalog would diverge. Fails
    /// closed without touching entries when no new generation is available.
    pub fn upsert(
        &mut self,
        replaced: Option<&ToolId>,
        record: CapabilityRecord,
    ) -> Result<(), CatalogError> {
        let same_identity_replacement = replaced == Some(&record.id);
        if !same_identity_replacement && self.entries.contains_key(&record.id) {
            return Err(CatalogError::DuplicateToolId(record.id.clone()));
        }
        let next = self.generation.checked_next()?;
        if let Some(stale) = replaced {
            if stale != &record.id {
                self.entries.remove(stale);
            }
        }
        self.entries.insert(record.id.clone(), record);
        self.generation = next;
        Ok(())
    }

    /// Advance strictly past both this catalog's generation and a prior
    /// registry generation. Used when a mutation rebuilds the catalog from
    /// scratch (e.g. `disable`): the rebuilt catalog must not reuse
    /// generation values already observed, or stale grants could survive.
    pub fn rebase_past(
        &mut self,
        prior: CatalogGeneration,
    ) -> Result<(), CatalogGenerationOverflow> {
        self.generation = self.generation.max(prior).checked_next()?;
        Ok(())
    }
}
