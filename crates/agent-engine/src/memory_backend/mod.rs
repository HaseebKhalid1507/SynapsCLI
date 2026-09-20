//! Host-owned exclusive memory routing. Backend selection is immutable per
//! binding; an Axel failure never opens the legacy note store. Configuration
//! selects a process, never a shell command or a model-controlled namespace.
use crate::{Result, RuntimeError};
use agent_core::config::{MemoryBackendConfig, MemoryBackendKind};
use agent_core::memory::forum::Author;
use agent_core::memory::store::{
    self, MemoryDescriptor, MemoryRecord, MemorySensitivity, NewMemoryRecord, ProjectMemoryQuery,
    ProjectScope,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
pub mod forum;
mod history;
pub mod migration;
mod process;
mod provisioning;
pub mod repository_migration;

pub fn competing_note_tool(name: &str) -> bool {
    matches!(
        name,
        "memory_store"
            | "memory_search"
            | "memory_fetch"
            | "memory_forget"
            | "memory_capture"
            | "memory_recall"
    )
}

pub const CONTRACT: &str = "synaps-axel/2";
pub const USER_SCOPE: &str = "p0000000000000000";
pub const AXEL_REVISION: &str = "edbdea401d66feedb87fcad28c879ece54e3ccd2";

#[derive(Clone)]
pub struct MemoryBinding(Arc<Binding>);
impl std::fmt::Debug for MemoryBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryBinding")
            .field("exclusive", &self.exclusive())
            .finish_non_exhaustive()
    }
}
#[derive(Clone)]
struct Binding {
    base: PathBuf,
    scope: std::result::Result<ProjectScope, String>,
    author: Author,
    operation_lock: Arc<tokio::sync::Mutex<()>>,
    repository: Option<agent_core::memory::repository::RepositoryIdentity>,
    user_scope_enabled: bool,
    managed_parent: bool,
    backend: Backend,
}
#[derive(Clone)]
enum Backend {
    Legacy,
    Axel { executable: PathBuf, brain: PathBuf },
    Unavailable,
}
fn error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::Tool(format!("memory: {}", message.into()))
}

