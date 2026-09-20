//! Explicit repository-scope continuity for the local operator, never model tools.
//!
//! Identity reporting and mapping digests inspect no memory. Built-in migration
//! imports each verified path scope under its ORIGINAL key, then links membership
//! to the repository key. Neither capture payloads nor archive identities are
//! rewritten. Every import remains a separate <=24 MiB atomic operation; a batch
//! and its final alias link are NOT a global transaction. Sources/config remain
//! untouched, and no session/private-history discovery is implied.

use super::{
    migration::{self, MigrationManifest},
    MemoryBinding,
};
use agent_core::memory::store::ProjectScope;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const IDENTITY_FORMAT: &str = "synaps-repository-memory-scope/1";
const BATCH_FORMAT: &str = "synaps-repository-memory-migration/1";

/// Metadata only: a verified root and its canonical or original path-scope key.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ScopeMember {
    pub project: String,
    pub root: PathBuf,
}

/// Safe operator output. No note IDs, bodies, capture evidence or archive notes.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct IdentityReport {
    pub format: &'static str,
    pub canonical_project: String,
    /// Current proof root, independent of a historical source path with the same key.
    pub canonical_root: PathBuf,
    pub legacy_project: String,
    pub members: Vec<ScopeMember>,
    pub mapping_digest: String,
}

/// Construct an identity report only from host-verified scopes. Callers must use
/// repository discovery, not infer aliases from project-key prefixes or names.
/// Sorting makes the mapping digest independent of worktree registration order.
/// Include the canonical member even when it has no legacy data of its own.
pub fn identity_report(
    canonical: &ProjectScope,
    legacy: &ProjectScope,
    verified: &[ProjectScope],
) -> Result<IdentityReport> {
    let mut members = BTreeMap::new();
    for scope in verified {
        if let Some(prior) = members.insert(scope.key().to_owned(), scope.root().to_path_buf()) {
            if prior != scope.root() {
                bail!("conflicting verified source roots for one memory scope");
            }
        }
    }
    // The random canonical key is distinct from all legacy path scopes. Include
    // it even without legacy data; preserve explicitly supplied historical roots.
    // Source reads use keys, not historical root existence.
    members
        .entry(canonical.key().to_owned())
        .or_insert_with(|| canonical.root().to_path_buf());
    if !members.contains_key(legacy.key()) {
        bail!("current legacy scope is not in the verified repository mapping");
    }
    let members: Vec<_> = members
        .into_iter()
        .map(|(project, root)| ScopeMember { project, root })
        .collect();
    // Hash native path bytes, not lossy display strings. A cwd within another
    // registered worktree must yield the same mapping consent, so legacy_project
    // is reported but is not part of the mapping's digest.
    let mut hash = Sha256::new();
    hash_part(&mut hash, IDENTITY_FORMAT.as_bytes());
    hash_part(&mut hash, canonical.key().as_bytes());
    hash_part(&mut hash, canonical.root().as_os_str().as_encoded_bytes());
    for member in &members {
        hash_part(&mut hash, member.project.as_bytes());
        hash_part(&mut hash, member.root.as_os_str().as_encoded_bytes());
    }
    Ok(IdentityReport {
        format: IDENTITY_FORMAT,
        canonical_project: canonical.key().into(),
        canonical_root: canonical.root().to_path_buf(),
        legacy_project: legacy.key().into(),
        members,
        mapping_digest: format!("{:x}", hash.finalize()),
    })
}

/// Offline built-in batch preview; each source preview carries its own original
/// scope, digest, migration ID and frame bound. Empty verified scopes are included
/// so new source data appearing between preview/apply invalidates the batch.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RepositoryManifest {
    pub format: &'static str,
    pub identity: IdentityReport,
    pub sources: Vec<MigrationManifest>,
    pub manifest_digest: String,
    pub atomicity: &'static str,
}

