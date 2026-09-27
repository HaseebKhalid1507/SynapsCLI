//! Service-owned tables in the *same* pinned Axel connection. No second store.
use crate::{contract::*, service::Service};
use axel_memkoshi::memory::{Memory, MemoryCategory};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

pub(crate) fn hash_parts(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for part in parts {
        h.update((part.len() as u64).to_be_bytes());
        h.update(part);
    }
    format!("{:x}", h.finalize())
}
pub(crate) fn hex(s: &str, n: usize) -> bool {
    s.len() == n
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(crate) fn initialize(c: &Connection) -> Result<()> {
    c.execute_batch("BEGIN IMMEDIATE;
      CREATE TABLE IF NOT EXISTS synaps_history(
        project_key TEXT NOT NULL,id TEXT PRIMARY KEY,logical TEXT NOT NULL,digest TEXT NOT NULL,
        fingerprint TEXT,source_count INTEGER NOT NULL,message_count INTEGER NOT NULL,
        messages TEXT NOT NULL,note TEXT NOT NULL,created_ms INTEGER NOT NULL,tombstoned INTEGER NOT NULL DEFAULT 0);
      CREATE INDEX IF NOT EXISTS synaps_history_fp ON synaps_history(project_key,fingerprint,tombstoned);
      CREATE TABLE IF NOT EXISTS synaps_captures(
        project_key TEXT NOT NULL,capture_id TEXT PRIMARY KEY,note_id TEXT NOT NULL UNIQUE,
        payload_digest TEXT NOT NULL,source_digest TEXT NOT NULL,evidence TEXT NOT NULL,
        withheld INTEGER NOT NULL,tombstoned INTEGER NOT NULL DEFAULT 0);
      CREATE TABLE IF NOT EXISTS synaps_fingerprints(
        project_key TEXT NOT NULL,kind TEXT NOT NULL,digest TEXT NOT NULL,
        PRIMARY KEY(project_key,kind,digest));
      CREATE TABLE IF NOT EXISTS synaps_migrations(
        project_key TEXT NOT NULL,migration_id TEXT PRIMARY KEY,manifest_digest TEXT NOT NULL,
        committed INTEGER NOT NULL DEFAULT 0,counts TEXT,apply_digest TEXT);
      COMMIT;")?;
    Ok(())
}
impl Service {
    pub(crate) fn check_id_owner(
        &self,
        c: &Connection,
        table: &str,
        column: &str,
        id: &str,
    ) -> Result<bool> {
        let owner: Option<String> = c
            .query_row(
                &format!("SELECT project_key FROM {table} WHERE {column}=?1"),
                [id],
                |r| r.get(0),
            )
            .optional()?;
        match owner {
            Some(owner) if !crate::scope::same_scope(c, &owner, &self.project)? => {
                Err(Error::Conflict)
            }
            Some(_) => Ok(true),
            None => Ok(false),
        }
    }

    pub(crate) fn transaction<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let tx = self.brain.conn().unchecked_transaction()?;
        let result = f(&tx)?;
        tx.commit()?;
        Ok(result)
    }
    pub(crate) fn note_fingerprint(record: &MemoryRecord) -> Option<String> {
        // Only suppress identical evidence with the same source identity; ordinary
        // independently useful notes with matching text must remain independent.
        record.provenance.session.as_ref().map(|session| {
            hash_parts(&[
                record.namespace.as_bytes(),
                record.provenance.source.as_bytes(),
                session.as_bytes(),
                record.content.as_bytes(),
            ])
        })
    }
    pub(crate) fn apply_suppression(&self, c: &Connection) -> Result<()> {
        let rows = c
            .prepare("SELECT id,provenance,content FROM memories WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1))")?
            .query_map([&self.project], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (id, p, body) in rows {
            let p: serde_json::Value = serde_json::from_str(&p)?;
            let mut r: MemoryRecord = serde_json::from_value(p["record"].clone())?;
            r.content = body;
            if let Some(fp) = Self::note_fingerprint(&r) {
                if c.query_row("SELECT EXISTS(SELECT 1 FROM synaps_fingerprints WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND kind='note' AND digest=?2)",params![self.project,fp],|r|r.get::<_,bool>(0))?{self.delete_record(c,&id,true)?;}
            }
        }
        let ids=c.prepare("SELECT note_id FROM synaps_captures WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND source_digest IN (SELECT digest FROM synaps_fingerprints WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND kind='capture')")?.query_map([&self.project],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
        for id in ids {
            self.delete_record(c, &id, true)?;
        }
        let ids=c.prepare("SELECT id FROM synaps_history WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND (digest IN (SELECT digest FROM synaps_fingerprints WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND kind='history') OR fingerprint IN (SELECT digest FROM synaps_fingerprints WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND kind='history'))")?.query_map([&self.project],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
        for id in ids {
            self.delete_history(c, &id)?;
        }
        Ok(())
    }
    pub(crate) fn insert_record(
        &self,
        c: &Connection,
        record: &MemoryRecord,
        disclosure: &str,
        replay: bool,
    ) -> Result<bool> {
        self.validate_record(record)?;
        let disclosure = record.disclosure()?.unwrap_or(disclosure);
        if crate::forum::reserved(record) && disclosure != "standard" {
            return Err(Error::Invalid);
        }
        // Global live ownership is checked even when a local tombstone exists.
        let exists = self.check_id_owner(c, "memories", "id", &record.id)?;
        if self.check_id_owner(c, "memory_tombstones", "id", &record.id)? {
            if replay {
                // Legacy ID-only tombstones acquire source evidence on replay.
                // Attribute it to the original tombstone, not the calling alias.
                if let Some(fp) = Self::note_fingerprint(record) {
                    c.execute(
                        "INSERT OR IGNORE INTO synaps_fingerprints SELECT project_key,'note',?2 FROM memory_tombstones WHERE id=?1",
                        params![record.id, fp],
                    )?;
                }
                return Ok(false);
            }
            return Err(Error::Conflict);
        }
        // Global Axel IDs are immutable. Check foreign ownership BEFORE local
        // fingerprint suppression, including replay/import paths.
        if let Some(fp) = Self::note_fingerprint(record) {
            if c.query_row("SELECT EXISTS(SELECT 1 FROM synaps_fingerprints WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND kind='note' AND digest=?2)",params![self.project,fp],|r|r.get::<_,bool>(0))? {
                return if replay {Ok(false)} else {Err(Error::Conflict)};
            }
        }
        if exists && crate::forum::reserved(record) {
            if !replay {
                return Err(Error::Conflict);
            }
            return self.forum_import_existing(c, record);
        }
        let mut metadata = record.clone();
        metadata.content.clear();
        let provenance = serde_json::to_string(
            &serde_json::json!({"schema":RECORD_SCHEMA,"record":metadata,"content_bytes":record.content.len()}),
        )?;
        let existing: Option<(String, String, String, String)> = c
            .query_row(
                "SELECT project_key,provenance,content,retention FROM memories WHERE id=?1",
                [&record.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        if let Some((project, p, body, retention)) = existing {
            if replay
                && project == record.project
                && serde_json::from_str::<serde_json::Value>(&p)?
                    == serde_json::from_str::<serde_json::Value>(&provenance)?
                && body == record.content
                && retention == disclosure
            {
                return Ok(false);
            }
            return Err(Error::Conflict);
        }
        let mut memory = Memory::new(MemoryCategory::Events, "synaps", "", record.content.clone());
        memory.id = record.id.clone();
        memory.created =
            DateTime::from_timestamp_millis(record.timestamp_ms as i64).ok_or(Error::Invalid)?;
        memory.tags = record.tags.clone();
        memory.expires_at = record
            .expires_ms()?
            .map(|n| DateTime::from_timestamp_millis(n).ok_or(Error::Invalid))
            .transpose()?;
        if let Some(signer) = self.brain.signer() {
            memory.signature = Some(signer.sign(&memory));
        }
        let sensitivity = if record.sensitivity == Sensitivity::Normal {
            "normal"
        } else {
            "secret"
        };
        // Exact pinned Axel scoped insert columns, without its nested BEGIN.
        // Atomic note+evidence+history imports require a caller-owned transaction.
        c.execute("INSERT INTO memories(id,category,topic,title,abstract_text,content,confidence,importance,source_sessions,tags,related_topics,created,updated,trust_level,signature,project_key,sensitivity,retention,provenance,expires_at,superseded_by,supersedes,contradicts,confirmed_by,invalidated_at)
          VALUES(?1,?2,?3,'','',?4,?5,?6,?7,?8,?9,?10,NULL,?11,?12,?13,?14,?15,?16,?17,NULL,'[]','[]','[]',NULL)",params![memory.id,memory.category.as_str(),memory.topic,memory.content,memory.confidence.as_str(),memory.importance,serde_json::to_string(&memory.source_sessions)?,serde_json::to_string(&memory.tags)?,serde_json::to_string(&memory.related_topics)?,memory.created.to_rfc3339(),memory.trust_level,memory.signature,record.project,sensitivity,disclosure,provenance,memory.expires_at.map(|t|t.to_rfc3339())])?;
        c.execute(
            "INSERT INTO memories_fts(mem_id,title,abstract,content) VALUES(?1,'','',?2)",
            params![
                record.id,
                if sensitivity == "normal" && disclosure == "standard" {
                    &record.content
                } else {
                    ""
                }
            ],
        )?;
        Ok(true)
    }
    pub(crate) fn delete_record(&self, c: &Connection, id: &str, import: bool) -> Result<bool> {
        if !valid_id(id) {
            return Err(Error::Invalid);
        }
        self.check_id_owner(c, "memory_tombstones", "id", id)?;
        self.check_id_owner(c, "synaps_captures", "note_id", id)?;
        let row: Option<(String, String, String)> = c
            .query_row(
                "SELECT project_key,provenance,content FROM memories WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let existed = row.is_some();
        let owner = row
            .as_ref()
            .map(|r| r.0.as_str())
            .unwrap_or(&self.project)
            .to_owned();
        if let Some((project, provenance, content)) = row {
            if !crate::scope::same_scope(c, &project, &self.project)? {
                return Err(Error::Scope);
            }
            let p: serde_json::Value = serde_json::from_str(&provenance)?;
            let mut record: MemoryRecord = serde_json::from_value(p["record"].clone())?;
            record.content = content;
            if let Some(fp) = Self::note_fingerprint(&record) {
                c.execute(
                    "INSERT OR IGNORE INTO synaps_fingerprints VALUES(?1,'note',?2)",
                    params![owner, fp],
                )?;
            }
        } else if !import {
            return Ok(false);
        }
        c.execute(
            "INSERT OR IGNORE INTO memory_tombstones(id,project_key,deleted_at) VALUES(?1,?2,?3)",
            params![id, owner, Utc::now().to_rfc3339()],
        )?;
        c.execute("INSERT OR IGNORE INTO synaps_fingerprints SELECT project_key,'capture',source_digest FROM synaps_captures WHERE note_id=?1 AND project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?2))",params![id,self.project])?;
        c.execute("UPDATE synaps_captures SET tombstoned=1,evidence='' WHERE note_id=?1 AND project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?2))",params![id,self.project])?;
        c.execute(
            "DELETE FROM memories WHERE id=?1 AND project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?2))",
            params![id, self.project],
        )?;
        c.execute("DELETE FROM memories_fts WHERE mem_id=?1", [id])?;
        // Remove graph reachability just as pinned forget_scoped does.
        for column in ["supersedes", "contradicts", "confirmed_by"] {
            c.execute(&format!("UPDATE memories SET {column}=(SELECT json_group_array(value) FROM json_each(memories.{column}) WHERE value<>?1) WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?2))"),params![id,self.project])?;
        }
        c.execute(
            "UPDATE memories SET superseded_by=NULL WHERE superseded_by=?1 AND project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?2))",
            params![id, self.project],
        )?;
        Ok(existed)
    }
}