impl MemoryBinding {
    /// Production bootstrap without a Runtime capability (legacy extensions).
    /// Selection is captured before any subprocess starts; never flips to legacy
    /// when an explicitly selected Axel service is unavailable.
    pub fn configured_current() -> Self {
        Self::from_config(&agent_core::config::load_config().memory_backend)
    }
    pub fn legacy_current() -> Self {
        Self::from_config(&MemoryBackendConfig::default())
    }
    pub fn from_config(config: &MemoryBackendConfig) -> Self {
        let base = agent_core::config::base_dir();
        let cwd = std::env::current_dir();
        let repository = if config.kind == MemoryBackendKind::Axel {
            cwd.as_ref()
                .ok()
                .map(|cwd| ProjectScope::discover_repository(cwd))
        } else {
            None
        };
        let scope = match &repository {
            Some(Ok(identity)) => Ok(identity.scope.clone()),
            Some(Err(_)) => {
                Err("repository memory identity unavailable; no path-scope fallback".into())
            }
            None => cwd
                .map_err(|_| "workspace unavailable".to_owned())
                .and_then(|cwd| {
                    ProjectScope::discover(&cwd).map_err(|_| "project scope unavailable".to_owned())
                }),
        };
        let mut binding = Self::new(base, scope, config);
        Arc::get_mut(&mut binding.0)
            .expect("new binding")
            .repository = repository.and_then(std::result::Result::ok);
        binding
    }
    fn new(
        base: PathBuf,
        scope: std::result::Result<ProjectScope, String>,
        config: &MemoryBackendConfig,
    ) -> Self {
        let backend = match config.kind {
            MemoryBackendKind::Legacy => Backend::Legacy,
            MemoryBackendKind::Axel => {
                let executable = config.executable.clone().or_else(|| {
                    std::env::current_exe().ok().and_then(|path| {
                        path.parent()
                            .map(|parent| parent.join("synaps-axel-memory-service"))
                    })
                });
                let brain = config
                    .brain
                    .clone()
                    .unwrap_or_else(|| base.join("memory/axel/brain.r8"));
                match executable {
                    Some(executable) if executable.is_absolute() && brain.is_absolute() => {
                        Backend::Axel { executable, brain }
                    }
                    _ => Backend::Unavailable,
                }
            }
            MemoryBackendKind::Unavailable => Backend::Unavailable,
        };
        Self(Arc::new(Binding {
            base,
            scope,
            author: Author::fresh(),
            operation_lock: Arc::new(tokio::sync::Mutex::new(())),
            repository: None,
            user_scope_enabled: config.user_scope,
            managed_parent: config.brain.is_none(),
            backend,
        }))
    }
    /// Captured host identity; metadata only. Never grants an alias merge.
    pub fn repository_identity(
        &self,
    ) -> Result<agent_core::memory::repository::RepositoryIdentity> {
        self.0
            .repository
            .clone()
            .ok_or_else(|| error("repository identity unavailable"))
    }
    /// Immutable execution provenance, not a saved conversation identity or
    /// an authorization grant. Ordinary clones retain this exact author.
    pub fn forum_author(&self) -> &Author {
        &self.0.author
    }
    /// New worker execution: fork only the author, retaining captured storage
    /// authority (including scope errors) and the shared operation lock.
    /// Resumed workers get a new actor too; no rediscovery or config reload.
    pub fn fork_for_worker(&self) -> Self {
        Self(Arc::new(Binding {
            author: self.0.author.child(),
            ..(*self.0).clone()
        }))
    }
    /// Operator-only rebinding for exact verified source scopes during migration.
    /// Never expose an arbitrary project selector through model tools.
    pub fn with_scope(&self, scope: ProjectScope) -> Self {
        Self(Arc::new(Binding {
            scope: Ok(scope),
            ..(*self.0).clone()
        }))
    }
    /// Only explicit note tools may select the reserved user scope. Capture,
    /// history, and automatic recall retain their repository binding.
    pub fn for_user_notes(&self) -> Result<Self> {
        if !self.is_axel() || !self.0.user_scope_enabled {
            return Err(error("user-wide notes require Axel and explicit memory.user_scope = true; no automatic sharing"));
        }
        let scope =
            ProjectScope::user_scope(&self.0.base).map_err(|_| error("user scope unavailable"))?;
        Ok(self.with_scope(scope))
    }
    pub fn brain_path(&self) -> Option<&Path> {
        match &self.0.backend {
            Backend::Axel { brain, .. } => Some(brain),
            _ => None,
        }
    }
    pub fn is_axel(&self) -> bool {
        matches!(self.0.backend, Backend::Axel { .. }) && self.0.scope.is_ok()
    }
    /// Pure configuration validation for consent grants; does not open storage
    /// or start a process. Execution performs stronger filesystem checks again.
    pub fn validate_axel_config(&self) -> Result<()> {
        match &self.0.backend {
            Backend::Axel { executable, brain } if self.0.scope.is_ok() => {
                process::validate_configuration(executable, brain)
            }
            _ => Err(error("Axel backend is not configured; no fallback")),
        }
    }
    pub fn exclusive(&self) -> bool {
        !matches!(self.0.backend, Backend::Legacy)
    }
    pub fn scope(&self) -> Result<&ProjectScope> {
        self.0
            .scope
            .as_ref()
            .map_err(|_| error("host project scope unavailable"))
    }
    pub fn base(&self) -> &Path {
        &self.0.base
    }
    /// Exact host-only RPC, never selected by model-authored paths or projects.
    /// Operator migration/export calls use this same backend and transaction boundary.
    pub async fn rpc(&self, operation: &str, payload: Value) -> Result<Value> {
        self.call(operation, payload).await
    }
    /// Atomically import an operator-approved source manifest into this one backend.
    /// The migration module owns preview/consent/source validation before this call.
    pub async fn migration_apply(&self, payload: Value) -> Result<Value> {
        self.rpc("migration_apply", payload).await
    }
    /// Query feature availability without touching any legacy memory surface.
    pub async fn capabilities(&self) -> Result<Value> {
        self.rpc("capabilities", json!({})).await
    }
    async fn call(&self, operation: &str, payload: Value) -> Result<Value> {
        let _guard = self.0.operation_lock.lock().await;
        let scope = self.scope()?;
        if scope.key() == USER_SCOPE
            && !matches!(
                operation,
                "store" | "search" | "fetch" | "forget" | "scope_info" | "capabilities"
            )
        {
            return Err(error("user-wide scope supports explicit notes only"));
        }
        match &self.0.backend {
            Backend::Axel { executable, brain } => {
                // Invalid executable/configuration must not provision storage.
                process::validate_configuration(executable, brain)?;
                if self.0.managed_parent
                    && !matches!(
                        operation,
                        "capabilities" | "legacy_export" | "native_export" | "upgrade_preview"
                    )
                {
                    let base = self.0.base.clone();
                    let brain = brain.clone();
                    tokio::task::spawn_blocking(move || {
                        provisioning::ensure_default_parent(&base, &brain)
                    })
                    .await
                    .map_err(|_| error("Axel parent provisioning worker failed"))??;
                }
                process::call(executable, brain, scope.key(), operation, payload).await
            }
            _ => Err(error("selected backend unavailable; no legacy fallback")),
        }
    }
    /// Validate original source scopes without rewriting their provenance. Only
    /// the selected service's operator-established membership can widen reads.
    async fn authorized_projects(&self, projects: &[&str]) -> Result<Vec<String>> {
        let current = self.scope()?.key();
        if projects.iter().all(|project| *project == current) {
            return Ok(vec![current.to_owned()]);
        }
        let info = self.call("scope_info", json!({})).await?;
        let members: Vec<String> = serde_json::from_value(info["members"].clone())
            .map_err(|_| error("invalid backend scope membership"))?;
        let canonical = info["canonical_project"]
            .as_str()
            .ok_or_else(|| error("missing backend canonical scope"))?;
        let valid = |key: &str| {
            key.len() == 17
                && key.starts_with('p')
                && key[1..]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        if members.is_empty()
            || members.len() > 4096
            || !valid(canonical)
            || !members.iter().all(|key| valid(key))
            || !members.iter().any(|key| key == current)
            || !members.iter().any(|key| key == canonical)
            || !projects
                .iter()
                .all(|project| members.iter().any(|member| member == project))
            || (members.iter().any(|key| key == USER_SCOPE)
                && (members.len() != 1 || current != USER_SCOPE))
        {
            return Err(error("backend scope membership violates isolation"));
        }
        Ok(members)
    }
    pub async fn search(&self, q: ProjectMemoryQuery) -> Result<Vec<MemoryDescriptor>> {
        let scope = self.scope()?.clone();
        if matches!(self.0.backend, Backend::Legacy) {
            let base = self.0.base.clone();
            return tokio::task::spawn_blocking(move || {
                store::search_project_in(&base, &scope, &q)
            })
            .await
            .map_err(|_| error("legacy search worker failed"))?
            .map_err(|e| error(e.to_string()));
        }
        let limit = q
            .limit
            .unwrap_or(store::DEFAULT_SEARCH_LIMIT)
            .min(store::MAX_SEARCH_LIMIT);
        let snippet_bytes = q
            .snippet_bytes
            .unwrap_or(store::DEFAULT_SNIPPET_BYTES)
            .min(store::MAX_SNIPPET_BYTES);
        let value = self.call("search", json!({"content_contains":q.content_contains,"tag_prefix":q.tag_prefix,"since_ms":q.since_ms,"until_ms":q.until_ms,"limit":limit,"snippet_bytes":snippet_bytes})).await?;
        let rows = value
            .as_array()
            .ok_or_else(|| error("invalid backend search response"))?;
        if rows.len() > limit {
            return Err(error("backend exceeded search bound"));
        }
        let source_projects = rows
            .iter()
            .map(|row| row["project"].as_str().unwrap_or(""))
            .collect::<Vec<_>>();
        let members = self.authorized_projects(&source_projects).await?;
        rows.iter()
            .map(|row| {
                let d: Descriptor = serde_json::from_value(row.clone())
                    .map_err(|_| error("invalid backend descriptor"))?;
                if !members.contains(&d.project)
                    || d.id.is_empty()
                    || d.id.len() > 128
                    || d.snippet.len() > snippet_bytes
                    || (d.sensitivity == MemorySensitivity::Secret && !d.snippet.is_empty())
                {
                    return Err(error("backend descriptor violates scope/disclosure/bounds"));
                }
                Ok(MemoryDescriptor {
                    id: d.id,
                    project: d.project,
                    timestamp_ms: d.timestamp_ms,
                    tags: d.tags,
                    snippet: d.snippet,
                    truncated: d.truncated,
                    content_bytes: d.content_bytes,
                    sensitivity: d.sensitivity,
                    retention: d.retention,
                })
            })
            .collect()
    }
    pub async fn fetch(&self, ids: &[&str]) -> Result<Vec<MemoryRecord>> {
        let scope = self.scope()?.clone();
        if ids.is_empty() || ids.len() > 25 || ids.iter().any(|id| id.is_empty() || id.len() > 128)
        {
            return Err(error("fetch requires 1..=25 bounded exact IDs"));
        }
        if matches!(self.0.backend, Backend::Legacy) {
            let base = self.0.base.clone();
            let ids = ids.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            return tokio::task::spawn_blocking(move || {
                store::fetch_exact_in(
                    &base,
                    &scope,
                    &ids.iter().map(String::as_str).collect::<Vec<_>>(),
                )
            })
            .await
            .map_err(|_| error("legacy fetch worker failed"))?
            .map_err(|e| error(e.to_string()));
        }
        let records: Vec<MemoryRecord> =
            serde_json::from_value(self.call("fetch", json!({"ids":ids})).await?)
                .map_err(|_| error("invalid backend fetch response"))?;
        if records.len() != ids.len() {
            return Err(error("backend fetch is incomplete"));
        }
        let source_projects = records
            .iter()
            .map(|record| record.project.as_deref().unwrap_or(""))
            .collect::<Vec<_>>();
        let members = self.authorized_projects(&source_projects).await?;
        for (record, id) in records.iter().zip(ids) {
            validate_record_members(record, &members, id)?;
        }
        Ok(records)
    }
    pub async fn store(&self, new: NewMemoryRecord) -> Result<MemoryRecord> {
        let scope = self.scope()?.clone();
        if new.content.len() > store::MAX_CONTENT_BYTES {
            return Err(error("content exceeds 16384 bytes"));
        }
        if matches!(self.0.backend, Backend::Legacy) {
            let base = self.0.base.clone();
            return tokio::task::spawn_blocking(move || store::store_record_in(&base, &scope, new))
                .await
                .map_err(|_| error("legacy store worker failed"))?
                .map_err(|e| error(e.to_string()));
        }
        let record = MemoryRecord {
            namespace: scope.namespace(),
            timestamp_ms: store::now_ms(),
            content: new.content,
            tags: new.tags,
            meta: self
                .0
                .repository
                .as_ref()
                .filter(|_| scope.key() != USER_SCOPE)
                .map(|identity| {
                    json!({
                        "_synaps_repository": {
                            "repository_project": identity.scope.key(),
                            "source_worktree_project": identity.legacy_scope.key(),
                            "source_worktree_root": identity.worktree_root,
                        }
                    })
                }),
            id: Some(format!("mem-{}", uuid::Uuid::new_v4().simple())),
            project: Some(scope.key().into()),
            provenance: Some(new.provenance),
            sensitivity: Some(new.sensitivity),
            retention: Some(new.retention),
        };
        let saved: MemoryRecord = serde_json::from_value(
            self.call(
                "store",
                serde_json::to_value(&record).map_err(|_| error("record serialization failed"))?,
            )
            .await.map_err(|cause| error(format!("store outcome unconfirmed for {}: {cause}; fetch this exact ID to reconcile, never automatically create a replacement", record.id.as_deref().unwrap())))?,
        )
        .map_err(|_| {
            error(format!(
                "invalid backend store acknowledgement; commit unknown for {}; do not automatically retry", record.id.as_deref().unwrap()
            ))
        })?;
        validate_record(&saved, &scope, record.id.as_deref().unwrap())?;
        let mut expected = record;
        if expected.sensitivity == Some(MemorySensitivity::Secret) {
            expected.content.clear();
        }
        if saved != expected {
            return Err(error("backend store acknowledgement mismatch; commit unknown, do not automatically retry"));
        }
        Ok(saved)
    }
    pub async fn forget(&self, id: &str) -> Result<()> {
        let scope = self.scope()?.clone();
        if id.is_empty() || id.len() > 128 {
            return Err(error("invalid exact ID"));
        }
        if matches!(self.0.backend, Backend::Legacy) {
            let base = self.0.base.clone();
            let id = id.to_owned();
            return tokio::task::spawn_blocking(move || store::forget_in(&base, &scope, &id))
                .await
                .map_err(|_| error("legacy forget worker failed"))?
                .map_err(|e| error(e.to_string()));
        }
        match self.call("forget", json!({"id":id})).await? {
            Value::Bool(true) => Ok(()),
            Value::Bool(false) => Err(error("record not found in this project scope")),
            _ => Err(error("invalid forget acknowledgement; commit unknown")),
        }
    }
}
fn validate_record(record: &MemoryRecord, scope: &ProjectScope, id: &str) -> Result<()> {
    validate_record_members(record, &[scope.key().to_owned()], id)
}
fn validate_record_members(record: &MemoryRecord, members: &[String], id: &str) -> Result<()> {
    if record.id.as_deref() != Some(id)
        || !record
            .project
            .as_ref()
            .is_some_and(|project| members.contains(project))
        || (record.namespace != format!("project-{}", record.project.as_deref().unwrap_or(""))
            && !(record.namespace == "captures"
                && id.starts_with("mem-cap-")
                && record
                    .provenance
                    .as_ref()
                    .is_some_and(|p| p.source == "capture")))
        || record.content.len() > store::MAX_CONTENT_BYTES
        || record.sensitivity.is_none()
        || record.retention.is_none()
        || (record.sensitivity == Some(MemorySensitivity::Secret) && !record.content.is_empty())
    {
        return Err(error("backend record violates scope/disclosure/bounds"));
    }
    Ok(())
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    id: String,
    project: String,
    timestamp_ms: u64,
    tags: Vec<String>,
    snippet: String,
    truncated: bool,
    content_bytes: usize,
    sensitivity: MemorySensitivity,
    retention: store::MemoryRetention,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_same_storage(parent: &MemoryBinding, child: &MemoryBinding) {
        assert_eq!(child.0.base, parent.0.base);
        assert_eq!(child.0.scope, parent.0.scope);
        assert_eq!(child.0.repository, parent.0.repository);
        assert_eq!(child.0.user_scope_enabled, parent.0.user_scope_enabled);
        assert_eq!(child.0.managed_parent, parent.0.managed_parent);
        assert_eq!(child.exclusive(), parent.exclusive());
        assert_eq!(child.is_axel(), parent.is_axel());
        assert!(Arc::ptr_eq(
            &child.0.operation_lock,
            &parent.0.operation_lock
        ));
        match (&parent.0.backend, &child.0.backend) {
            (Backend::Legacy, Backend::Legacy) | (Backend::Unavailable, Backend::Unavailable) => {}
            (
                Backend::Axel {
                    executable: a,
                    brain: b,
                },
                Backend::Axel {
                    executable: c,
                    brain: d,
                },
            ) => {
                assert_eq!(a, c);
                assert_eq!(b, d);
            }
            _ => panic!("worker changed the selected backend"),
        }
    }

    #[test]
    fn forum_author_is_fresh_but_clones_and_scope_rebindings_keep_identity() {
        let dir = tempfile::tempdir().unwrap();
        let scope = ProjectScope::for_root(dir.path()).unwrap();
        let parent = MemoryBinding::new(dir.path().into(), Ok(scope.clone()), &Default::default());
        let fresh = MemoryBinding::new(dir.path().into(), Ok(scope), &Default::default());
        parent.forum_author().validate().unwrap();
        assert!(parent.forum_author().parent.is_none());
        assert_ne!(parent.forum_author().actor, fresh.forum_author().actor);
        assert_ne!(parent.forum_author().group, fresh.forum_author().group);
        let clone = parent.clone();
        assert!(Arc::ptr_eq(&parent.0, &clone.0));
        assert_eq!(clone.forum_author(), parent.forum_author());

        let other = tempfile::tempdir().unwrap();
        let other_scope = ProjectScope::for_root(other.path()).unwrap();
        let worker = parent.fork_for_worker();
        for binding in [&parent, &worker] {
            let scoped = binding.with_scope(other_scope.clone());
            assert_eq!(scoped.scope().unwrap(), &other_scope);
            assert_eq!(scoped.forum_author(), binding.forum_author());
            // Restoring the scope must reveal otherwise identical storage state.
            assert_same_storage(
                binding,
                &scoped.with_scope(binding.scope().unwrap().clone()),
            );
        }
    }

    #[test]
    fn forum_worker_forks_only_author_even_with_bad_scope_or_unavailable_backend() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let scope = ProjectScope::for_root(&root).unwrap();
        let repository = agent_core::memory::repository::RepositoryIdentity {
            scope: scope.clone(),
            legacy_scope: scope.clone(),
            aliases: vec![scope.clone()],
            repository_root: root.clone(),
            worktree_root: root.clone(),
            common_dir: Some(root.join("synthetic-common")),
        };
        for kind in [
            MemoryBackendKind::Legacy,
            MemoryBackendKind::Axel,
            MemoryBackendKind::Unavailable,
        ] {
            for bad_scope in [false, true] {
                for user_scope in [false, true] {
                    for managed_parent in [false, true] {
                        let config = MemoryBackendConfig {
                            kind,
                            executable: Some(root.join("unused-service")),
                            brain: (!managed_parent).then(|| root.join("custom-brain.r8")),
                            user_scope,
                        };
                        let selected_scope = if bad_scope {
                            Err("captured repository discovery failure".into())
                        } else {
                            Ok(scope.clone())
                        };
                        let mut parent = MemoryBinding::new(root.clone(), selected_scope, &config);
                        Arc::get_mut(&mut parent.0).unwrap().repository = Some(repository.clone());
                        let first = parent.fork_for_worker();
                        let second = parent.fork_for_worker();
                        for worker in [&first, &second] {
                            assert_same_storage(&parent, worker);
                            assert!(!Arc::ptr_eq(&parent.0, &worker.0));
                            worker.forum_author().validate().unwrap();
                            assert_ne!(worker.forum_author().actor, parent.forum_author().actor);
                            assert_eq!(worker.forum_author().group, parent.forum_author().group);
                            assert_eq!(
                                worker.forum_author().parent.as_deref(),
                                Some(parent.forum_author().actor.as_str())
                            );
                            assert_eq!(worker.clone().forum_author(), worker.forum_author());
                        }
                        assert_ne!(first.forum_author().actor, second.forum_author().actor);
                        let grandchild = first.fork_for_worker();
                        assert_same_storage(&first, &grandchild);
                        assert_eq!(grandchild.forum_author().group, parent.forum_author().group);
                        assert_eq!(
                            grandchild.forum_author().parent.as_deref(),
                            Some(first.forum_author().actor.as_str())
                        );
                        assert!(parent.forum_author().parent.is_none());
                    }
                }
            }
        }
        assert!(!root.join("memory").exists());
        assert!(!root.join("custom-brain.r8").exists());
    }