pub fn preview_builtin_in(
    base: &Path,
    canonical: &ProjectScope,
    legacy: &ProjectScope,
    verified: &[ProjectScope],
) -> Result<RepositoryManifest> {
    let identity = identity_report(canonical, legacy, verified)?;
    let scopes: BTreeMap<_, _> = std::iter::once(canonical)
        .chain(verified)
        .map(|s| (s.key(), s))
        .collect();
    let mut sources = Vec::new();
    for scope in scopes.values() {
        sources.push(migration::preview_in(base, scope)?);
    }
    let mut hash = Sha256::new();
    hash_part(&mut hash, BATCH_FORMAT.as_bytes());
    hash_part(&mut hash, identity.mapping_digest.as_bytes());
    for source in &sources {
        hash_part(&mut hash, source.project.as_bytes());
        hash_part(&mut hash, source.manifest_digest.as_bytes());
    }
    Ok(RepositoryManifest {
        format: BATCH_FORMAT,
        identity,
        sources,
        manifest_digest: format!("{:x}", hash.finalize()),
        atomicity: "one bounded atomic import per source, then separate alias link; partial progress possible",
    })
}

/// Report the immutable identity captured by the production binding. This never
/// reads a brain, changes configuration, or reinterprets the current directory.
pub fn report(binding: &MemoryBinding) -> Result<IdentityReport> {
    let identity = binding.repository_identity()?;
    identity_report(&identity.scope, &identity.legacy_scope, &identity.aliases)
}

pub fn preview_builtin(binding: &MemoryBinding) -> Result<RepositoryManifest> {
    let identity = binding.repository_identity()?;
    preview_builtin_in(
        binding.base(),
        &identity.scope,
        &identity.legacy_scope,
        &identity.aliases,
    )
}

fn hash_part(hash: &mut Sha256, part: &[u8]) {
    hash.update((part.len() as u64).to_le_bytes());
    hash.update(part);
}

/// Link preview is offline. Consent binds the exact mapping, destination, and
/// selected original scope keys. It does not claim to snapshot live DB contents.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LinkManifest {
    pub format: &'static str,
    pub identity: IdentityReport,
    pub destination: PathBuf,
    pub link_projects: Vec<String>,
    pub manifest_digest: String,
    pub effect: &'static str,
}

pub fn preview_link(binding: &MemoryBinding, projects: &[String]) -> Result<LinkManifest> {
    let identity = report(binding)?;
    let destination = binding
        .brain_path()
        .context("shared Axel destination unavailable")?
        .to_path_buf();
    let mut projects = projects.to_vec();
    projects.sort();
    projects.dedup();
    if projects
        .iter()
        .any(|p| !identity.members.iter().any(|m| &m.project == p))
    {
        bail!("alias linking requires exact host-verified source scope keys");
    }
    projects.retain(|p| p != &identity.canonical_project);
    let mut hash = Sha256::new();
    hash_part(&mut hash, b"synaps-repository-memory-link/1");
    hash_part(&mut hash, identity.mapping_digest.as_bytes());
    hash_part(&mut hash, destination.as_os_str().as_encoded_bytes());
    for project in &projects {
        hash_part(&mut hash, project.as_bytes());
    }
    Ok(LinkManifest {
        format: "synaps-repository-memory-link/1", identity, destination,
        link_projects: projects, manifest_digest: format!("{:x}", hash.finalize()),
        effect: "explicitly share existing selected scope memory/history and deletion evidence; no payload identity rewrite; separate atomic link per scope",
    })
}

pub fn verified_projects(binding: &MemoryBinding) -> Result<Vec<String>> {
    Ok(report(binding)?
        .members
        .into_iter()
        .map(|m| m.project)
        .collect())
}

