//! Explicit operator-only legacy migration. Never used by model tools or startup.
//!
//! Each atomic source is the original project's built-in JSONL file and validated context
//! archives. Preview contains counts and hashes, never bodies, tags, IDs or notes.
//! Sources are opened read-only, nofollow, without creating directories/lock files.
//! Apply rereads and verifies the preview and sends ONE <=24 MiB transaction. It
//! never edits config, deletes source, switches selection or performs dual writes.
//! Stop legacy writers for cutover: file locks are advisory, and old appenders do
//! not participate in them. Retained descriptors plus a second read detect changes
//! before dispatch; they cannot make uncooperative legacy processes transactional.

use super::MemoryBinding;
use agent_core::context_archive;
use agent_core::memory::store::{
    MemoryProvenance, MemoryRecord, MemoryRetention, MemorySensitivity, ProjectScope,
    MAX_CONTENT_BYTES,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// The complete RPC envelope, not merely its records, must fit this limit.
pub const MAX_APPLY_BYTES: usize = 24 * 1024 * 1024;
const MAX_SOURCE_BYTES: usize = MAX_APPLY_BYTES;
const MAX_SOURCE_LINES: usize = 100_000;
const FORMAT: &str = "synaps-legacy-axel-migration/1";

/// Safe to display to the operator: fixed-size metadata, never source text.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MigrationManifest {
    pub format: &'static str,
    pub source: &'static str,
    pub project: String,
    pub source_project: Option<String>,
    pub captures: usize,
    pub fingerprints: usize,
    pub migration_id: String,
    pub manifest_digest: String,
    pub source_jsonl_bytes: usize,
    pub source_jsonl_present: bool,
    pub archive_inventory_bytes: usize,
    pub records: usize,
    pub tombstones: usize,
    pub tombstone_suppressed_records: usize,
    pub excluded_unscoped_records: usize,
    pub excluded_foreign_records: usize,
    pub duplicate_records: usize,
    pub secret_records: usize,
    pub sensitive_records: usize,
    pub histories: usize,
    pub history_tombstones: usize,
    pub apply_frame_bytes: usize,
}

// No Debug/Serialize: this carries private bodies and read locks, not a preview.
struct Snapshot {
    manifest: MigrationManifest,
    payload: Value,
    source: Option<std::fs::File>,
    raw: Vec<u8>,
    histories: Vec<Value>,
}

/// Offline preview. Does not start the service or create/open a destination brain.
pub fn preview_in(base: &Path, scope: &ProjectScope) -> Result<MigrationManifest> {
    Ok(snapshot_in(base, scope)?.manifest)
}

/// Operator-only apply; the source is freshly read and checked, never taken from
/// an editable saved preview. Only metadata is returned by the service. On an
/// unknown commit, retry this same preview, not a newly generated record set.
pub async fn apply(binding: &MemoryBinding, manifest_digest: &str) -> Result<Value> {
    if !is_digest(manifest_digest) {
        bail!("--manifest-digest must be the exact 64-character lowercase preview SHA-256");
    }
    if !binding.is_axel() {
        bail!("migration requires an explicitly configured Axel destination; no fallback");
    }
    let scope = binding.scope()?;
    let mut snapshot = snapshot_in(binding.base(), scope)?;
    snapshot.verify_digest(manifest_digest)?;
    snapshot.verify_unchanged(binding.base(), scope)?;
    let result = binding
        .rpc("migration_apply", snapshot.payload.clone())
        .await
        .with_context(|| format!(
            "migration outcome unconfirmed (migration_id={}, manifest_digest={}); source/config unchanged; reconcile or retry this exact manifest, never create replacement IDs",
            snapshot.manifest.migration_id, snapshot.manifest.manifest_digest
        ))?;
    // Keep the read lock alive until the operation has acknowledged.
    drop(snapshot);
    Ok(result)
}

impl Snapshot {
    fn verify_digest(&self, digest: &str) -> Result<()> {
        if digest != self.manifest.manifest_digest {
            bail!("migration source changed or preview digest mismatched; preview again (nothing imported)");
        }
        Ok(())
    }

    fn verify_unchanged(&mut self, base: &Path, scope: &ProjectScope) -> Result<()> {
        if let Some(source) = self.source.as_mut() {
            source.seek(SeekFrom::Start(0))?;
            if read_bounded(source)? != self.raw {
                bail!("migration source changed during read; preview again (nothing imported)");
            }
        }
        // Re-open through the nofollow path as well: catch atomic replacement or
        // creation after a missing-file snapshot, not just edits to the held fd.
        let current = open_source(base, scope)?;
        if current.is_some() != self.source.is_some() {
            bail!("migration source changed during read; preview again (nothing imported)");
        }
        if let Some(mut file) = current {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let held = self.source.as_ref().expect("presence checked").metadata()?;
                let now = file.metadata()?;
                if (held.dev(), held.ino()) != (now.dev(), now.ino()) {
                    bail!(
                        "migration source replaced during read; preview again (nothing imported)"
                    );
                }
            }
            if read_bounded(&mut file)? != self.raw {
                bail!("migration source changed during read; preview again (nothing imported)");
            }
        }
        if archive_inventory(base, scope)? != self.histories {
            bail!("migration archives changed during read; preview again (nothing imported)");
        }
        Ok(())
    }
}

fn snapshot_in(base: &Path, scope: &ProjectScope) -> Result<Snapshot> {
    let mut source = open_source(base, scope)?;
    if let Some(file) = &source {
        if !fs4::fs_std::FileExt::try_lock_shared(file).context("cannot lock migration source")? {
            bail!("migration source is locked; stop legacy writers and retry");
        }
    }
    let raw = match &mut source {
        Some(file) => read_bounded(file)?,
        None => Vec::new(),
    };
    let parsed = parse_jsonl(&raw, scope)?;
    if parsed.records.len() + parsed.tombstones.len() > 16384 {
        bail!("migration exceeds single-transaction 16384 note/tombstone limit; no truncation supported");
    }
    let histories = archive_inventory(base, scope)?;
    let archive_bytes = serde_json::to_vec(&histories)?;
    let presence = [u8::from(source.is_some())];
    let digest = hash_parts(&[
        FORMAT.as_bytes(),
        base.as_os_str().as_encoded_bytes(),
        scope.key().as_bytes(),
        &presence,
        &raw,
        &archive_bytes,
    ]);
    let migration_id = hash_parts(&[FORMAT.as_bytes(), b"apply", digest.as_bytes()]);
    let history_tombstones = histories.iter().filter(|h| h["tombstone"] == true).count();
    let mut manifest = MigrationManifest {
        format: FORMAT,
        source: "builtin_project_jsonl_and_context_archives",
        project: scope.key().into(),
        source_project: None,
        captures: 0,
        fingerprints: 0,
        migration_id: migration_id.clone(),
        manifest_digest: digest.clone(),
        source_jsonl_bytes: raw.len(),
        source_jsonl_present: source.is_some(),
        archive_inventory_bytes: archive_bytes.len(),
        records: parsed.records.len(),
        tombstones: parsed.tombstones.len(),
        tombstone_suppressed_records: parsed.suppressed,
        excluded_unscoped_records: parsed.unscoped,
        excluded_foreign_records: parsed.foreign,
        duplicate_records: parsed.duplicates,
        secret_records: parsed
            .records
            .iter()
            .filter(|r| r.sensitivity == Some(MemorySensitivity::Secret))
            .count(),
        sensitive_records: parsed
            .records
            .iter()
            .filter(|r| r.sensitivity == Some(MemorySensitivity::Sensitive))
            .count(),
        histories: histories.len() - history_tombstones,
        history_tombstones,
        apply_frame_bytes: 0,
    };
    let payload = json!({
        "migration_id": migration_id,
        "manifest_digest": digest,
        "records": parsed.records,
        "tombstones": parsed.tombstones,
        "histories": histories,
    });
    manifest.apply_frame_bytes = check_frame_bound(scope.key(), &payload)?;
    Ok(Snapshot {
        manifest,
        payload,
        source,
        raw,
        histories,
    })
}

