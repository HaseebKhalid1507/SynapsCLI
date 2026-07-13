use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use thiserror::Error;

pub const PROMPT_SCHEMA: &str = "synaps-prompt/1";

#[derive(Debug, Error)]
pub enum PromptError {
    #[error("invalid prompt data: {0}")]
    Invalid(String),
    #[error("unknown prompt module: {0}")]
    UnknownReference(String),
    #[error("ambiguous prompt adapters: {0}")]
    Ambiguous(String),
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize)]
pub struct PromptModuleId(String);
impl PromptModuleId {
    pub fn parse(value: impl Into<String>) -> Result<Self, PromptError> {
        let value = value.into();
        if value.trim().is_empty() || value.contains(char::is_whitespace) {
            return Err(PromptError::Invalid("module id".into()));
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QualifiedModelId {
    raw: String,
    slash: usize,
}
impl QualifiedModelId {
    pub fn parse(value: impl Into<String>) -> Result<Self, PromptError> {
        let raw = value.into();
        let slash = raw.find('/').ok_or_else(|| {
            PromptError::Invalid("qualified model must contain provider/model".into())
        })?;
        if slash == 0 || slash + 1 == raw.len() {
            return Err(PromptError::Invalid(
                "qualified model has an empty component".into(),
            ));
        }
        Ok(Self { raw, slash })
    }
    pub fn provider(&self) -> &str {
        &self.raw[..self.slash]
    }
    pub fn model(&self) -> &str {
        &self.raw[self.slash + 1..]
    }
    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

#[derive(Clone, Debug, Default)]
pub struct PromptSelectors {
    provider: Option<String>,
    family: Option<String>,
    exact: Option<QualifiedModelId>,
}
impl PromptSelectors {
    pub fn provider(value: impl Into<String>) -> Self {
        Self {
            provider: Some(value.into()),
            ..Self::default()
        }
    }
    pub fn family(value: impl Into<String>) -> Self {
        Self {
            family: Some(value.into()),
            ..Self::default()
        }
    }
    pub fn exact(value: QualifiedModelId) -> Self {
        Self {
            exact: Some(value),
            ..Self::default()
        }
    }
    fn specificity(&self) -> u8 {
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
    fn matches(&self, target: &QualifiedModelId) -> bool {
        self.exact.as_ref().map_or(true, |v| v == target)
            && self
                .provider
                .as_deref()
                .map_or(true, |v| v == target.provider())
            && self
                .family
                .as_deref()
                .map_or(true, |v| target.model().split('/').next() == Some(v))
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptModuleSource {
    Builtin,
    User,
}
#[derive(Clone, Debug)]
pub enum ModuleMutability {
    ImmutablePolicy,
    MutableGuidance,
}
#[derive(Clone, Debug)]
pub struct PromptModule {
    pub id: PromptModuleId,
    pub version: String,
    pub source: PromptModuleSource,
    pub priority: u16,
    pub selectors: PromptSelectors,
    pub mutability: ModuleMutability,
    pub content: String,
    pub sha256: String,
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
    ) -> Self {
        let content = content.into();
        let sha256 = format!("{:x}", Sha256::digest(content.as_bytes()));
        Self {
            id,
            version: version.into(),
            source,
            priority,
            selectors,
            mutability,
            content,
            sha256,
        }
    }
}

pub struct AdapterRegistry {
    modules: Vec<PromptModule>,
}
impl AdapterRegistry {
    pub fn new(modules: Vec<PromptModule>) -> Self {
        Self { modules }
    }
    pub fn select(&self, target: &QualifiedModelId) -> Result<Vec<&PromptModule>, PromptError> {
        let mut selected: Vec<_> = self
            .modules
            .iter()
            .filter(|m| m.selectors.matches(target))
            .collect();
        selected.sort_by_key(|m| (m.selectors.specificity(), m.priority, m.id.as_str()));
        for pair in selected.windows(2) {
            if pair[0].selectors.specificity() == pair[1].selectors.specificity()
                && pair[0].priority == pair[1].priority
            {
                return Err(PromptError::Ambiguous(format!(
                    "{} and {}",
                    pair[0].id.as_str(),
                    pair[1].id.as_str()
                )));
            }
        }
        Ok(selected)
    }
}

#[derive(Debug, Deserialize)]
pub struct PromptManifest {
    schema: String,
    kernel: String,
    #[serde(default)]
    adapters: Vec<String>,
}
impl PromptManifest {
    pub fn parse(input: &str) -> Result<Self, PromptError> {
        let value: Self =
            serde_json::from_str(input).map_err(|e| PromptError::Invalid(e.to_string()))?;
        if value.schema != PROMPT_SCHEMA {
            return Err(PromptError::Invalid(format!(
                "unsupported schema {}",
                value.schema
            )));
        }
        if value.kernel.is_empty() {
            return Err(PromptError::Invalid("kernel is required".into()));
        }
        Ok(value)
    }
    pub fn schema(&self) -> &str {
        &self.schema
    }
    pub fn validate_references<'a>(
        &self,
        known: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), PromptError> {
        let known: BTreeSet<_> = known.into_iter().collect();
        for id in std::iter::once(&self.kernel).chain(self.adapters.iter()) {
            if !known.contains(id.as_str()) {
                return Err(PromptError::UnknownReference(id.clone()));
            }
        }
        Ok(())
    }
}

pub struct PromptStack {
    modules: Vec<PromptModule>,
}
#[derive(Serialize)]
pub struct PromptModuleMetadata<'a> {
    id: &'a PromptModuleId,
    version: &'a str,
    source: &'a PromptModuleSource,
    sha256: &'a str,
}
impl PromptStack {
    pub fn new(modules: Vec<PromptModule>) -> Self {
        Self { modules }
    }
    pub fn modules(&self) -> &[PromptModule] {
        &self.modules
    }
    pub fn inspect(&self) -> Vec<PromptModuleMetadata<'_>> {
        self.modules
            .iter()
            .map(|m| PromptModuleMetadata {
                id: &m.id,
                version: &m.version,
                source: &m.source,
                sha256: &m.sha256,
            })
            .collect()
    }
}

pub fn resolve_system_prompt_module(content: impl Into<String>) -> PromptModule {
    PromptModule::new(
        PromptModuleId::parse("user.system").unwrap(),
        "legacy",
        PromptModuleSource::User,
        u16::MAX,
        PromptSelectors::default(),
        ModuleMutability::MutableGuidance,
        content,
    )
}