/// Select an original scope from captured host evidence, including historical
/// paths that no longer exist. Never reconstruct/recanonicalize a source root.
pub fn source_binding(binding: &MemoryBinding, project: &str) -> Result<MemoryBinding> {
    let identity = binding.repository_identity()?;
    let source = identity
        .aliases
        .iter()
        .find(|s| s.key() == project)
        .or_else(|| (identity.scope.key() == project).then_some(&identity.scope))
        .context("source target must exactly match a verified repository scope key")?;
    Ok(binding.with_scope(source.clone()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeInfo {
    pub canonical_project: String,
    pub members: Vec<String>,
    pub mode: String,
    pub user_scope: bool,
}

/// Strict metadata projection: never forward arbitrary service output to stdout.
pub async fn scope_info(binding: &MemoryBinding) -> Result<ScopeInfo> {
    parse_scope_info(
        binding.rpc("scope_info", json!({})).await?,
        binding.scope()?.key(),
    )
}
fn parse_scope_info(value: Value, requested: &str) -> Result<ScopeInfo> {
    let info: ScopeInfo = serde_json::from_value(value)
        .map_err(|_| anyhow::anyhow!("invalid service scope metadata"))?;
    if !matches!(info.mode.as_str(), "multi_project" | "pinned")
        || info.user_scope
        || info.members.len() > 4096
        || !info
            .members
            .iter()
            .all(|p| valid_project(p) && p != "p0000000000000000")
        || info
            .members
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != info.members.len()
        || !info.members.iter().any(|p| p == requested)
        || !info.members.contains(&info.canonical_project)
    {
        bail!("invalid service scope membership metadata");
    }
    Ok(info)
}
fn valid_project(s: &str) -> bool {
    s.len() == 17
        && s.starts_with('p')
        && s[1..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn require_digest(expected: &str, actual: &str) -> Result<()> {
    if !migration::is_digest(expected) || expected != actual {
        bail!("source, destination or repository mapping changed / preview digest mismatched; preview again (nothing dispatched)");
    }
    Ok(())
}

/// No global rollback is promised. Error reports intentionally omit raw service
/// errors/bodies and retain acknowledged progress for safe reconciliation.
#[derive(Debug, Clone, Serialize)]
pub struct ApplyReport {
    pub complete: bool,
    pub imported_projects: Vec<String>,
    pub linked_projects: Vec<String>,
    pub failed_project: Option<String>,
    pub failed_operation: Option<&'static str>,
    pub outcome_unconfirmed: bool,
    pub source_unchanged: bool,
    pub config_unchanged: bool,
    pub atomicity: &'static str,
    pub recovery: &'static str,
}
impl ApplyReport {
    fn new() -> Self {
        Self { complete: false, imported_projects: vec![], linked_projects: vec![],
            failed_project: None, failed_operation: None, outcome_unconfirmed: false,
            source_unchanged: true, config_unchanged: true,
            atomicity: "separate <=24 MiB atomic imports and separate alias links; partial progress possible",
            recovery: "stop writers; reconcile or retry the identical preview/options; never replace IDs; no config cutover performed" }
    }
    fn failed(&mut self, project: &str, operation: &'static str) {
        self.failed_project = Some(project.into());
        self.failed_operation = Some(operation);
        self.outcome_unconfirmed = true;
    }
}

pub async fn apply_link(
    binding: &MemoryBinding,
    projects: &[String],
    digest: &str,
) -> Result<ApplyReport> {
    let preview = preview_link(binding, projects)?;
    require_digest(digest, &preview.manifest_digest)?;
    let mut result = ApplyReport::new();
    link_members(binding, &preview, &mut result).await;
    Ok(result)
}
async fn link_members(binding: &MemoryBinding, preview: &LinkManifest, result: &mut ApplyReport) {
    for project in &preview.link_projects {
        let member = preview
            .identity
            .members
            .iter()
            .find(|m| &m.project == project)
            .expect("validated mapping");
        let operation = async {
            // Register an empty source scope if necessary. This is explicit apply,
            // never a report/preview side effect and never imports private files.
            let source = source_binding(binding, project)?;
            let info = scope_info(&source).await?;
            if info.mode != "multi_project" {
                bail!("pinned destination requires explicit upgrade-memory first");
            }
            let value = binding
                .rpc(
                    "scope_alias",
                    json!({
                        "alias_project": project, "alias_root": member.root,
                        "canonical_root": preview.identity.canonical_root,
                    }),
                )
                .await?;
            let info = parse_scope_info(value, project)?;
            if info.canonical_project != preview.identity.canonical_project {
                bail!("alias acknowledgement does not match canonical repository");
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if operation.is_err() {
            result.failed(project, "scope_alias");
            return;
        }
        result.linked_projects.push(project.clone());
    }
    result.complete = true;
}

/// The combined consent binds imports plus the subsequent explicit alias plan.
#[derive(Debug, Clone, Serialize)]
pub struct MigrationPlan {
    pub format: &'static str,
    pub sources: Vec<MigrationManifest>,
    pub linking: LinkManifest,
    pub manifest_digest: String,
    pub atomicity: &'static str,
}
fn plan(sources: Vec<MigrationManifest>, linking: LinkManifest) -> MigrationPlan {
    let mut hash = Sha256::new();
    hash_part(&mut hash, BATCH_FORMAT.as_bytes());
    hash_part(&mut hash, linking.manifest_digest.as_bytes());
    for source in &sources {
        hash_part(&mut hash, source.project.as_bytes());
        hash_part(&mut hash, source.manifest_digest.as_bytes());
    }
    MigrationPlan { format: BATCH_FORMAT, sources, linking, manifest_digest: format!("{:x}", hash.finalize()),
        atomicity: "one <=24 MiB atomic import per original scope, then separate explicit alias links; partial progress possible" }
}
pub fn preview_migration(binding: &MemoryBinding) -> Result<MigrationPlan> {
    let sources = preview_builtin(binding)?.sources;
    Ok(plan(
        sources,
        preview_link(binding, &verified_projects(binding)?)?,
    ))
}
pub async fn apply_migration(binding: &MemoryBinding, digest: &str) -> Result<ApplyReport> {
    // Preflight every source and bound before the first dispatch. Individual
    // applies re-read again. Uncooperative writers still require operator quiescence.
    let preview = preview_migration(binding)?;
    require_digest(digest, &preview.manifest_digest)?;
    let mut result = ApplyReport::new();
    for source in &preview.sources {
        let target = source_binding(binding, &source.project)?;
        if migration::apply(&target, &source.manifest_digest)
            .await
            .is_err()
        {
            result.failed(&source.project, "migration_apply");
            return Ok(result);
        }
        result.imported_projects.push(source.project.clone());
    }
    link_members(binding, &preview.linking, &mut result).await;
    Ok(result)
}

/// Plugin mapping is per source: exporter alias -> explicit verified original
/// host scope. Native host exports must target their own source scope exactly.
pub async fn preview_external(
    binding: &MemoryBinding,
    source: &migration::MigrationSource,
    target: &str,
) -> Result<MigrationPlan> {
    let scoped = source_binding(binding, target)?;
    let manifest = migration::preview_from(&scoped, source).await?;
    Ok(plan(
        vec![manifest],
        preview_link(binding, &[target.into()])?,
    ))
}
pub async fn apply_external(
    binding: &MemoryBinding,
    source: &migration::MigrationSource,
    target: &str,
    digest: &str,
) -> Result<ApplyReport> {
    let preview = preview_external(binding, source, target).await?;
    require_digest(digest, &preview.manifest_digest)?;
    let mut result = ApplyReport::new();
    let scoped = source_binding(binding, target)?;
    if migration::apply_from(&scoped, source, &preview.sources[0].manifest_digest)
        .await
        .is_err()
    {
        result.failed(target, "migration_apply");
        return Ok(result);
    }
    result.imported_projects.push(target.into());
    link_members(binding, &preview.linking, &mut result).await;
    Ok(result)
}

/// Digest-gated schema-marker conversion of the configured existing brain, NOT
/// an import or alias grant. Offline preview hashes the complete quiescent file;
/// the service validates its pinned host schema and exact expected pin at apply.
/// Stop/checkpoint writers first. File locks are advisory; the small host-check
/// to service-dispatch gap cannot protect against uncooperative external writers.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UpgradeManifest {
    pub format: &'static str,
    pub brain: PathBuf,
    pub expected_project: String,
    pub canonical_project: String,
    pub mapping_digest: String,
    pub brain_bytes: u64,
    pub manifest_digest: String,
    pub schema_validation: &'static str,
    pub effect: &'static str,
}

pub fn preview_upgrade(binding: &MemoryBinding, expected_project: &str) -> Result<UpgradeManifest> {
    let _source = source_binding(binding, expected_project)?;
    let identity = report(binding)?;
    let brain = binding
        .brain_path()
        .context("configured shared brain unavailable")?;
    let (brain_bytes, content_digest) = upgrade_file_digest(brain)?;
    let mut hash = Sha256::new();
    hash_part(&mut hash, b"synaps-memory-scope-upgrade/1");
    hash_part(&mut hash, brain.as_os_str().as_encoded_bytes());
    hash_part(&mut hash, expected_project.as_bytes());
    hash_part(&mut hash, identity.mapping_digest.as_bytes());
    hash_part(&mut hash, content_digest.as_bytes());
    Ok(UpgradeManifest { format: "synaps-memory-scope-upgrade/1", brain: brain.into(),
        expected_project: expected_project.into(), canonical_project: identity.canonical_project,
        mapping_digest: identity.mapping_digest, brain_bytes, manifest_digest: format!("{:x}", hash.finalize()),
        schema_validation: "offline file preview only; service must validate existing pinned host schema and expected project before conversion",
        effect: "convert existing pinned brain schema marker to multi-project; retain all original identities/data; no aliases, config change or import" })
}

pub async fn apply_upgrade(
    binding: &MemoryBinding,
    expected_project: &str,
    digest: &str,
) -> Result<ScopeInfo> {
    let preview = preview_upgrade(binding, expected_project)?;
    require_digest(digest, &preview.manifest_digest)?;
    // A second independent read also catches path replacement between snapshots.
    let current = preview_upgrade(binding, expected_project)?;
    require_digest(digest, &current.manifest_digest)?;
    let scoped = source_binding(binding, expected_project)?;
    let value = scoped.rpc("scope_upgrade", json!({"expected_project":expected_project})).await
        .context("upgrade outcome unconfirmed; inspect scope_info before retry; committed marker conversion changes the file digest; no import/config change performed")?;
    let info = parse_scope_info(value, expected_project)?;
    if info.mode != "multi_project" {
        bail!("upgrade acknowledgement is not multi_project; reconcile before retry");
    }
    Ok(info)
}

fn upgrade_file_digest(path: &Path) -> Result<(u64, String)> {
    use std::io::Read;
    if path.extension().and_then(|s| s.to_str()) != Some("r8") {
        bail!("upgrade requires the configured existing absolute .r8 brain");
    }
    require_checkpointed(path)?;
    let mut file = migration::open_export(path)?;
    if !fs4::fs_std::FileExt::try_lock_shared(&file)? {
        bail!("brain locked; stop writers before upgrade preview");
    }
    let before = file.metadata()?;
    let mut hash = Sha256::new();
    hash_part(&mut hash, &before.len().to_le_bytes());
    let mut buffer = [0u8; 65536];
    let mut bytes = 0u64;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        bytes = bytes.checked_add(n as u64).context("brain size overflow")?;
        if bytes > before.len() {
            bail!("brain changed during preview; stop writers");
        }
        hash.update(&buffer[..n]);
    }
    let current = migration::open_export(path)?;
    if bytes != before.len()
        || !same_file(&before, &file.metadata()?)
        || !same_file(&before, &current.metadata()?)
    {
        bail!("brain changed/replaced during preview; stop writers");
    }
    require_checkpointed(path)?;
    Ok((bytes, format!("{:x}", hash.finalize())))
}
fn require_checkpointed(path: &Path) -> Result<()> {
    for suffix in ["-wal", "-journal", "-shm"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        match migration::open_export(Path::new(&name)) {
            Ok(file) if suffix == "-shm" || file.metadata()?.len() == 0 => {}
            Ok(_) => bail!("upgrade preview requires stopped writers and checkpointed WAL/journal; no checkpoint performed automatically"),
            Err(e) if e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {}
            Err(_) => bail!("unsafe/unreadable brain sidecar; upgrade refused"),
        }
    }
    Ok(())
}
#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    (
        a.dev(),
        a.ino(),
        a.len(),
        a.mtime(),
        a.mtime_nsec(),
        a.ctime(),
        a.ctime_nsec(),
    ) == (
        b.dev(),
        b.ino(),
        b.len(),
        b.mtime(),
        b.mtime_nsec(),
        b.ctime(),
        b.ctime_nsec(),
    )
}
#[cfg(not(unix))]
fn same_file(_: &std::fs::Metadata, _: &std::fs::Metadata) -> bool {
    false
}