    #[test]
    fn forum_worker_preserves_user_note_opt_in_and_user_scope_without_widening() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let scope = ProjectScope::for_root(&root).unwrap();
        for user_scope in [false, true] {
            let parent = MemoryBinding::new(
                root.clone(),
                Ok(scope.clone()),
                &MemoryBackendConfig {
                    kind: MemoryBackendKind::Axel,
                    executable: Some(root.join("unused-service")),
                    brain: Some(root.join("brain.r8")),
                    user_scope,
                },
            );
            let worker = parent.fork_for_worker();
            assert_eq!(worker.for_user_notes().is_ok(), user_scope);
            if user_scope {
                let user = parent.for_user_notes().unwrap();
                assert_eq!(user.forum_author(), parent.forum_author());
                let user_worker = user.fork_for_worker();
                assert_eq!(user_worker.scope().unwrap().key(), USER_SCOPE);
                assert_same_storage(&user, &user_worker);
            }
        }
    }

    #[test]
    fn forum_workers_and_scope_rebindings_share_the_operation_lock() {
        let dir = tempfile::tempdir().unwrap();
        let scope = ProjectScope::for_root(dir.path()).unwrap();
        let parent = MemoryBinding::new(dir.path().into(), Ok(scope.clone()), &Default::default());
        let worker = parent.fork_for_worker();
        let sibling = parent.fork_for_worker();
        let scoped = worker.with_scope(scope);
        let guard = parent.0.operation_lock.try_lock().unwrap();
        for binding in [&worker, &sibling, &scoped] {
            assert!(Arc::ptr_eq(
                &parent.0.operation_lock,
                &binding.0.operation_lock
            ));
            assert!(binding.0.operation_lock.try_lock().is_err());
        }
        drop(guard);
        let worker_guard = worker.0.operation_lock.try_lock().unwrap();
        assert!(parent.0.operation_lock.try_lock().is_err());
        assert!(sibling.0.operation_lock.try_lock().is_err());
        drop(worker_guard);
        assert!(scoped.0.operation_lock.try_lock().is_ok());
    }

    /// Explicit opt-in integration test uses only a fresh temp brain and scope.
    /// Run with SYNAPS_AXEL_TEST_BIN=/absolute/service cargo test ... --ignored.
    #[tokio::test]
    #[ignore = "requires separately built pinned Axel service; synthetic temp storage only"]
    async fn real_axel_service_roundtrip_without_legacy_store() {
        let exe =
            PathBuf::from(std::env::var_os("SYNAPS_AXEL_TEST_BIN").expect("explicit test binary"));
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let scope = ProjectScope::for_root(&root).unwrap();
        let binding = MemoryBinding::new(
            root.clone(),
            Ok(scope.clone()),
            &MemoryBackendConfig {
                kind: MemoryBackendKind::Axel,
                executable: Some(exe.clone()),
                brain: Some(root.join("brain.r8")),
                user_scope: false,
            },
        );
        for sensitivity in [
            MemorySensitivity::Normal,
            MemorySensitivity::Sensitive,
            MemorySensitivity::Secret,
        ] {
            let saved = binding
                .store(NewMemoryRecord {
                    content: "Ünicode cobalt_27".into(),
                    tags: vec!["tag_%literal".into()],
                    provenance: store::MemoryProvenance {
                        source: "model".into(),
                        session: Some(format!("synthetic-{sensitivity:?}")),
                    },
                    sensitivity,
                    retention: store::MemoryRetention::MaxAgeDays(365),
                })
                .await
                .unwrap();
            let id = saved.id.unwrap();
            let rows = binding
                .search(ProjectMemoryQuery {
                    content_contains: Some("ÜNICODE".into()),
                    tag_prefix: Some("tag_%".into()),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(
                rows.iter().any(|row| row.id == id),
                sensitivity != MemorySensitivity::Secret
            );
            let fetched = binding.fetch(&[&id]).await.unwrap();
            assert_eq!(
                fetched[0].content,
                if sensitivity == MemorySensitivity::Secret {
                    ""
                } else {
                    "Ünicode cobalt_27"
                }
            );
            assert_eq!(fetched[0].sensitivity, Some(sensitivity));
            binding.forget(&id).await.unwrap();
            assert!(binding.fetch(&[&id]).await.is_err());
        }
        // Exercise the public short tool surface with the same captured binding.
        use crate::tools::Tool;
        let context = || {
            let mut ctx = crate::tools::test_helpers::create_tool_context();
            ctx.capabilities.memory_backend = Some(binding.clone());
            ctx
        };
        let stored = crate::tools::memory::MemoryStoreTool
            .execute(
                json!({"content":"SHORT_TOOL_AXEL_CANARY","sensitivity":"normal"}),
                context(),
            )
            .await
            .unwrap();
        let tool_id = stored.split_whitespace().nth(2).unwrap();
        let searched = crate::tools::memory::MemorySearchTool
            .execute(json!({"query":"SHORT_TOOL_AXEL_CANARY"}), context())
            .await
            .unwrap();
        assert!(searched.contains(tool_id));
        let fetched = crate::tools::memory::MemoryFetchTool
            .execute(json!({"ids":[tool_id]}), context())
            .await
            .unwrap();
        assert!(fetched.contains("SHORT_TOOL_AXEL_CANARY"));
        crate::tools::memory::MemoryForgetTool
            .execute(json!({"id":tool_id}), context())
            .await
            .unwrap();
        assert!(!root.join("memory").exists());
        assert!(binding.history_search("", 8).await.is_ok());
        assert!(!root.join("context-archives").exists());
        let other = tempfile::tempdir().unwrap();
        let foreign = MemoryBinding::new(
            root.clone(),
            Ok(ProjectScope::for_root(other.path()).unwrap()),
            &MemoryBackendConfig {
                kind: MemoryBackendKind::Axel,
                executable: Some(exe),
                brain: Some(root.join("brain.r8")),
                user_scope: false,
            },
        );
        assert!(foreign.search(Default::default()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn unavailable_axel_never_opens_legacy_store() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let scope = ProjectScope::for_root(&root).unwrap();
        let cfg = MemoryBackendConfig {
            kind: MemoryBackendKind::Axel,
            executable: None,
            brain: None,
            user_scope: false,
        };
        let binding = MemoryBinding::new(root.clone(), Ok(scope), &cfg);
        assert!(binding.exclusive());
        assert!(binding.search(Default::default()).await.is_err());
        assert!(binding.fetch(&["mem-unknown"]).await.is_err());
        assert!(binding.forget("mem-unknown").await.is_err());
        assert!(binding
            .store(NewMemoryRecord {
                content: "note".into(),
                tags: vec![],
                provenance: store::MemoryProvenance {
                    source: "model".into(),
                    session: None
                },
                sensitivity: MemorySensitivity::Normal,
                retention: store::MemoryRetention::Standard
            })
            .await
            .is_err());
        assert!(!root.join("memory").exists());
    }
}

#[cfg(test)]
mod shared_tests;
