//! Explicit local-operator export of a quiescent legacy scoped Axel brain.
//! Never constructs Brain, initializes schema, or opens the target database.
use crate::{contract::*, private_path::PrivatePath};
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{os::unix::fs::MetadataExt, path::PathBuf};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyExport {
    pub source_brain: PathBuf,
    pub source_project: String,
}
pub fn export(q: LegacyExport, target_project: &str) -> Result<Value> {
    if q.source_project.is_empty() || q.source_project.len() > 4096 {
        return Err(Error::Invalid);
    }
    let private = PrivatePath::acquire(&q.source_brain)?;
    // immutable=1 avoids even SHM creation/write on an operator's source. Never
    // ignore uncheckpointed WAL: require operator to stop writer and checkpoint.
    for suffix in ["-wal", "-journal"] {
        let path = PathBuf::from(format!(
            "{}{suffix}",
            private.path.to_str().ok_or(Error::UnsafePath)?
        ));
        match std::fs::metadata(path) {
            Ok(m) if m.len() > 0 => return Err(Error::Unsupported),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(Error::Storage),
        }
    }
    let before = std::fs::metadata(&private.path)?;
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut source_hash = Sha256::new();
    source_hash.update(before.len().to_be_bytes());
    let mut file = std::fs::File::open(&private.path)?;
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        source_hash.update(&buffer[..n]);
    }
    for part in [q.source_project.as_bytes(), target_project.as_bytes()] {
        source_hash.update((part.len() as u64).to_be_bytes());
        source_hash.update(part);
    }
    let source_digest = format!("{:x}", source_hash.finalize());
    let mut uri = String::from("file:");
    for b in private.path.to_str().ok_or(Error::UnsafePath)?.bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.".contains(&b) {
            uri.push(b as char)
        } else {
            uri.push_str(&format!("%{b:02X}"))
        }
    }
    uri.push_str("?immutable=1&mode=ro");
    let c = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    c.prepare("SELECT key,value FROM brain_meta LIMIT 0")
        .map_err(|_| Error::Unsupported)?;
    let native: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM brain_meta WHERE key IN ('synaps-axel/1/project','synaps-axel/2/multi-project'))", [], |r| r.get(0))?;
    if native {
        // Native payload project/digest identity cannot be retargeted. Import
        // under its original scope, then register an explicit repository alias.
        if q.source_project != target_project || !valid_project(target_project) {
            return Err(Error::Scope);
        }
        crate::scope::check_mode(&c, target_project)?;
        let mut inventory = crate::retention::export_connection(
            &c,
            target_project,
            crate::retention::Export { full: true },
        )?;
        inventory["source_digest"] = json!(source_digest);
        let after = std::fs::metadata(&private.path)?;
        if before.ino() != after.ino()
            || before.len() != after.len()
            || before.mtime() != after.mtime()
            || before.mtime_nsec() != after.mtime_nsec()
        {
            return Err(Error::Storage);
        }
        private.check_files()?;
        return Ok(inventory);
    }
    let mut stmt=c.prepare("SELECT id,category,topic,title,abstract_text,content,tags,created,project_key,sensitivity,retention,provenance,expires_at,source_sessions FROM memories WHERE project_key=?1 AND id NOT IN (SELECT id FROM memory_tombstones WHERE project_key=?1) ORDER BY id LIMIT 16385").map_err(|_|Error::Unsupported)?;
    let rows = stmt.query_map([&q.source_project], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, String>(7)?,
            r.get::<_, String>(8)?,
            r.get::<_, String>(9)?,
            r.get::<_, String>(10)?,
            r.get::<_, String>(11)?,
            r.get::<_, Option<String>>(12)?,
            r.get::<_, String>(13)?,
        ))
    })?;
    let mut records = Vec::new();
    let mut bytes = 0usize;
    for row in rows {
        let (
            id,
            category,
            topic,
            title,
            abstract_text,
            content,
            tags,
            created,
            project,
            sensitivity,
            disclosure,
            provenance,
            expires,
            sessions,
        ) = row?;
        if !matches!(sensitivity.as_str(), "normal" | "secret")
            || !matches!(
                disclosure.as_str(),
                "standard" | "local_only" | "visible_after_consent" | "persist_never_transmit"
            )
        {
            return Err(Error::Unsupported);
        }
        let timestamp_ms = chrono::DateTime::parse_from_rfc3339(&created)
            .map_err(|_| Error::Unsupported)?
            .timestamp_millis();
        if timestamp_ms < 0 {
            return Err(Error::Unsupported);
        }
        let expires_ms = expires
            .map(|s| {
                chrono::DateTime::parse_from_rfc3339(&s)
                    .map(|d| d.timestamp_millis())
                    .map_err(|_| Error::Unsupported)
            })
            .transpose()?;
        let tags: Vec<String> = serde_json::from_str(&tags).map_err(|_| Error::Unsupported)?;
        let source_sessions: Vec<String> =
            serde_json::from_str(&sessions).map_err(|_| Error::Unsupported)?;
        let record = MemoryRecord {
            namespace: format!("project-{target_project}"),
            timestamp_ms: timestamp_ms as u64,
            content,
            tags,
            meta: Some(
                json!({"_axel":{"disclosure":disclosure,"expires_ms":expires_ms,"source_project":project,"provenance":provenance,"category":category,"topic":topic,"title":title,"abstract":abstract_text,"source_sessions":source_sessions}}),
            ),
            id,
            project: target_project.to_owned(),
            provenance: Provenance {
                source: "legacy_axel".into(),
                session: source_sessions.first().cloned(),
            },
            sensitivity: if sensitivity == "secret" || disclosure != "standard" {
                Sensitivity::Secret
            } else {
                Sensitivity::Normal
            },
            retention: Retention::Standard,
        };
        record.validate(target_project)?;
        bytes = bytes.saturating_add(serde_json::to_vec(&record)?.len());
        if records.len() >= 16384 || bytes > MAX_LARGE_FRAME - 4096 {
            return Err(Error::TooLarge);
        }
        records.push(record);
    }
    let tombstones: Vec<String> = c
        .prepare("SELECT id FROM memory_tombstones WHERE project_key=?1 ORDER BY id LIMIT 16385")
        .map_err(|_| Error::Unsupported)?
        .query_map([&q.source_project], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    if records.len() + tombstones.len() > 16384 {
        return Err(Error::TooLarge);
    }
    if tombstones.iter().any(|id| !valid_id(id)) {
        return Err(Error::Unsupported);
    }
    let after = std::fs::metadata(&private.path)?;
    if before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
    {
        return Err(Error::Storage);
    }
    private.check_files()?;
    Ok(
        json!({"format":EXPORT_FORMAT,"source_project":q.source_project,"target_project":target_project,"records":records,"tombstones":tombstones,"histories":[],"captures":[],"fingerprints":[],"source_digest":source_digest}),
    )
}
