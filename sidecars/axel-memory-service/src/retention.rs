use crate::{
    contract::*,
    history::{ArchivedMessage, HistoryImport},
    migration::{CaptureExport, Fingerprint},
    service::{acknowledge_commit, Service},
};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sweep {
    pub max_age_days: Option<u32>,
    pub max_disk_bytes: Option<u64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Export {
    #[serde(default)]
    pub full: bool,
}
impl Service {
    pub fn stats(&self) -> Result<Value> {
        let c = self.brain.conn();
        let notes: u64 = c.query_row(
            "SELECT count(*) FROM memories WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1))",
            [&self.project],
            |r| r.get(0),
        )?;
        let history: u64 = c.query_row(
            "SELECT count(*) FROM synaps_history WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND tombstoned=0",
            [&self.project],
            |r| r.get(0),
        )?;
        let tombstones:u64=c.query_row("SELECT (SELECT count(*) FROM memory_tombstones WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)))+(SELECT count(*) FROM synaps_history WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND tombstoned=1)",[&self.project],|r|r.get(0))?;
        let bytes = self.scoped_bytes()?;
        Ok(
            json!({"notes":notes,"history":history,"tombstones":tombstones,"bytes":bytes,"bytes_kind":"scope_logical_payload","database_bytes":self.allocated_bytes()?}),
        )
    }
    fn allocated_bytes(&self) -> Result<u64> {
        let c = self.brain.conn();
        let pages: u64 = c.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let size: u64 = c.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        Ok(pages.saturating_mul(size))
    }
    // SQLite pages/FTS/free space/WAL are shared and cannot be charged to a
    // project. Disk quotas here bound logical live payloads within this group.
    fn scoped_bytes(&self) -> Result<u64> {
        let c = self.brain.conn();
        let mut total = 0u64;
        for (table, expression, filter) in [
            (
                "memories",
                "length(CAST(content AS BLOB))+length(CAST(provenance AS BLOB))",
                "1",
            ),
            (
                "synaps_history",
                "length(CAST(messages AS BLOB))+length(CAST(note AS BLOB))",
                "tombstoned=0",
            ),
            (
                "synaps_captures",
                "length(CAST(evidence AS BLOB))",
                "tombstoned=0",
            ),
        ] {
            let sql = format!("SELECT coalesce(sum({expression}),0) FROM {table} WHERE {filter} AND project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1))");
            total =
                total.saturating_add(c.query_row(&sql, [&self.project], |r| r.get::<_, u64>(0))?);
        }
        Ok(total)
    }
    pub fn sweep(&self, q: Sweep) -> Result<Value> {
        let now = chrono::Utc::now().timestamp_millis();
        self.transaction(|c|{
            let mut stmt=c.prepare("SELECT id,provenance,expires_at,length(CAST(content AS BLOB))+length(CAST(provenance AS BLOB)) FROM memories WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) ORDER BY created ASC,id ASC")?;
            let rows=stmt.query_map([&self.project],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,u64>(3)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
            drop(stmt);
            let mut survivors=Vec::new();
            let mut estimated=self.scoped_bytes()?;
            for (id,provenance,expires,bytes) in rows {
                let envelope:Value=serde_json::from_str(&provenance).map_err(|_|Error::Unsupported)?;
                let record:MemoryRecord=serde_json::from_value(envelope["record"].clone()).map_err(|_|Error::Unsupported)?;
                let expired=record.expires_ms()?.is_some_and(|n|n<=now)
                    || expires.map(|s|chrono::DateTime::parse_from_rfc3339(&s).map(|t|t.timestamp_millis()<=now).map_err(|_|Error::Unsupported)).transpose()?.unwrap_or(false);
                let aged=q.max_age_days.is_some_and(|days|record.timestamp_ms.saturating_add(u64::from(days)*86_400_000)<=now as u64);
                if expired||aged {self.delete_record(c,&id,false)?;estimated=self.scoped_bytes()?;}else{survivors.push((id,bytes));}
            }
            if let Some(target)=q.max_disk_bytes {
                for (id,_bytes) in survivors {
                    if estimated<=target{break}
                    self.delete_record(c,&id,false)?;estimated=self.scoped_bytes()?;
                }
            }
            self.apply_suppression(c)?;
            Ok(())
        })?;
        // No global VACUUM or global-size-driven deletion in a project sweep.
        acknowledge_commit(self.durable())?;
        let mut result = self.stats()?;
        if let Some(target) = q.max_disk_bytes {
            result["target_met"] = json!(self.scoped_bytes()? <= target);
        }
        Ok(result)
    }
    pub fn export(&self, q: Export) -> Result<Value> {
        export_connection(self.brain.conn(), &self.project, q)
    }
}
pub(crate) fn export_connection(
    c: &rusqlite::Connection,
    project: &str,
    q: Export,
) -> Result<Value> {
    // Inventories are ONE ORIGINAL OWNER, never an alias union. Host batches
    // scope_info.members and reauthorizes aliases separately after restore.
    let mut records = Vec::new();
    let mut stmt=c.prepare("SELECT provenance,content,retention,expires_at,tags,sensitivity,created,id FROM memories WHERE project_key=?1 ORDER BY id")?;
    let rows = stmt.query_map([project], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, String>(7)?,
        ))
    })?;
    for row in rows {
        let (p, body, disclosure, expires, tags, class, created, id) = row?;
        let envelope: Value = serde_json::from_str(&p).map_err(|_| Error::Unsupported)?;
        if envelope["schema"] != RECORD_SCHEMA {
            return Err(Error::Unsupported);
        }
        let mut record: MemoryRecord = serde_json::from_value(envelope["record"].clone())?;
        if record.project != project {
            return Err(Error::Scope);
        }
        record.validate(&record.project)?;
        if crate::forum::reserved(&record) || id.starts_with("msg-") {
            if record.id != id
                || !record.content.is_empty()
                || envelope["content_bytes"].as_u64() != Some(body.len() as u64)
                || envelope.as_object().is_none_or(|m| m.len() != 3)
            {
                return Err(Error::Unsupported);
            }
            record.content = body;
            crate::forum::validate_storage(
                &record,
                &tags,
                &class,
                &disclosure,
                expires.as_deref(),
                &created,
            )?;
            if !q.full {
                record.content.clear();
            }
        } else if q.full {
            record.content = body;
        }
        if disclosure != "standard" || expires.is_some() {
            if record.meta.is_none() {
                record.meta = Some(json!({}));
            }
            let meta = record
                .meta
                .as_mut()
                .and_then(Value::as_object_mut)
                .ok_or(Error::Unsupported)?;
            let policy = meta
                .entry("_axel")
                .or_insert(json!({}))
                .as_object_mut()
                .ok_or(Error::Unsupported)?;
            policy.insert("disclosure".into(), json!(disclosure));
            if let Some(expires) = expires {
                policy.insert(
                    "expires_ms".into(),
                    json!(chrono::DateTime::parse_from_rfc3339(&expires)
                        .map_err(|_| Error::Unsupported)?
                        .timestamp_millis()),
                );
            }
        }
        record.validate(&record.project)?;
        records.push(record);
    }
    let tombstones: Vec<String> = c
        .prepare("SELECT id FROM memory_tombstones WHERE project_key=?1 ORDER BY id")?
        .query_map([project], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    let mut histories = Vec::new();
    let mut stmt=c.prepare("SELECT id,logical,digest,source_count,messages,note,tombstoned FROM synaps_history WHERE project_key=?1 ORDER BY id")?;
    let rows = stmt.query_map([project], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, usize>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, bool>(6)?,
        ))
    })?;
    for row in rows {
        let (id, logical_id, digest, source_message_count, messages, note, tombstone) = row?;
        histories.push(HistoryImport {
            id,
            logical_id,
            digest,
            source_message_count,
            messages: if q.full {
                serde_json::from_str::<Vec<ArchivedMessage>>(&messages)?
            } else {
                Vec::new()
            },
            note: if q.full { note } else { String::new() },
            tombstone,
        });
    }
    let mut captures = Vec::new();
    let mut stmt=c.prepare("SELECT capture_id,note_id,payload_digest,source_digest,evidence,withheld,tombstoned FROM synaps_captures WHERE project_key=?1 ORDER BY capture_id")?;
    let rows = stmt.query_map(params![project], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, bool>(5)?,
            r.get::<_, bool>(6)?,
        ))
    })?;
    for row in rows {
        let (capture_id, note_id, payload_digest, source_digest, evidence, withheld, tombstoned) =
            row?;
        captures.push(CaptureExport {
            capture_id,
            note_id,
            payload_digest,
            source_digest,
            evidence: if q.full && !tombstoned {
                Some(serde_json::from_str(&evidence)?)
            } else {
                None
            },
            withheld,
            tombstoned,
        });
    }
    let fingerprints: Vec<Fingerprint> = c
        .prepare(
            "SELECT kind,digest FROM synaps_fingerprints WHERE project_key=?1 ORDER BY kind,digest",
        )?
        .query_map([project], |r| {
            Ok(Fingerprint {
                kind: r.get(0)?,
                digest: r.get(1)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(
        json!({"format":EXPORT_FORMAT,"source_project":project,"target_project":project,"records":records,"tombstones":tombstones,"histories":histories,"captures":captures,"fingerprints":fingerprints}),
    )
}
