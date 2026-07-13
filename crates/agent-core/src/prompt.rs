use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use thiserror::Error;

pub const PROMPT_SCHEMA: &str = "synaps-prompt/1";
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;
pub const MAX_MODULE_BYTES: usize = 1024 * 1024;
pub const MAX_COMPOSED_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum PromptError {
    #[error("invalid prompt data: {0}")]
    Invalid(String),
    #[error("unknown prompt module: {0}")]
    UnknownReference(String),
    #[error("ambiguous prompt adapters: {0}")]
    Ambiguous(String),
}

fn valid_atom(value: &str) -> bool {
    !value.is_empty()
        && !value
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '/')
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PromptModuleId(String);
impl PromptModuleId {
    pub fn parse(value: impl Into<String>) -> Result<Self, PromptError> {
        let value = value.into();
        if value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(PromptError::Invalid("module id".into()));
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct QualifiedModelId(String);
impl TryFrom<String> for QualifiedModelId {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value).map_err(|error| error.to_string())
    }
}
impl From<QualifiedModelId> for String {
    fn from(value: QualifiedModelId) -> Self {
        value.0
    }
}
impl QualifiedModelId {
    pub fn parse(value: impl Into<String>) -> Result<Self, PromptError> {
        let value = value.into();
        let mut parts = value.split('/');
        let provider = parts.next().unwrap_or_default();
        let rest: Vec<_> = parts.collect();
        if !valid_atom(provider) || rest.is_empty() || rest.iter().any(|p| !valid_atom(p)) {
            return Err(PromptError::Invalid(
                "qualified model must contain non-empty provider/model segments".into(),
            ));
        }
        Ok(Self(value))
    }
    pub fn provider(&self) -> &str {
        self.0.split('/').next().unwrap()
    }
    pub fn model(&self) -> &str {
        &self.0[self.provider().len() + 1..]
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for QualifiedModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("QualifiedModelId").field(&self.0).finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelFamilyId(String);
impl ModelFamilyId {
    pub fn parse(value: impl Into<String>) -> Result<Self, PromptError> {
        let v = value.into();
        if !valid_atom(&v) {
            return Err(PromptError::Invalid("model family".into()));
        }
        Ok(Self(v))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug)]
pub struct SelectionContext {
    model: QualifiedModelId,
    family: Option<ModelFamilyId>,
}
impl SelectionContext {
    pub fn new(
        model: QualifiedModelId,
        family: Option<ModelFamilyId>,
    ) -> Result<Self, PromptError> {
        Ok(Self { model, family })
    }
    pub fn model(&self) -> &QualifiedModelId {
        &self.model
    }
    pub fn family(&self) -> Option<&ModelFamilyId> {
        self.family.as_ref()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptSelectors {
    provider: Option<String>,
    family: Option<ModelFamilyId>,
    exact: Option<QualifiedModelId>,
}
impl PromptSelectors {
    pub fn provider(v: impl Into<String>) -> Result<Self, PromptError> {
        let v = v.into();
        if !valid_atom(&v) {
            return Err(PromptError::Invalid("provider selector".into()));
        }
        Ok(Self {
            provider: Some(v),
            ..Self::default()
        })
    }
    pub fn family(v: ModelFamilyId) -> Self {
        Self {
            family: Some(v),
            ..Self::default()
        }
    }
    pub fn exact(v: QualifiedModelId) -> Self {
        Self {
            exact: Some(v),
            ..Self::default()
        }
    }
    pub fn provider_and_exact(
        provider: impl Into<String>,
        exact: QualifiedModelId,
    ) -> Result<Self, PromptError> {
        let provider = provider.into();
        if !valid_atom(&provider) || provider != exact.provider() {
            return Err(PromptError::Invalid(
                "provider/exact selector mismatch".into(),
            ));
        }
        Ok(Self {
            provider: Some(provider),
            exact: Some(exact),
            family: None,
        })
    }
    fn validate(&self) -> Result<(), PromptError> {
        if let Some(p) = &self.provider {
            if !valid_atom(p) {
                return Err(PromptError::Invalid("provider selector".into()));
            }
            if self.exact.as_ref().is_some_and(|e| e.provider() != p) {
                return Err(PromptError::Invalid(
                    "provider/exact selector mismatch".into(),
                ));
            }
        }
        Ok(())
    }
    fn layer(&self) -> u8 {
        if self.exact.is_some() {
            3
        } else if self.family.is_some() {
            2
        } else if self.provider.is_some() {
            1
        } else {
            0
        }
    }
    fn matches(&self, c: &SelectionContext) -> bool {
        self.provider
            .as_deref()
            .map_or(true, |p| p == c.model.provider())
            && self
                .family
                .as_ref()
                .map_or(true, |f| c.family.as_ref() == Some(f))
            && self.exact.as_ref().map_or(true, |e| e == &c.model)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptModuleSource {
    Builtin,
    User,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleMutability {
    ImmutablePolicy,
    MutableGuidance,
}

#[derive(Clone)]
pub struct PromptModule {
    pub id: PromptModuleId,
    pub version: String,
    pub source: PromptModuleSource,
    pub priority: u16,
    pub selectors: PromptSelectors,
    pub mutability: ModuleMutability,
    content: String,
    pub sha256: String,
    safe_path: Option<String>,
}
impl fmt::Debug for PromptModule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PromptModule")
            .field("id", &self.id)
            .field("version", &self.version)
            .field("sha256", &self.sha256)
            .field("byte_count", &self.content.len())
            .finish_non_exhaustive()
    }
}
impl PromptModule {
    pub fn new(
        id: PromptModuleId,
        version: impl Into<String>,
        source: PromptModuleSource,
        priority: u16,
        selectors: PromptSelectors,
        mutability: ModuleMutability,
        content: impl Into<String>,
    ) -> Result<Self, PromptError> {
        selectors.validate()?;
        let content = content.into();
        if content.len() > MAX_MODULE_BYTES {
            return Err(PromptError::Invalid("module exceeds size limit".into()));
        }
        let sha256 = format!("{:x}", Sha256::digest(content.as_bytes()));
        Ok(Self {
            id,
            version: version.into(),
            source,
            priority,
            selectors,
            mutability,
            content,
            sha256,
            safe_path: None,
        })
    }
    pub fn content(&self) -> &str {
        &self.content
    }
}

pub struct AdapterRegistry {
    modules: BTreeMap<PromptModuleId, PromptModule>,
}
impl AdapterRegistry {
    pub fn new(modules: Vec<PromptModule>) -> Result<Self, PromptError> {
        let mut map = BTreeMap::new();
        for m in modules {
            if map.insert(m.id.clone(), m).is_some() {
                return Err(PromptError::Invalid("duplicate module id".into()));
            }
        }
        Ok(Self { modules: map })
    }
    pub fn select(&self, c: &SelectionContext) -> Result<Vec<&PromptModule>, PromptError> {
        let mut v: Vec<_> = self
            .modules
            .values()
            .filter(|m| m.selectors.matches(c) && m.selectors.layer() > 0)
            .collect();
        v.sort_by_key(|m| (m.selectors.layer(), m.priority, m.id.as_str()));
        for w in v.windows(2) {
            if w[0].selectors.layer() == w[1].selectors.layer() && w[0].priority == w[1].priority {
                return Err(PromptError::Ambiguous(format!(
                    "{} and {}",
                    w[0].id.as_str(),
                    w[1].id.as_str()
                )));
            }
        }
        Ok(v)
    }
    fn get(&self, id: &str) -> Result<&PromptModule, PromptError> {
        self.modules
            .values()
            .find(|m| m.id.as_str() == id)
            .ok_or_else(|| PromptError::UnknownReference(id.into()))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptManifest {
    schema: String,
    kernel: String,
    #[serde(default)]
    adapters: Vec<String>,
    #[serde(default)]
    modules: Vec<ModuleDeclaration>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModuleDeclaration {
    id: PromptModuleId,
    version: String,
    source: PromptModuleSource,
    #[serde(default)]
    path: Option<String>,
    priority: u16,
    selectors: PromptSelectors,
    mutability: ModuleMutability,
    #[serde(default)]
    content: Option<String>,
}
impl PromptManifest {
    pub fn parse(input: &str) -> Result<Self, PromptError> {
        if input.len() > MAX_MANIFEST_BYTES {
            return Err(PromptError::Invalid("manifest exceeds size limit".into()));
        }
        let v: Self =
            serde_yaml::from_str(input).map_err(|e| PromptError::Invalid(e.to_string()))?;
        if v.schema != PROMPT_SCHEMA {
            return Err(PromptError::Invalid("unsupported schema".into()));
        }
        if v.kernel.is_empty() {
            return Err(PromptError::Invalid("kernel is required".into()));
        }
        let mut ids = BTreeSet::new();
        for m in &v.modules {
            m.selectors.validate()?;
            if !ids.insert(&m.id) {
                return Err(PromptError::Invalid("duplicate module id".into()));
            }
            if m.content
                .as_ref()
                .is_some_and(|content| content.len() > MAX_MODULE_BYTES)
            {
                return Err(PromptError::Invalid("module exceeds size limit".into()));
            }
            if m.path.is_some() == m.content.is_some() {
                return Err(PromptError::Invalid(format!(
                    "module {} must declare exactly one of path or content",
                    m.id.as_str()
                )));
            }
        }
        Ok(v)
    }
    pub fn registry(
        &self,
        manifest_dir: Option<&std::path::Path>,
    ) -> Result<AdapterRegistry, PromptError> {
        let mut modules = Vec::with_capacity(self.modules.len());
        for declaration in &self.modules {
            let (content, safe_path) = if let Some(content) = &declaration.content {
                (content.clone(), None)
            } else {
                use std::path::Component;
                let relative = std::path::Path::new(declaration.path.as_deref().unwrap());
                if relative.is_absolute()
                    || relative.components().any(|component| {
                        !matches!(component, Component::Normal(_) | Component::CurDir)
                    })
                {
                    return Err(PromptError::Invalid(
                        "module path must be confined to manifest directory".into(),
                    ));
                }
                let base = manifest_dir.ok_or_else(|| {
                    PromptError::Invalid("manifest directory is required for module paths".into())
                })?;
                let canonical_base = base.canonicalize().map_err(|_| {
                    PromptError::Invalid("manifest directory is unavailable".into())
                })?;
                let candidate = base.join(relative);
                let canonical_candidate = candidate
                    .canonicalize()
                    .map_err(|_| PromptError::Invalid("module path is unavailable".into()))?;
                if !canonical_candidate.starts_with(&canonical_base) {
                    return Err(PromptError::Invalid(
                        "module path must be confined to manifest directory".into(),
                    ));
                }
                let content = std::fs::read_to_string(&canonical_candidate).map_err(|_| {
                    PromptError::Invalid(format!(
                        "module {} could not be read",
                        declaration.id.as_str()
                    ))
                })?;
                (content, Some(relative.to_string_lossy().into_owned()))
            };
            let mut module = PromptModule::new(
                declaration.id.clone(),
                declaration.version.clone(),
                declaration.source.clone(),
                declaration.priority,
                declaration.selectors.clone(),
                declaration.mutability.clone(),
                content,
            )?;
            module.safe_path = safe_path;
            modules.push(module);
        }
        AdapterRegistry::new(modules)
    }
    pub fn schema(&self) -> &str {
        &self.schema
    }
    pub fn validate_references<'a>(
        &self,
        known: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), PromptError> {
        let k: BTreeSet<_> = known.into_iter().collect();
        for id in std::iter::once(&self.kernel).chain(&self.adapters) {
            if !k.contains(id.as_str()) {
                return Err(PromptError::UnknownReference(id.clone()));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PromptProvenance {
    pub prompt_schema: String,
    pub prompt_stack: Vec<PromptProvenanceModule>,
    pub delegation_policy_digest: String,
    pub foreground_model: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PromptProvenanceModule {
    pub id: String,
    pub version: String,
    pub sha256: String,
}

pub struct PromptStack {
    modules: Vec<PromptModule>,
    context: SelectionContext,
    composed: String,
}
impl fmt::Debug for PromptStack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PromptStack")
            .field("modules", &self.modules)
            .field("context", &self.context)
            .field("byte_count", &self.composed.len())
            .finish()
    }
}
#[derive(Serialize)]
pub struct PromptInspection<'a> {
    schema: &'static str,
    foreground_model: &'a str,
    enforcement_state: &'static str,
    byte_count: usize,
    modules: Vec<PromptModuleMetadata<'a>>,
}
#[derive(Serialize)]
pub struct PromptModuleMetadata<'a> {
    id: &'a PromptModuleId,
    version: &'a str,
    sha256: &'a str,
    selectors: &'a PromptSelectors,
    mutability: &'a ModuleMutability,
    source: &'a PromptModuleSource,
    safe_source: Option<&'a str>,
    byte_count: usize,
}
impl PromptStack {
    pub fn new(modules: Vec<PromptModule>, context: SelectionContext) -> Result<Self, PromptError> {
        let composed = modules
            .iter()
            .map(|m| m.content())
            .collect::<Vec<_>>()
            .join("\n");
        if composed.len() > MAX_COMPOSED_BYTES {
            return Err(PromptError::Invalid(
                "composed prompt exceeds size limit".into(),
            ));
        }
        Ok(Self {
            modules,
            context,
            composed,
        })
    }
    pub fn modules(&self) -> &[PromptModule] {
        &self.modules
    }
    pub fn composed(&self) -> &str {
        &self.composed
    }
    pub fn provenance(&self, delegation_policy_digest: impl Into<String>) -> PromptProvenance {
        PromptProvenance {
            prompt_schema: PROMPT_SCHEMA.into(),
            prompt_stack: self
                .modules
                .iter()
                .map(|module| PromptProvenanceModule {
                    id: module.id.as_str().into(),
                    version: module.version.clone(),
                    sha256: module.sha256.clone(),
                })
                .collect(),
            delegation_policy_digest: delegation_policy_digest.into(),
            foreground_model: self.context.model.as_str().into(),
        }
    }
    pub fn inspect(&self) -> PromptInspection<'_> {
        PromptInspection {
            schema: PROMPT_SCHEMA,
            foreground_model: self.context.model.as_str(),
            enforcement_state: "advisory",
            byte_count: self.composed.len(),
            modules: self
                .modules
                .iter()
                .map(|m| PromptModuleMetadata {
                    id: &m.id,
                    version: &m.version,
                    sha256: &m.sha256,
                    selectors: &m.selectors,
                    mutability: &m.mutability,
                    source: &m.source,
                    safe_source: m.safe_path.as_deref(),
                    byte_count: m.content.len(),
                })
                .collect(),
        }
    }
}

pub fn compile_prompt_stack(
    manifest: &PromptManifest,
    registry: &AdapterRegistry,
    context: &SelectionContext,
    user: Option<PromptModule>,
) -> Result<PromptStack, PromptError> {
    let mut out = vec![registry.get(&manifest.kernel)?.clone()];
    let requested: BTreeSet<_> = manifest.adapters.iter().map(String::as_str).collect();
    let selected = registry.select(context)?;
    for id in &requested {
        let module = registry.get(id)?;
        if !selected.iter().any(|candidate| candidate.id == module.id) {
            return Err(PromptError::Invalid(format!(
                "requested adapter {id} does not match selection context"
            )));
        }
    }
    for m in selected {
        if requested.contains(m.id.as_str()) {
            out.push(m.clone())
        }
    }
    if let Some(u) = user {
        if out.iter().any(|m| m.id == u.id) {
            return Err(PromptError::Invalid("duplicate module id".into()));
        }
        out.push(u)
    }
    PromptStack::new(out, context.clone())
}
pub fn resolved_system_prompt_as_user_module(
    content: impl Into<String>,
) -> Result<PromptModule, PromptError> {
    PromptModule::new(
        PromptModuleId::parse("user.system")?,
        "legacy",
        PromptModuleSource::User,
        u16::MAX,
        PromptSelectors::default(),
        ModuleMutability::MutableGuidance,
        content,
    )
}
pub fn resolve_system_prompt_module(content: impl Into<String>) -> PromptModule {
    resolved_system_prompt_as_user_module(content).expect("legacy prompt is within size limit")
}
