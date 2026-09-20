use crate::contract::*;
use crate::private_path::PrivatePath;
use axel::{project_memory as pm, r8::Brain};
use chrono::Utc;
use rusqlite::{functions::FunctionFlags, params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub(crate) const MARKER: &str = "synaps-axel/1/project";
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    schema: String,
    record: MemoryRecord,
    content_bytes: usize,
}

pub struct Service {
    pub(crate) brain: Brain,
    pub(crate) private: PrivatePath,
    pub(crate) canonical: String,
    pub(crate) members: Vec<String>,
    pub(crate) shared: bool,
    pub(crate) project: String,
}
impl Service {
    pub fn open(path: &Path, project: &str) -> Result<Self> {
        Self::open_with_user_scope(path, project, false)
    }
    pub fn open_with_user_scope(path: &Path, project: &str, user_scope: bool) -> Result<Self> {
        if !valid_project(project) || (project == USER_PROJECT) != user_scope {
            return Err(Error::Scope);
        }
        let private = PrivatePath::acquire(path)?;
        let path = &private.path;
        let exists = path.try_exists()?;
        if exists {
            // Read-only preflight BEFORE Brain::open (which changes journal mode).
            // Never migrate/adopt a legacy, unmarked or differently scoped brain.
            let c = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?;
            crate::scope::check_mode(&c, project)?;
            c.prepare("SELECT project_key,sensitivity,retention,provenance,expires_at FROM memories LIMIT 0").map_err(|_| Error::Unsupported)?;
            c.prepare("SELECT id,project_key FROM memory_tombstones LIMIT 0")
                .map_err(|_| Error::Unsupported)?;
        }
        let brain = if exists {
            Brain::open(path)?
        } else {
            Brain::create(path, Some("synaps-axel-memory-service"))?
        };
        let c = brain.conn();
        c.execute_batch("PRAGMA synchronous=FULL; PRAGMA fullfsync=ON; PRAGMA checkpoint_fullfsync=ON; PRAGMA busy_timeout=5000; PRAGMA trusted_schema=OFF;")?;
        if !exists {
            pm::migrate(c)?; // Axel schema initialization on a NEW file only.
            c.execute(
                "INSERT INTO brain_meta(key,value) VALUES(?1,?2)",
                params![crate::scope::MULTI_MARKER, "1"],
            )?;
        }
        crate::database::initialize(c)?;
        let (shared, canonical, members) = crate::scope::initialize(c, project)?;
        // Every stored title/abstract is empty. Axel's secret FTS row therefore
        // contains no indexed tokens (only its UNINDEXED mem_id).
        c.create_scalar_function(
            "synaps_lower",
            1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| Ok(ctx.get::<String>(0)?.to_lowercase()),
        )?;
        c.create_scalar_function(
            "synaps_note_fp",
            4,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let parts = (
                    ctx.get::<String>(0)?,
                    ctx.get::<String>(1)?,
                    ctx.get::<Option<String>>(2)?,
                    ctx.get::<String>(3)?,
                );
                Ok(parts.2.map(|session| {
                    crate::database::hash_parts(&[
                        parts.0.as_bytes(),
                        parts.1.as_bytes(),
                        session.as_bytes(),
                        parts.3.as_bytes(),
                    ])
                }))
            },
        )?;
        let service = Self {
            brain,
            private,
            shared,
            canonical,
            members,
            project: project.to_owned(),
        };
        service.durable()?;
        Ok(service)
    }
    pub(crate) fn durable(&self) -> Result<()> {
        let (busy, _, _): (i64, i64, i64) =
            self.brain
                .conn()
                .query_row("PRAGMA wal_checkpoint(FULL)", [], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?;
        if busy != 0 {
            return Err(Error::Storage);
        }
        self.private.sync()
    }
    pub fn store(&self, record: MemoryRecord) -> Result<MemoryRecord> {
        if crate::forum::reserved(&record) {
            return Err(Error::Invalid);
        }
        record.validate(&self.project)?;
        self.transaction(|c| self.insert_record(c, &record, "standard", false))?;
        acknowledge_commit(self.durable())?;
        Ok(record.disclosed())
    }
    // Only our versioned envelope is interpretable. Unknown disclosure classes,
    // unscoped rows and tombstones never cross this boundary. Integer TTL avoids
    // lexical RFC3339 fractional-second comparisons in upstream search/fetch.
    pub(crate) fn live_sql() -> &'static str {
        "m.project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND m.id NOT IN (SELECT id FROM memory_tombstones WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)))
         AND json_valid(m.provenance)
         AND json_extract(m.provenance,'$.schema')='synaps-axel/1'
         AND json_extract(m.provenance,'$.record.project')=m.project_key
         AND json_extract(m.provenance,'$.record.id')=m.id
         AND m.retention='standard'
         AND NOT EXISTS(SELECT 1 FROM synaps_fingerprints f WHERE f.project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=m.project_key)) AND f.kind='note' AND f.digest=synaps_note_fp(json_extract(m.provenance,'$.record.namespace'),json_extract(m.provenance,'$.record.provenance.source'),json_extract(m.provenance,'$.record.provenance.session'),m.content))
         AND NOT EXISTS(SELECT 1 FROM synaps_captures c WHERE c.project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=m.project_key)) AND c.note_id=m.id AND (c.withheld=1 OR c.tombstoned=1 OR c.source_digest IN (SELECT digest FROM synaps_fingerprints WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=m.project_key)) AND kind='capture')))
         AND ((m.sensitivity='normal' AND json_extract(m.provenance,'$.record.sensitivity')='normal')
           OR (m.sensitivity='secret' AND json_extract(m.provenance,'$.record.sensitivity') IN ('sensitive','secret')))
         AND (json_extract(m.provenance,'$.record.retention')='standard'
           OR (json_type(m.provenance,'$.record.retention.max_age_days')='integer'
             AND json_extract(m.provenance,'$.record.timestamp_ms') + json_extract(m.provenance,'$.record.retention.max_age_days')*86400000 > ?2))
         AND (m.expires_at IS NULL OR julianday(m.expires_at)>julianday(?2/1000.0,'unixepoch'))"
    }
    pub(crate) fn decode(
        &self,
        provenance: &str,
        body: String,
        stored_class: &str,
    ) -> Result<(MemoryRecord, usize)> {
        let envelope: Envelope =
            serde_json::from_str(provenance).map_err(|_| Error::Unsupported)?;
        if envelope.schema != RECORD_SCHEMA
            || !envelope.record.content.is_empty()
            || envelope.content_bytes > MAX_CONTENT
        {
            return Err(Error::Unsupported);
        }
        let mut record = envelope.record;
        self.validate_record_metadata(&record)
            .map_err(|_| Error::Unsupported)?;
        if (record.sensitivity == Sensitivity::Normal) != (stored_class == "normal") {
            return Err(Error::Unsupported);
        }
        if record.sensitivity != Sensitivity::Secret && body.len() != envelope.content_bytes {
            return Err(Error::Unsupported);
        }
        record.content = if record.sensitivity == Sensitivity::Secret {
            String::new()
        } else {
            body
        };
        crate::forum::validate_body(&record).map_err(|_| Error::Unsupported)?;
        Ok((record, envelope.content_bytes))
    }
    fn note_live_sql() -> String {
        format!(
            "{} AND json_extract(m.provenance,'$.record.namespace')<>'forum'
            AND substr(m.id,1,4)<>'msg-'
            AND json_type(m.provenance,'$.record.meta._synaps_forum') IS NULL",
            Self::live_sql()
        )
    }
    pub fn fetch(&self, ids: &[String]) -> Result<Vec<MemoryRecord>> {
        if ids.len() > 25 || ids.iter().any(|s| !valid_id(s)) {
            return Err(Error::Invalid);
        }
        let sql = format!("SELECT m.provenance, CASE WHEN json_extract(m.provenance,'$.record.sensitivity')='secret' THEN '' ELSE m.content END, m.sensitivity FROM memories m WHERE {} AND m.id=?3", Self::note_live_sql());
        let mut stmt = self.brain.conn().prepare(&sql)?;
        let mut result = Vec::new();
        let now = Utc::now().timestamp_millis();
        for id in ids {
            let row: Option<(String, String, String)> = stmt
                .query_row(params![self.project, now, id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .optional()?;
            let (meta, body, class) = row.ok_or(Error::NotFound)?;
            result.push(self.decode(&meta, body, &class)?.0);
        }
        Ok(result)
    }
    pub fn search(&self, query: Search) -> Result<Vec<MemoryDescriptor>> {
        let limit = query.limit.unwrap_or(8).min(25);
        let bytes = query.snippet_bytes.unwrap_or(160).min(400);
        if limit == 0 {
            return Ok(Vec::new());
        }
        if query.since_ms.is_some_and(|n| n > i64::MAX as u64) {
            return Ok(Vec::new());
        }
        let sql = format!("SELECT m.provenance, CASE WHEN json_extract(m.provenance,'$.record.sensitivity')='secret' THEN '' ELSE m.content END, m.sensitivity
          FROM memories m WHERE {}
          AND (?3 IS NULL OR CASE WHEN json_extract(m.provenance,'$.record.sensitivity')='secret' THEN 0 ELSE instr(synaps_lower(m.content),?3)>0 END)
          AND (?4 IS NULL OR EXISTS(SELECT 1 FROM json_each(m.tags) WHERE substr(value,1,length(?4))=?4))
          AND (?5 IS NULL OR json_extract(m.provenance,'$.record.timestamp_ms')>=?5)
          AND (?6 IS NULL OR json_extract(m.provenance,'$.record.timestamp_ms')<=?6)
          ORDER BY json_extract(m.provenance,'$.record.timestamp_ms') DESC, m.id ASC LIMIT ?7", Self::note_live_sql());
        let mut stmt = self.brain.conn().prepare(&sql)?;
        let needle = query.content_contains.map(|s| s.to_lowercase());
        let rows = stmt.query_map(
            params![
                self.project,
                Utc::now().timestamp_millis(),
                needle,
                query.tag_prefix,
                query.since_ms.map(|n| n as i64),
                query.until_ms.map(|n| n.min(i64::MAX as u64) as i64),
                limit as i64
            ],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )?;
        let mut result = Vec::new();
        for row in rows {
            let (meta, body, class) = row?;
            let (r, content_bytes) = self.decode(&meta, body, &class)?;
            let mut end = r.content.len().min(bytes);
            while !r.content.is_char_boundary(end) {
                end -= 1;
            }
            result.push(MemoryDescriptor {
                id: r.id,
                project: r.project,
                timestamp_ms: r.timestamp_ms,
                tags: r.tags,
                snippet: r.content[..end].to_owned(),
                truncated: end < r.content.len(),
                content_bytes,
                sensitivity: r.sensitivity,
                retention: r.retention,
            });
        }
        Ok(result)
    }
    pub fn forget(&self, id: &str) -> Result<bool> {
        if !valid_id(id) || id.starts_with("msg-") {
            return Err(Error::Invalid);
        }
        let result = self.transaction(|c| {
            let deleted = self.delete_record(c, id, false)?;
            // Suppression is group-wide. Materialize it before acknowledgement
            // so per-owner exports cannot contain a live capture whose derived
            // note will be suppressed when that inventory is restored.
            self.apply_suppression(c)?;
            Ok(deleted)
        })?;
        acknowledge_commit(self.durable())?;
        Ok(result)
    }
}

// A storage commit precedes checkpoint/fsync. Failure is not a refusal.
pub(crate) fn acknowledge_commit(result: Result<()>) -> Result<()> {
    result.map_err(|_| Error::CommitUnknown)
}

#[cfg(test)]
mod commit_tests {
    use super::*;
    #[test]
    fn post_commit_sync_failure_is_unknown_not_refusal() {
        assert_eq!(
            acknowledge_commit(Err(Error::Storage)),
            Err(Error::CommitUnknown)
        );
        assert_eq!(acknowledge_commit(Ok(())), Ok(()));
        assert_eq!(Error::CommitUnknown.wire()["code"], "commit_unknown");
    }
}