fn check_frame_bound(project: &str, payload: &Value) -> Result<usize> {
    let bytes = serde_json::to_vec(&json!({
        "schema": super::CONTRACT, "project": project,
        "operation": "migration_apply", "payload": payload,
    }))?
    .len()
        + 1;
    if bytes > MAX_APPLY_BYTES {
        bail!("migration exceeds the single-transaction 24 MiB limit; no truncation or partial import is supported");
    }
    Ok(bytes)
}

fn archive_inventory(base: &Path, scope: &ProjectScope) -> Result<Vec<Value>> {
    // This read-only core API validates the original projection digest and scope;
    // do NOT reseal/reproject, reinterpret logical hashes, or disclose hidden notes.
    let mut values = context_archive::export_in(base, scope.key())
        .context("cannot read validated legacy archive inventory")?;
    values.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Ok(values)
}

#[cfg(unix)]
fn open_source(base: &Path, scope: &ProjectScope) -> Result<Option<std::fs::File>> {
    use agent_core::core::private_fs::ConfinedDir;
    if !base.is_absolute() || base == Path::new("/") {
        bail!("migration base must be an absolute non-root no-symlink path");
    }
    let open = || {
        ConfinedDir::open_absolute_no_symlinks(base)?
            .open_file(&["memory".into(), format!("{}.jsonl", scope.namespace())])
    };
    match open() {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => bail!("cannot open project migration source read-only/nofollow"),
    }
}

#[cfg(not(unix))]
fn open_source(_: &Path, _: &ProjectScope) -> Result<Option<std::fs::File>> {
    bail!("safe legacy migration requires Unix nofollow filesystem support")
}

fn read_bounded(file: &mut std::fs::File) -> Result<Vec<u8>> {
    if file.metadata()?.len() > MAX_SOURCE_BYTES as u64 {
        bail!("legacy JSONL source exceeds 24 MiB; no truncated preview/import is supported");
    }
    let mut raw = Vec::new();
    file.take(MAX_SOURCE_BYTES as u64 + 1)
        .read_to_end(&mut raw)?;
    if raw.len() > MAX_SOURCE_BYTES {
        bail!("legacy JSONL source exceeds 24 MiB; no truncated preview/import is supported");
    }
    Ok(raw)
}

