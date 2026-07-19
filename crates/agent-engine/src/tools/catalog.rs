//! `ToolCatalog` — all locally known capabilities (spec §7.1, Task 14).
//!
//! The catalog is a passive, additive inventory: it records what capabilities
//! exist locally (stable ID, source, compact descriptor, schema + digest,
//! implementation factory, trust provenance, side-effect placeholder) without
//! changing what is exposed or executable. [`super::ToolRegistry`] remains the
//! active behavior projection until later tasks introduce `DiscoveryIndex`,
//! `SessionToolSet`, and `ExecutionGate`.
//!
//! Insertion invariants (spec §4.2):
//! - no implementation is constructed (factories are stored, never invoked);
//! - no process is started and no network is touched;
//! - no schema is exposed to the model;
//! - no execution grant is issued.

use crate::tools::{Tool, ToolRegistry};
use agent_core::BoundedText;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use thiserror::Error;

pub use agent_core::orchestration::capability::{
    CatalogGeneration, SchemaDigest, SessionActivationGrant, ToolId, ToolIdError,
};

/// Deferred implementation constructor. Stored at insertion, invoked only by
/// explicit later acquisition (execution-gate territory, not catalog).
pub type ToolFactory = Arc<dyn Fn() -> Arc<dyn Tool> + Send + Sync>;

/// Where a capability comes from. Drives the ID namespace and, later,
/// per-source permission policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapabilitySource {
    Builtin,
    Extension { extension_id: String },
    Mcp { server_id: String },
    Plugin { plugin_id: String },
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

/// Typed catalog mutation failures. All fail closed without advancing the
/// catalog generation.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CatalogError {
    #[error("duplicate tool id in catalog: {0}")]
    DuplicateToolId(ToolId),
    #[error("invalid tool id: {0}")]
    InvalidToolId(#[from] ToolIdError),
}

/// Inventory of all locally known capabilities, keyed by stable [`ToolId`]
/// in deterministic order, with a generation that advances on every mutation.
#[derive(Debug)]
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

    /// Catalog every tool already constructed by an existing registry
    /// construction path (`ToolRegistry::new()` and variants). Purely reads
    /// in-process metadata: no process start, no network, no schema exposure,
    /// no execution grant. Non-canonical runtime names fail closed rather
    /// than being sanitized into alias-prone IDs.
    pub fn from_registry(registry: &ToolRegistry) -> Result<Self, CatalogError> {
        let mut catalog = Self::empty();
        for tool in registry.iter_tools_sorted() {
            let (source, provenance, id_raw) = match tool.extension_id() {
                Some(extension_id) => (
                    CapabilitySource::Extension {
                        extension_id: extension_id.to_string(),
                    },
                    TrustProvenance::ExtensionManifest {
                        extension_id: extension_id.to_string(),
                    },
                    format!("extension.{extension_id}:{}", tool.name()),
                ),
                None => (
                    CapabilitySource::Builtin,
                    TrustProvenance::BuiltinRuntime,
                    format!("builtin:{}", tool.name()),
                ),
            };
            let id = ToolId::parse(&id_raw)?;
            let implementation = Arc::clone(tool);
            let factory: ToolFactory = Arc::new(move || Arc::clone(&implementation));
            let record = CapabilityRecord::new(
                id,
                source,
                tool.description(),
                Vec::new(),
                SchemaLocator::Inline(tool.parameters()),
                factory,
                provenance,
            );
            catalog.insert(record)?;
        }
        Ok(catalog)
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
    /// replacement, no generation advance). On success the catalog
    /// generation increments by exactly one. The record's factory is stored,
    /// never invoked.
    pub fn insert(&mut self, record: CapabilityRecord) -> Result<(), CatalogError> {
        if self.entries.contains_key(&record.id) {
            return Err(CatalogError::DuplicateToolId(record.id.clone()));
        }
        self.entries.insert(record.id.clone(), record);
        self.generation = self.generation.next();
        Ok(())
    }
}