pub(super) fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}
fn hash_parts(parts: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part);
    }
    format!("{:x}", hash.finalize())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceRecord {
    namespace: String,
    timestamp_ms: u64,
    content: String,
    #[serde(default)]
    tags: Vec<String>,
    meta: Option<Value>,
    id: Option<String>,
    project: Option<String>,
    provenance: Option<SourceProvenance>,
    sensitivity: Option<MemorySensitivity>,
    retention: Option<MemoryRetention>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceProvenance {
    source: String,
    session: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceTombstone {
    tombstone: String,
    timestamp_ms: u64,
}

struct Parsed {
    records: Vec<MemoryRecord>,
    tombstones: BTreeSet<String>,
    suppressed: usize,
    unscoped: usize,
    foreign: usize,
    duplicates: usize,
}

fn parse_jsonl(raw: &[u8], scope: &ProjectScope) -> Result<Parsed> {
    let text = std::str::from_utf8(raw).context("legacy migration source is not UTF-8")?;
    let mut records = BTreeMap::new();
    let mut parsed = Parsed {
        records: Vec::new(),
        tombstones: BTreeSet::new(),
        suppressed: 0,
        unscoped: 0,
        foreign: 0,
        duplicates: 0,
    };
    for (index, line) in text.lines().enumerate() {
        if index >= MAX_SOURCE_LINES {
            bail!("legacy migration source exceeds 100000 lines; no truncation supported");
        }
        if line.trim().is_empty() {
            continue;
        }
        let fail = || {
            anyhow::anyhow!("invalid or unsupported legacy migration record on line {}; no source content is included in this error", index + 1)
        };
        // Only use Value for discriminating the line. Strict structs below catch
        // duplicate/unknown top-level fields, including hidden policy overrides.
        let value: Value = serde_json::from_str(line).map_err(|_| fail())?;
        if value.get("tombstone").is_some() {
            let tomb: SourceTombstone = serde_json::from_str(line).map_err(|_| fail())?;
            if !valid_id(&tomb.tombstone) || tomb.timestamp_ms > i64::MAX as u64 {
                return Err(fail());
            }
            parsed.tombstones.insert(tomb.tombstone);
            continue;
        }
        let r: SourceRecord = serde_json::from_str(line).map_err(|_| fail())?;
        // Built-in records have no opaque metadata. Reject rather than discard
        // unsupported embedded disclosure/expiry policies, even on excluded rows.
        if r.meta.is_some() {
            return Err(fail());
        }
        if r.project.is_none() || r.id.is_none() {
            parsed.unscoped += 1;
            continue;
        }
        if r.project.as_deref() != Some(scope.key()) {
            parsed.foreign += 1;
            continue;
        }
        let id = r.id.as_deref().expect("checked");
        if r.namespace != scope.namespace()
            || !valid_id(id)
            || r.content.len() > MAX_CONTENT_BYTES
            || r.sensitivity.is_none()
            || r.retention.is_none()
            || r.provenance.is_none()
            || r.timestamp_ms > i64::MAX as u64
            || chrono::DateTime::from_timestamp_millis(r.timestamp_ms as i64).is_none()
        {
            return Err(fail());
        }
        if let Some(MemoryRetention::MaxAgeDays(days)) = r.retention {
            let expiry = r
                .timestamp_ms
                .checked_add(u64::from(days) * 86_400_000)
                .ok_or_else(fail)?;
            if expiry > i64::MAX as u64
                || chrono::DateTime::from_timestamp_millis(expiry as i64).is_none()
            {
                return Err(fail());
            }
        }
        let record = MemoryRecord {
            namespace: r.namespace,
            timestamp_ms: r.timestamp_ms,
            content: r.content,
            tags: r.tags,
            meta: r.meta,
            id: r.id,
            project: r.project,
            provenance: r.provenance.map(|p| MemoryProvenance {
                source: p.source,
                session: p.session,
            }),
            sensitivity: r.sensitivity,
            retention: r.retention,
        };
        let id = record.id.clone().expect("checked");
        if let Some(prior) = records.get(&id) {
            if prior != &record {
                return Err(fail());
            }
            parsed.duplicates += 1;
        } else {
            records.insert(id, record);
        }
    }
    // Tombstones are independent of record order and survive even without a
    // live ID. Suppress stale rows before sending; the target also applies all
    // tombstones first and must suppress target-side forgotten IDs on retry.
    parsed.suppressed = records
        .keys()
        .filter(|id| parsed.tombstones.contains(*id))
        .count();
    parsed.records = records
        .into_iter()
        .filter(|(id, _)| !parsed.tombstones.contains(id))
        .map(|(_, r)| r)
        .collect();
    Ok(parsed)
}

/// Explicit operator source. These paths/aliases must never originate in a
/// model tool parameter. No automatic plugin discovery or prefix conversion.
pub enum MigrationSource {
    Builtin,
    Brain {
        path: std::path::PathBuf,
        project: String,
    },
    Export {
        path: std::path::PathBuf,
        project: String,
    },
}

pub async fn preview_from(
    binding: &MemoryBinding,
    source: &MigrationSource,
) -> Result<MigrationManifest> {
    match source {
        MigrationSource::Builtin => preview_in(binding.base(), binding.scope()?),
        _ => Ok(external_snapshot(binding, source).await?.manifest),
    }
}
pub async fn apply_from(
    binding: &MemoryBinding,
    source: &MigrationSource,
    digest: &str,
) -> Result<Value> {
    if matches!(source, MigrationSource::Builtin) {
        return apply(binding, digest).await;
    }
    if !is_digest(digest) {
        bail!("invalid preview manifest digest");
    }
    let snapshot = external_snapshot(binding, source).await?;
    snapshot.verify_digest(digest)?;
    // Keep export-file read lock until acknowledgement. Re-export the source
    // brain immediately before applying to detect changes after preview/current
    // read; the read-only exporter independently checks source stability.
    let current = external_snapshot(binding, source).await?;
    current.verify_digest(digest)?;
    let result = binding.rpc("migration_apply", snapshot.payload.clone()).await
        .context("migration outcome unconfirmed; source/config unchanged; retry this exact manifest to reconcile")?;
    drop(current);
    drop(snapshot);
    Ok(result)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Inventory {
    format: String,
    records: Vec<SourceRecord>,
    tombstones: Vec<String>,
    histories: Vec<Value>,
    #[serde(default)]
    captures: Vec<Value>,
    #[serde(default)]
    fingerprints: Vec<Value>,
    #[serde(default)]
    source_digest: Option<String>,
    source_project: String,
    target_project: String,
}

async fn external_snapshot(binding: &MemoryBinding, source: &MigrationSource) -> Result<Snapshot> {
    let scope = binding.scope()?;
    let (path, alias, kind) = match source {
        MigrationSource::Brain { path, project } => (path, project, "legacy_plugin_brain"),
        MigrationSource::Export { path, project } => (path, project, "operator_private_export"),
        MigrationSource::Builtin => unreachable!(),
    };
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        || alias.is_empty()
        || alias.len() > 4096
        || alias.chars().any(char::is_control)
    {
        bail!(
            "source requires an absolute no-traversal path and exact bounded source project alias"
        );
    }
    let (file, raw) = match source {
        MigrationSource::Brain { .. } => {
            let value = binding.rpc("legacy_export", json!({"source_brain":path,"source_project":alias})).await
                .context("read-only legacy export refused; stop writers/checkpoint source, verify authoritative source alias and supported policies")?;
            (None, serde_json::to_vec(&value)?)
        }
        MigrationSource::Export { .. } => {
            let mut file = open_export(path)?;
            if !fs4::fs_std::FileExt::try_lock_shared(&file)? {
                bail!("source export locked");
            }
            let raw = read_bounded(&mut file)?;
            (Some(file), raw)
        }
        _ => unreachable!(),
    };
    if raw.len() > MAX_SOURCE_BYTES {
        bail!("source export exceeds 24 MiB; no truncation supported");
    }
    let inventory: Inventory = serde_json::from_slice(&raw).map_err(|_| {
        anyhow::anyhow!(
            "invalid/unsupported source export schema; full records/tombstones/histories required"
        )
    })?;
    if inventory.format != "synaps-axel-export/1"
        || inventory
            .source_digest
            .as_deref()
            .is_some_and(|d| !is_digest(d))
        || inventory.source_project != *alias
        || inventory.target_project != scope.key()
        // A host-looking original owner is never a plugin retargeting grant,
        // even when the inventory has only tombstones/fingerprints or no bodies.
        || (host_project(alias) && alias != scope.key())
        || (alias != scope.key() && (!inventory.captures.is_empty() || !inventory.histories.is_empty()))
    {
        bail!("invalid source export identity or source digest");
    }
    for fingerprint in &inventory.fingerprints {
        if !fingerprint.as_object().is_some_and(|o| o.len() == 2)
            || !matches!(
                fingerprint["kind"].as_str(),
                Some("note" | "capture" | "history")
            )
            || !fingerprint["digest"].as_str().is_some_and(is_digest)
        {
            bail!("invalid exported deletion fingerprint");
        }
    }
    let mut records = BTreeMap::new();
    let tombstones: BTreeSet<_> = inventory.tombstones.into_iter().collect();
    if tombstones.iter().any(|s| !valid_id(s)) {
        bail!("invalid exported tombstone ID");
    }
    for record in inventory.records {
        let r = validate_export_record(record, scope, alias)?;
        let id = r.id.clone().expect("validated");
        if let Some(prior) = records.insert(id, r.clone()) {
            if prior != r {
                bail!("conflicting exported records");
            }
        }
    }
    let suppressed = records.keys().filter(|id| tombstones.contains(*id)).count();
    let records: Vec<_> = records
        .into_iter()
        .filter(|(id, _)| !tombstones.contains(id))
        .map(|(_, r)| r)
        .collect();
    for record in &records {
        if let Some(id) = record
            .meta
            .as_ref()
            .and_then(|m| m.get("capture_id"))
            .and_then(Value::as_str)
        {
            if !inventory
                .captures
                .iter()
                .any(|c| c["capture_id"] == id && c["note_id"].as_str() == record.id.as_deref())
            {
                bail!("capture-derived note is missing its structured capture evidence");
            }
        }
    }
    if records.len() + tombstones.len() > 16384
        || inventory.histories.len() > 128
        || inventory.captures.len() > 16384
        || inventory.fingerprints.len() > 32768
    {
        bail!("export exceeds single-transaction record limits; no truncation supported");
    }
    // Core validates/recomputes original legacy archive projections. This
    // inventory is kept byte/logically exact; service validation before its one
    // transaction is authoritative for imported histories and capture policies.
    let mut histories = inventory.histories;
    let history_tombstones = histories.iter().filter(|h| h["tombstone"] == true).count();
    for h in &histories {
        if !h.is_object()
            || !h["id"]
                .as_str()
                .is_some_and(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            || !h["digest"].as_str().is_some_and(is_digest)
            || !h["logical_id"].as_str().is_some_and(is_digest)
        {
            bail!("unsupported history export identity/digest");
        }
    }
    histories.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    let archive_inventory_bytes = serde_json::to_vec(&histories)?.len();
    let digest = hash_parts(&[
        FORMAT.as_bytes(),
        kind.as_bytes(),
        path.as_os_str().as_encoded_bytes(),
        alias.as_bytes(),
        scope.key().as_bytes(),
        &raw,
    ]);
    let migration_id = hash_parts(&[FORMAT.as_bytes(), b"apply", digest.as_bytes()]);
    let mut manifest = MigrationManifest {
        format: FORMAT,
        source: kind,
        project: scope.key().into(),
        source_project: Some(alias.clone()),
        captures: inventory.captures.len(),
        fingerprints: inventory.fingerprints.len(),
        migration_id: migration_id.clone(),
        manifest_digest: digest.clone(),
        source_jsonl_bytes: raw.len(),
        source_jsonl_present: true,
        archive_inventory_bytes,
        records: records.len(),
        tombstones: tombstones.len(),
        tombstone_suppressed_records: suppressed,
        excluded_unscoped_records: 0,
        excluded_foreign_records: 0,
        duplicate_records: 0,
        secret_records: records
            .iter()
            .filter(|r| r.sensitivity == Some(MemorySensitivity::Secret))
            .count(),
        sensitive_records: records
            .iter()
            .filter(|r| r.sensitivity == Some(MemorySensitivity::Sensitive))
            .count(),
        histories: histories.len() - history_tombstones,
        history_tombstones,
        apply_frame_bytes: 0,
    };
    let payload = json!({"migration_id":migration_id,"manifest_digest":digest,"records":records,"tombstones":tombstones,"histories":histories,"captures":inventory.captures,"fingerprints":inventory.fingerprints});
    manifest.apply_frame_bytes = check_frame_bound(scope.key(), &payload)?;
    Ok(Snapshot {
        manifest,
        payload,
        source: file,
        raw,
        histories,
    })
}

#[cfg(unix)]
pub(super) fn open_export(path: &Path) -> Result<std::fs::File> {
    use agent_core::core::private_fs::ConfinedDir;
    use std::os::unix::fs::MetadataExt;
    let parent = path.parent().context("source export parent missing")?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("invalid source export filename")?;
    let file = ConfinedDir::open_absolute_no_symlinks(parent)?.open_file(&[name.into()])?;
    let metadata = file.metadata()?;
    if metadata.mode() & 0o077 != 0 || metadata.uid() != unsafe { libc::geteuid() } {
        bail!("source export must be operator-owned mode 0600 (or stricter)");
    }
    Ok(file)
}
#[cfg(not(unix))]
pub(super) fn open_export(_: &Path) -> Result<std::fs::File> {
    bail!("private source export requires Unix")
}

fn host_project(key: &str) -> bool {
    key.len() == 17
        && key.starts_with('p')
        && key[1..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Validate provenance shape only. These retained roots/keys are DATA, never
/// authority to discover/read a path or widen service membership. In particular,
/// repository_project may differ from the record owner (e.g. user-wide notes).
fn valid_repository_provenance(value: &Value) -> bool {
    let Some(meta) = value.as_object() else {
        return false;
    };
    meta.len() == 3
        && ["repository_project", "source_worktree_project"]
            .iter()
            .all(|key| {
                meta.get(*key)
                    .and_then(Value::as_str)
                    .is_some_and(|p| host_project(p) && p != "p0000000000000000")
            })
        && meta
            .get("source_worktree_root")
            .and_then(Value::as_str)
            .is_some_and(|root| {
                !root.is_empty()
                    && root.len() <= 4096
                    && !root.contains('\0')
                    && Path::new(root).is_absolute()
                    && !Path::new(root).components().any(|c| {
                        matches!(
                            c,
                            std::path::Component::ParentDir | std::path::Component::CurDir
                        )
                    })
            })
}

fn validate_export_record(
    r: SourceRecord,
    scope: &ProjectScope,
    alias: &str,
) -> Result<MemoryRecord> {
    let invalid = || {
        anyhow::anyhow!("unsupported/conflicting exported record scope or policy; nothing imported")
    };
    // Exporter must already bind target to this EXACT host key. For plugin rows
    // its preserved _axel.source_project proves the exact operator alias mapping.
    if r.project.as_deref() != Some(scope.key())
        || !r.id.as_deref().is_some_and(valid_id)
        || r.sensitivity.is_none()
        || r.retention.is_none()
        || r.provenance.is_none()
        || r.content.len() > MAX_CONTENT_BYTES
        || r.namespace.is_empty()
        || r.namespace.len() > 64
        || r.namespace.contains('/')
        || r.namespace.contains('\\')
        || r.namespace.contains("..")
        || r.namespace.chars().any(char::is_whitespace)
        || r.timestamp_ms > i64::MAX as u64
        || chrono::DateTime::from_timestamp_millis(r.timestamp_ms as i64).is_none()
    {
        return Err(invalid());
    }
    // Forum is a reserved typed domain. Preserve original owner/digest and
    // refuse ordinary records attempting to impersonate it on import.
    let forum = r.namespace == agent_core::memory::forum::NAMESPACE
        || r.id.as_deref().is_some_and(|id| id.starts_with("msg-"))
        || r.meta
            .as_ref()
            .is_some_and(|m| m.get("_synaps_forum").is_some());
    if forum {
        let value = r
            .meta
            .as_ref()
            .and_then(|m| m.get("_synaps_forum"))
            .ok_or_else(invalid)?;
        let envelope: agent_core::memory::forum::Envelope =
            serde_json::from_value(value.clone()).map_err(|_| invalid())?;
        envelope.validate(&r.content).map_err(|_| invalid())?;
        if alias != scope.key()
            || r.namespace != agent_core::memory::forum::NAMESPACE
            || envelope.project != scope.key()
            || r.id.as_deref() != Some(envelope.id().as_str())
            || !r.tags.is_empty()
            || r.sensitivity != Some(MemorySensitivity::Normal)
            || r.retention != Some(MemoryRetention::MaxAgeDays(envelope.retention_days))
            || !r.provenance.as_ref().is_some_and(|p| {
                p.source == envelope.source()
                    && p.session.as_deref() == Some(envelope.author.group.as_str())
            })
        {
            return Err(invalid());
        }
    }
    if let Some(meta) = &r.meta {
        let m = meta.as_object().ok_or_else(invalid)?;
        let capture_keys = [
            "capture_id",
            "source_digest",
            "capture_schema",
            "local_only",
        ];
        let capture = m.contains_key("capture_id");
        let repository = m.get("_synaps_repository");
        if repository
            .is_some_and(|value| alias != scope.key() || !valid_repository_provenance(value))
            || m.keys().any(|key| {
                key != "_axel"
                    && key != "_synaps_repository"
                    && !(forum && key == "_synaps_forum")
                    && !(capture && capture_keys.contains(&key.as_str()))
            })
        {
            return Err(invalid());
        }
        if forum
            && (capture
                || repository.is_some()
                || m.keys()
                    .any(|key| !matches!(key.as_str(), "_synaps_forum" | "_axel")))
        {
            return Err(invalid());
        }
        if capture {
            let id = m
                .get("capture_id")
                .and_then(Value::as_str)
                .filter(|s| is_digest(s))
                .ok_or_else(invalid)?;
            if alias != scope.key()
                || r.namespace != "captures"
                || !r.provenance.as_ref().is_some_and(|p| {
                    p.source == "capture" && p.session.as_deref().is_some_and(|s| !s.is_empty())
                })
                || r.id.as_deref() != Some(format!("mem-cap-{id}").as_str())
                || !m
                    .get("source_digest")
                    .and_then(Value::as_str)
                    .is_some_and(is_digest)
                || !m.get("capture_schema").is_some_and(Value::is_string)
                || !m.get("local_only").is_some_and(Value::is_boolean)
            {
                return Err(invalid());
            }
        }
        let empty = serde_json::Map::new();
        let axel = match m.get("_axel") {
            Some(value) => value.as_object().ok_or_else(invalid)?,
            None if capture || forum || repository.is_some() => &empty,
            None => return Err(invalid()),
        };
        let allowed = [
            "disclosure",
            "expires_ms",
            "source_project",
            "provenance",
            "category",
            "topic",
            "title",
            "abstract",
            "source_sessions",
        ];
        if axel.keys().any(|k| !allowed.contains(&k.as_str()))
            || match axel.get("source_project").and_then(Value::as_str) {
                Some(p) => p != alias,
                None => alias != scope.key(),
            }
        {
            return Err(invalid());
        }
        if forum
            && (axel
                .keys()
                .any(|key| !matches!(key.as_str(), "disclosure" | "expires_ms"))
                || axel
                    .get("disclosure")
                    .is_some_and(|value| value != "standard"))
        {
            return Err(invalid());
        }
        match axel.get("disclosure").and_then(Value::as_str) {
            Some("standard") => {}
            None if alias == scope.key() => {}
            Some("local_only" | "visible_after_consent" | "persist_never_transmit")
                if r.sensitivity == Some(MemorySensitivity::Secret) || alias == scope.key() => {}
            _ => return Err(invalid()),
        }
        if let Some(v) = axel.get("expires_ms").filter(|v| !v.is_null()) {
            let n = v.as_i64().filter(|n| *n >= 0).ok_or_else(invalid)?;
            if chrono::DateTime::from_timestamp_millis(n).is_none() {
                return Err(invalid());
            }
            if let Some(MemoryRetention::MaxAgeDays(days)) = r.retention {
                if r.timestamp_ms.checked_add(u64::from(days) * 86_400_000) != Some(n as u64) {
                    return Err(invalid());
                }
            }
        }
    } else if alias != scope.key() {
        return Err(invalid());
    }
    if let Some(MemoryRetention::MaxAgeDays(days)) = r.retention {
        let n = r
            .timestamp_ms
            .checked_add(u64::from(days) * 86_400_000)
            .ok_or_else(invalid)?;
        if n > i64::MAX as u64 || chrono::DateTime::from_timestamp_millis(n as i64).is_none() {
            return Err(invalid());
        }
    }
    Ok(MemoryRecord {
        namespace: r.namespace,
        timestamp_ms: r.timestamp_ms,
        content: r.content,
        tags: r.tags,
        meta: r.meta,
        id: r.id,
        project: r.project,
        provenance: r.provenance.map(|p| MemoryProvenance {
            source: p.source,
            session: p.session,
        }),
        sensitivity: r.sensitivity,
        retention: r.retention,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use agent_core::context_archive::ArchiveStore;
    use agent_core::memory::store::{self, NewMemoryRecord};
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn fixture() -> (tempfile::TempDir, ProjectScope, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let scope = ProjectScope::for_root(tmp.path()).unwrap();
        fs::create_dir(tmp.path().join("memory")).unwrap();
        let path = tmp
            .path()
            .join("memory")
            .join(format!("{}.jsonl", scope.namespace()));
        (tmp, scope, path)
    }
    fn record(scope: &ProjectScope, id: &str) -> Value {
        json!({"namespace":scope.namespace(),"timestamp_ms":1000,"content":"synthetic-secret-body", "tags":[],"id":id,"project":scope.key(),"provenance":{"source":"user"},"sensitivity":"secret","retention":"standard"})
    }
    fn write_lines(path: &Path, rows: &[Value]) {
        fs::write(
            path,
            rows.iter().map(|v| format!("{v}\n")).collect::<String>(),
        )
        .unwrap();
    }
    fn binding(tmp: &tempfile::TempDir, scope: &ProjectScope) -> MemoryBinding {
        MemoryBinding::new(
            tmp.path().to_path_buf(),
            Ok(scope.clone()),
            &agent_core::config::MemoryBackendConfig {
                kind: agent_core::config::MemoryBackendKind::Axel,
                executable: Some(tmp.path().join("must-not-execute")),
                brain: Some(tmp.path().join("must-not-create.r8")),
                user_scope: false,
            },
        )
    }
    #[tokio::test]
    async fn changed_source_refuses_before_service_and_leaves_source_intact() {
        let (tmp, scope, path) = fixture();
        write_lines(&path, &[record(&scope, "mem-a")]);
        let preview = preview_in(tmp.path(), &scope).unwrap();
        let mut changed = record(&scope, "mem-a");
        changed["content"] = json!("synthetic-changed");
        write_lines(&path, &[changed]);
        let before = fs::read(&path).unwrap();
        let error = apply(&binding(&tmp, &scope), &preview.manifest_digest)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("source changed"));
        assert_eq!(before, fs::read(&path).unwrap());
        assert!(!tmp.path().join("must-not-create.r8").exists());
    }
    #[test]
    fn tombstones_first_absent_ids_and_late_reinsert_suppressed() {
        let (tmp, scope, path) = fixture();
        write_lines(
            &path,
            &[
                json!({"tombstone":"mem-a","timestamp_ms":2}),
                record(&scope, "mem-a"),
                json!({"tombstone":"mem-absent","timestamp_ms":3}),
                record(&scope, "mem-live"),
            ],
        );
        let before = fs::read(&path).unwrap();
        let snapshot = snapshot_in(tmp.path(), &scope).unwrap();
        assert_eq!(snapshot.manifest.tombstones, 2);
        assert_eq!(snapshot.manifest.tombstone_suppressed_records, 1);
        assert_eq!(
            snapshot.payload["tombstones"],
            json!(["mem-a", "mem-absent"])
        );
        assert_eq!(snapshot.payload["records"].as_array().unwrap().len(), 1);
        assert_eq!(fs::read(&path).unwrap(), before);
        let preview = serde_json::to_string(&snapshot.manifest).unwrap();
        assert!(!preview.contains("synthetic-secret-body"));
        assert!(!preview.contains("mem-absent"));
        let again = preview_in(tmp.path(), &scope).unwrap();
        assert_eq!(snapshot.manifest, again);
        // Even a deletion timestamp change participates in the full source hash.
        write_lines(&path, &[json!({"tombstone":"mem-absent","timestamp_ms":4})]);
        assert_ne!(
            again.manifest_digest,
            preview_in(tmp.path(), &scope).unwrap().manifest_digest
        );
    }
    #[test]
    fn unscoped_excluded_foreign_never_prefix_converted_and_malformed_refused() {
        let (tmp, scope, path) = fixture();
        let mut unscoped = record(&scope, "mem-a");
        unscoped.as_object_mut().unwrap().remove("project");
        let mut foreign = record(&scope, "mem-b");
        foreign["project"] = json!(format!("proj_{}", &scope.key()[1..]));
        write_lines(&path, &[unscoped, foreign]);
        let manifest = preview_in(tmp.path(), &scope).unwrap();
        assert_eq!(manifest.records, 0);
        assert_eq!(manifest.excluded_unscoped_records, 1);
        assert_eq!(manifest.excluded_foreign_records, 1);
        for row in [
            json!({"tombstone":"mem-a","timestamp_ms":2,"retention":"never_persist"}),
            {
                let mut r = record(&scope, "mem-a");
                r["meta"] = json!({"retention":"never_persist"});
                r
            },
            {
                let mut r = record(&scope, "mem-a");
                r["sensitivity"] = json!("unknown");
                r
            },
        ] {
            write_lines(&path, &[row]);
            assert!(preview_in(tmp.path(), &scope).is_err());
        }
        fs::write(&path, b"{broken secret source").unwrap();
        let error = preview_in(tmp.path(), &scope).unwrap_err().to_string();
        assert!(!error.contains("broken secret"));
    }
    #[test]
    fn history_ids_projection_hashes_hidden_notes_and_tombstones_preserved() {
        let (tmp, scope, path) = fixture();
        write_lines(&path, &[]);
        let store = ArchiveStore::new(tmp.path(), scope.key(), "synthetic-logical").unwrap();
        let a = store
            .seal(
                &[std::sync::Arc::new(
                    json!({"role":"user","content":"synthetic evidence A"}),
                )],
                "synthetic hidden note",
            )
            .unwrap();
        let b = store
            .seal(
                &[std::sync::Arc::new(
                    json!({"role":"user","content":"synthetic evidence B"}),
                )],
                "",
            )
            .unwrap();
        store.forget(&b.id).unwrap();
        let inventory = context_archive::export_in(tmp.path(), scope.key()).unwrap();
        let snapshot = snapshot_in(tmp.path(), &scope).unwrap();
        assert_eq!(snapshot.manifest.histories, 1);
        assert_eq!(snapshot.manifest.history_tombstones, 1);
        for row in inventory {
            assert!(snapshot.payload["histories"]
                .as_array()
                .unwrap()
                .contains(&row));
        }
        assert!(snapshot.payload["histories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == a.id));
        let preview = serde_json::to_string(&snapshot.manifest).unwrap();
        assert!(!preview.contains("synthetic hidden note"));
        assert!(!preview.contains("synthetic evidence"));
    }
    #[test]
    fn no_source_mkdir_and_symlink_ancestor_or_file_refused() {
        let (tmp, scope, path) = fixture();
        assert!(!tmp.path().join("context-archives").exists());
        preview_in(tmp.path(), &scope).unwrap();
        assert!(!tmp.path().join("context-archives").exists());
        assert!(!path.exists());
        let missing = tmp.path().join("absent-base");
        preview_in(&missing, &scope).unwrap();
        assert!(!missing.exists());
        let victim = tmp.path().join("victim");
        fs::write(&victim, b"private").unwrap();
        symlink(&victim, &path).unwrap();
        assert!(preview_in(tmp.path(), &scope).is_err());
        let alias = tmp.path().join("alias");
        symlink(tmp.path(), &alias).unwrap();
        assert!(preview_in(&alias, &scope).is_err());
        assert_eq!(fs::read(victim).unwrap(), b"private");
    }
    #[test]
    fn bound_is_whole_frame_and_conflicting_ids_reject() {
        let (tmp, scope, path) = fixture();
        let first = record(&scope, "mem-a");
        let mut second = first.clone();
        second["content"] = json!("different");
        write_lines(&path, &[first, second]);
        assert!(preview_in(tmp.path(), &scope).is_err());
        assert!(
            check_frame_bound(scope.key(), &json!({"body":"a".repeat(MAX_APPLY_BYTES)})).is_err()
        );
    }
    #[test]
    fn forum_export_preserves_identity_and_refuses_forgery() {
        use agent_core::memory::forum::{Author, Envelope, Post};
        let (_tmp, scope, _) = fixture();
        let post = Post {
            request_key: "finding".into(),
            thread_id: None,
            reply_to: None,
            title: "Synthetic".into(),
            body: "Important finding".into(),
            retention_days: 30,
        };
        let e = Envelope::new(scope.key(), Author::fresh(), &post).unwrap();
        let value = json!({"namespace":"forum","timestamp_ms":1000,"content":post.body,"tags":[],"id":e.id(),"project":scope.key(),"provenance":{"source":e.source(),"session":e.author.group},"sensitivity":"normal","retention":{"max_age_days":30},"meta":{"_synaps_forum":e,"_axel":{"disclosure":"standard","expires_ms":2592001000u64}}});
        let parse = |v: Value| {
            validate_export_record(serde_json::from_value(v).unwrap(), &scope, scope.key())
        };
        let saved = parse(value.clone()).unwrap();
        assert_eq!(
            saved.meta.as_ref().unwrap()["_synaps_forum"],
            value["meta"]["_synaps_forum"]
        );
        for (field, bad) in [
            ("content", json!("forged")),
            ("namespace", json!(scope.namespace())),
            ("sensitivity", json!("secret")),
            ("tags", json!(["not allowed"])),
            ("id", json!("mem-fake")),
        ] {
            let mut v = value.clone();
            v[field] = bad;
            assert!(parse(v).is_err(), "{field}");
        }
        let mut v = value.clone();
        v["meta"]["_synaps_forum"]["digest"] = json!("a".repeat(64));
        assert!(parse(v).is_err());
        let mut v = value.clone();
        v["meta"]["_axel"]["expires_ms"] = json!(1);
        assert!(parse(v).is_err());
        let mut v = value;
        v["meta"].as_object_mut().unwrap().remove("_synaps_forum");
        assert!(parse(v).is_err());
    }

    #[tokio::test]
    async fn operator_export_preserves_capture_fingerprints_and_requires_exact_alias() {
        let (tmp, scope, _) = fixture();
        let path = tmp.path().join("source.json");
        let capture = json!({"capture_id":"a".repeat(64),"note_id":"mem-cap-synthetic","payload_digest":"b".repeat(64),"source_digest":"c".repeat(64),"evidence":null,"withheld":true,"tombstoned":true});
        let fingerprint = json!({"kind":"history","digest":"f".repeat(64)});
        let inventory = json!({"format":"synaps-axel-export/1","source_project":scope.key(),"target_project":scope.key(),"records":[record(&scope,"mem-a")],"tombstones":["mem-gone"],"histories":[],"captures":[capture],"fingerprints":[fingerprint]});
        fs::write(&path, serde_json::to_vec(&inventory).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let source = MigrationSource::Export {
            path: path.clone(),
            project: scope.key().into(),
        };
        let snapshot = external_snapshot(&binding(&tmp, &scope), &source)
            .await
            .unwrap();
        assert_eq!(snapshot.payload["captures"], inventory["captures"]);
        assert_eq!(snapshot.payload["fingerprints"], inventory["fingerprints"]);
        assert_eq!(snapshot.manifest.captures, 1);
        assert_eq!(snapshot.manifest.fingerprints, 1);
        let wrong = MigrationSource::Export {
            path,
            project: "proj_wrong".into(),
        };
        assert!(external_snapshot(&binding(&tmp, &scope), &wrong)
            .await
            .is_err());
    }
    #[tokio::test]
    async fn bodyless_export_requires_exact_format_source_and_target() {
        let (tmp, scope, _) = fixture();
        let path = tmp.path().join("bodyless.json");
        let source = MigrationSource::Export {
            path: path.clone(),
            project: scope.key().into(),
        };
        let valid = json!({"format":"synaps-axel-export/1","source_project":scope.key(),"target_project":scope.key(),"records":[],"tombstones":["mem-gone"],"histories":[],"fingerprints":[{"kind":"note","digest":"a".repeat(64)}]});
        for key in ["format", "source_project", "target_project"] {
            for missing in [true, false] {
                let mut value = valid.clone();
                if missing {
                    value.as_object_mut().unwrap().remove(key);
                } else {
                    value[key] = json!("wrong");
                }
                fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                assert!(
                    external_snapshot(&binding(&tmp, &scope), &source)
                        .await
                        .is_err(),
                    "{key}, missing={missing}"
                );
            }
        }
        fs::write(&path, serde_json::to_vec(&valid).unwrap()).unwrap();
        assert!(external_snapshot(&binding(&tmp, &scope), &source)
            .await
            .is_ok());
    }
    #[test]
    fn repository_provenance_is_preserved_as_data_not_scope_authority() {
        let (tmp, scope, _) = fixture();
        let provenance = json!({"repository_project":"p1111111111111111",
            "source_worktree_project":"p2222222222222222", "source_worktree_root":"/synthetic/moved\nroot"});
        for owner in [scope.clone(), ProjectScope::user_scope(tmp.path()).unwrap()] {
            let mut value = record(&owner, "mem-provenance");
            value["meta"] = json!({"_synaps_repository":provenance});
            let parsed = validate_export_record(
                serde_json::from_value(value.clone()).unwrap(),
                &owner,
                owner.key(),
            )
            .unwrap();
            assert_eq!(parsed.meta, Some(value["meta"].clone()));
            // Historical paths are never resolved, created, or interpreted as an alias.
            assert_eq!(parsed.project.as_deref(), Some(owner.key()));
            for bad in [
                json!({}),
                json!({"repository_project":"p0000000000000000",
                "source_worktree_project":"p2222222222222222","source_worktree_root":"/synthetic/root"}),
                {
                    let mut p = provenance.clone();
                    p["extra"] = json!(true);
                    p
                },
                {
                    let mut p = provenance.clone();
                    p["source_worktree_root"] = json!("../escape");
                    p
                },
                {
                    let mut p = provenance.clone();
                    p["source_worktree_project"] = json!("proj_wrong");
                    p
                },
            ] {
                let mut invalid = value.clone();
                invalid["meta"]["_synaps_repository"] = bad;
                assert!(validate_export_record(
                    serde_json::from_value(invalid).unwrap(),
                    &owner,
                    owner.key()
                )
                .is_err());
            }
        }
    }

    #[tokio::test]
    async fn bodyless_host_inventory_cannot_retarget_owner_even_with_matching_target() {
        let (tmp, scope, _) = fixture();
        let path = tmp.path().join("bodyless.json");
        let source = MigrationSource::Export {
            path: path.clone(),
            project: "p1111111111111111".into(),
        };
        let value = json!({"format":"synaps-axel-export/1","source_project":"p1111111111111111",
            "target_project":scope.key(),"records":[],"tombstones":["mem-gone"],"histories":[],"captures":[],"fingerprints":[]});
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(external_snapshot(&binding(&tmp, &scope), &source)
            .await
            .is_err());
    }

    #[tokio::test]
    #[ignore = "requires explicitly selected service binary; synthetic temp state only"]
    async fn real_production_note_metadata_ttl_export_import_roundtrip() {
        let (tmp, scope, _) = fixture();
        let executable = std::path::PathBuf::from(
            std::env::var_os("SYNAPS_AXEL_TEST_BIN").expect("explicit synthetic test binary"),
        );
        let make_binding = |name: &str| {
            MemoryBinding::new(
                tmp.path().into(),
                Ok(scope.clone()),
                &agent_core::config::MemoryBackendConfig {
                    kind: agent_core::config::MemoryBackendKind::Axel,
                    executable: Some(executable.clone()),
                    brain: Some(tmp.path().join(name)),
                    user_scope: false,
                },
            )
        };
        let mut source_binding = make_binding("source.r8");
        // Use the actual production store path, not a hand-sanitized export row.
        std::sync::Arc::get_mut(&mut source_binding.0)
            .unwrap()
            .repository = Some(
            ProjectScope::discover_repository_with_override(scope.root(), Some(scope.root()))
                .unwrap(),
        );
        let saved = source_binding
            .store(NewMemoryRecord {
                content: "synthetic production-shaped TTL note".into(),
                tags: vec!["synthetic".into()],
                provenance: MemoryProvenance {
                    source: "user".into(),
                    session: None,
                },
                sensitivity: MemorySensitivity::Normal,
                retention: MemoryRetention::MaxAgeDays(7),
            })
            .await
            .unwrap();
        let original = source_binding
            .rpc("export", json!({"full":true}))
            .await
            .unwrap();
        assert!(original["records"][0]["meta"]["_synaps_repository"].is_object());
        assert_eq!(
            original["records"][0]["meta"]["_axel"]["expires_ms"],
            saved.timestamp_ms + 7 * 86_400_000
        );
        let path = tmp.path().join("full.json");
        fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let source = MigrationSource::Export {
            path,
            project: scope.key().into(),
        };
        let target = make_binding("target.r8");
        let preview = preview_from(&target, &source).await.unwrap();
        apply_from(&target, &source, &preview.manifest_digest)
            .await
            .unwrap();
        assert_eq!(
            target.rpc("export", json!({"full":true})).await.unwrap(),
            original
        );
        let id = saved.id.unwrap();
        assert_eq!(
            target.fetch(&[&id]).await.unwrap()[0].timestamp_ms,
            saved.timestamp_ms
        );
        target.forget(&id).await.unwrap();
        apply_from(&target, &source, &preview.manifest_digest)
            .await
            .unwrap();
        assert!(target.fetch(&[&id]).await.is_err());
    }

    #[tokio::test]
    #[ignore = "requires explicitly selected service binary; synthetic temp state only"]
    async fn real_migration_preserves_tombstones_source_and_retry_deletion() {
        let (tmp, scope, path) = fixture();
        let record = store::store_record_in(
            tmp.path(),
            &scope,
            NewMemoryRecord {
                content: "synthetic migration content".into(),
                tags: vec![],
                provenance: MemoryProvenance {
                    source: "user".into(),
                    session: None,
                },
                sensitivity: MemorySensitivity::Normal,
                retention: MemoryRetention::Standard,
            },
        )
        .unwrap();
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        use std::io::Write;
        writeln!(f, "{{\"tombstone\":\"mem-absent\",\"timestamp_ms\":1}}").unwrap();
        drop(f);
        let before = fs::read(&path).unwrap();
        let exe = std::env::var_os("SYNAPS_AXEL_TEST_BIN").expect("explicit synthetic test binary");
        let binding = MemoryBinding::new(
            tmp.path().into(),
            Ok(scope.clone()),
            &agent_core::config::MemoryBackendConfig {
                kind: agent_core::config::MemoryBackendKind::Axel,
                executable: Some(exe.into()),
                brain: Some(tmp.path().join("target.r8")),
                user_scope: false,
            },
        );
        let preview = preview_in(tmp.path(), &scope).unwrap();
        apply(&binding, &preview.manifest_digest).await.unwrap();
        let inventory = binding.rpc("export", json!({"full":true})).await.unwrap();
        assert!(inventory["tombstones"]
            .as_array()
            .unwrap()
            .contains(&json!("mem-absent")));
        let id = record.id.as_deref().unwrap();
        binding.forget(id).await.unwrap();
        apply(&binding, &preview.manifest_digest).await.unwrap();
        assert!(binding.fetch(&[id]).await.is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }
    #[tokio::test]
    #[ignore = "requires explicitly selected service binary; synthetic temp state only"]
    async fn real_capture_export_preview_apply_query_roundtrip() {
        let (tmp, scope, _) = fixture();
        let executable = std::path::PathBuf::from(
            std::env::var_os("SYNAPS_AXEL_TEST_BIN").expect("explicit synthetic test binary"),
        );
        let make_binding = |name: &str| {
            MemoryBinding::new(
                tmp.path().into(),
                Ok(scope.clone()),
                &agent_core::config::MemoryBackendConfig {
                    kind: agent_core::config::MemoryBackendKind::Axel,
                    executable: Some(executable.clone()),
                    brain: Some(tmp.path().join(name)),
                    user_scope: false,
                },
            )
        };
        let source_binding = make_binding("source.r8");
        let target_binding = make_binding("target.r8");
        let chat = json!({"schema":"chat_turn_capture/1","capture_id":"a".repeat(64),"project_id":scope.key(),"session_id":"synthetic","turn_id":"turn1","turn_ordinal":1,"source_digest":"b".repeat(64),"user":"synthetic question","assistant":"synthetic answer","tools":[]});
        let summary = json!({"schema":"conversation_summary/1","capture_id":"7".repeat(64),"project_id":scope.key(),"source_session_id":"synthetic","source_message_count":2,"source_turn_range":{"first":0,"last":1,"digest":"8".repeat(64)},"summary":"synthetic restricted summary","summary_provider":null,"summary_model":null,"local_only":true,"prompt_stack_digest":"9".repeat(64),"redaction_policy":"policy_exclusions","content_classes":["user_text"],"summarized_at_unix_ms":store::now_ms()});
        source_binding.rpc("capture", chat).await.unwrap();
        source_binding.rpc("capture", summary).await.unwrap();
        let inventory = source_binding
            .rpc("export", json!({"full":true}))
            .await
            .unwrap();
        let path = tmp.path().join("private-export.json");
        fs::write(&path, serde_json::to_vec(&inventory).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let source = MigrationSource::Export {
            path: path.clone(),
            project: scope.key().into(),
        };
        let preview = preview_from(&target_binding, &source).await.unwrap();
        assert_eq!(preview.captures, 2);
        assert_eq!(preview.secret_records, 1);
        apply_from(&target_binding, &source, &preview.manifest_digest)
            .await
            .unwrap();
        for capture in ["a".repeat(64), "7".repeat(64)] {
            let result = target_binding
                .rpc("capture_query", json!({"capture_id":capture}))
                .await
                .unwrap();
            assert_eq!(result["committed"], true);
        }
        let roundtrip = target_binding
            .rpc("export", json!({"full":true}))
            .await
            .unwrap();
        assert_eq!(inventory["captures"], roundtrip["captures"]);
        assert_eq!(inventory["records"], roundtrip["records"]);
        assert!(target_binding
            .fetch(&[&format!("mem-cap-{}", "7".repeat(64))])
            .await
            .is_err());
        let before = fs::read(&path).unwrap();
        target_binding
            .forget(&format!("mem-cap-{}", "a".repeat(64)))
            .await
            .unwrap();
        apply_from(&target_binding, &source, &preview.manifest_digest)
            .await
            .unwrap();
        assert_eq!(
            target_binding
                .rpc("capture_query", json!({"capture_id":"a".repeat(64)}))
                .await
                .unwrap()["tombstoned"],
            true
        );
        assert_eq!(before, fs::read(&path).unwrap());
    }
}
