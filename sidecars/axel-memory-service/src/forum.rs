//! Typed forum messages in the existing Axel memories table. No separate content store.
use crate::{
    contract::{Error, MemoryRecord, Provenance, Result, Retention, Sensitivity, USER_PROJECT},
    forum_contract::{self as forum, Envelope, PostRequest, Receipt, Status},
    service::{acknowledge_commit, Service},
};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

/// All three reserved markers are independent: a partial/forged forum record
/// must not fall back to the ordinary note domain.
pub(crate) fn reserved(record: &MemoryRecord) -> bool {
    record.namespace == forum::NAMESPACE
        || record.id.starts_with("msg-")
        || record
            .meta
            .as_ref()
            .is_some_and(|m| m.get(forum::META_KEY).is_some())
}

pub(crate) fn validate_metadata(record: &MemoryRecord) -> Result<Option<Envelope>> {
    if !reserved(record) {
        return Ok(None);
    }
    let meta = record
        .meta
        .as_ref()
        .and_then(Value::as_object)
        .ok_or(Error::Invalid)?;
    if meta.keys().any(|k| k != forum::META_KEY && k != "_axel") {
        return Err(Error::Invalid);
    }
    let envelope: Envelope =
        serde_json::from_value(meta.get(forum::META_KEY).ok_or(Error::Invalid)?.clone())?;
    envelope.validate_metadata().map_err(|_| Error::Invalid)?;
    if record.namespace != forum::NAMESPACE
        || record.id != envelope.id()
        || record.project != envelope.project
        || record.provenance.source != envelope.source()
        || record.provenance.session.as_deref() != Some(envelope.author.group.as_str())
        || !record.tags.is_empty()
        || record.sensitivity != Sensitivity::Normal
        || record.retention != Retention::MaxAgeDays(envelope.retention_days)
    {
        return Err(Error::Invalid);
    }
    // Native full exports materialize the physical expiry. Accept that exact
    // policy, never an independent disclosure class or a changed lifetime.
    if let Some(policy) = meta.get("_axel") {
        let policy = policy.as_object().ok_or(Error::Invalid)?;
        if policy
            .keys()
            .any(|k| k != "disclosure" && k != "expires_ms")
            || policy.get("disclosure").is_some_and(|v| v != "standard")
        {
            return Err(Error::Invalid);
        }
        let expiry = record
            .timestamp_ms
            .checked_add(u64::from(envelope.retention_days) * 86_400_000)
            .ok_or(Error::Invalid)?;
        if policy
            .get("expires_ms")
            .is_some_and(|v| v.as_u64() != Some(expiry))
        {
            return Err(Error::Invalid);
        }
    }
    Ok(Some(envelope))
}

pub(crate) fn validate_body(record: &MemoryRecord) -> Result<()> {
    if let Some(envelope) = validate_metadata(record)? {
        envelope
            .validate(&record.content)
            .map_err(|_| Error::Invalid)?;
    }
    Ok(())
}

/// Physical Axel columns must agree too; the digest is not authentication.
/// Shared by dedicated reads and native export (which must not launder a forged row).
pub(crate) fn validate_storage(
    record: &MemoryRecord,
    tags: &str,
    class: &str,
    disclosure: &str,
    expires: Option<&str>,
    created: &str,
) -> Result<()> {
    if !reserved(record) {
        return Ok(());
    }
    let tags: Vec<String> = serde_json::from_str(tags).map_err(|_| Error::Unsupported)?;
    let expires = expires
        .map(|s| {
            chrono::DateTime::parse_from_rfc3339(s)
                .map(|t| t.timestamp_millis())
                .map_err(|_| Error::Unsupported)
        })
        .transpose()?;
    let created = chrono::DateTime::parse_from_rfc3339(created)
        .map_err(|_| Error::Unsupported)?
        .timestamp_millis();
    if tags != record.tags
        || class != "normal"
        || disclosure != "standard"
        || expires != record.expires_ms()?
        || created < 0
        || created as u64 != record.timestamp_ms
    {
        return Err(Error::Unsupported);
    }
    validate_body(record).map_err(|_| Error::Unsupported)
}

const COLUMNS: &str = "m.provenance,m.content,m.sensitivity,m.tags,m.retention,m.expires_at,m.created,m.id,m.project_key";
struct Stored {
    provenance: String,
    body: String,
    class: String,
    tags: String,
    disclosure: String,
    expires: Option<String>,
    created: String,
    id: String,
    project: String,
}
impl Stored {
    fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            provenance: r.get(0)?,
            body: r.get(1)?,
            class: r.get(2)?,
            tags: r.get(3)?,
            disclosure: r.get(4)?,
            expires: r.get(5)?,
            created: r.get(6)?,
            id: r.get(7)?,
            project: r.get(8)?,
        })
    }
    fn decode(self, service: &Service) -> Result<(MemoryRecord, Envelope)> {
        let record = service.decode(&self.provenance, self.body, &self.class)?.0;
        if record.id != self.id || record.project != self.project {
            return Err(Error::Unsupported);
        }
        validate_storage(
            &record,
            &self.tags,
            &self.class,
            &self.disclosure,
            self.expires.as_deref(),
            &self.created,
        )?;
        let envelope = validate_metadata(&record)?.ok_or(Error::Unsupported)?;
        Ok((record, envelope))
    }
}

impl Service {
    fn forum_scope(&self) -> Result<()> {
        if self.project == USER_PROJECT {
            Err(Error::Scope)
        } else {
            Ok(())
        }
    }

    fn forum_owners(&self, c: &Connection, id: &str) -> Result<(bool, bool)> {
        // Check BOTH owners before interpreting a local tombstone. A forged
        // local tombstone must not hide a foreign live-ID collision.
        let exists = self.check_id_owner(c, "memories", "id", id)?;
        let tombstone = self.check_id_owner(c, "memory_tombstones", "id", id)?;
        Ok((exists, tombstone))
    }

    fn forum_live(&self, c: &Connection, id: &str, now: i64) -> Result<(MemoryRecord, Envelope)> {
        self.forum_owners(c, id)?;
        let sql = format!(
            "SELECT {COLUMNS} FROM memories m WHERE {} AND m.id=?3",
            Self::live_sql()
        );
        c.query_row(&sql, params![self.project, now, id], Stored::row)
            .optional()?
            .ok_or(Error::NotFound)?
            .decode(self)
    }

    pub(crate) fn forum_import_existing(
        &self,
        c: &Connection,
        record: &MemoryRecord,
    ) -> Result<bool> {
        self.validate_record(record)?;
        let sql = format!("SELECT {COLUMNS} FROM memories m WHERE m.id=?1");
        let (previous, envelope) = c.query_row(&sql, [&record.id], Stored::row)?.decode(self)?;
        if Some(envelope) != validate_metadata(record)? || previous.content != record.content {
            return Err(Error::Conflict);
        }
        // Timestamp is not hashed. Keep the first service/import timestamp;
        // importing a later copy cannot extend a lifetime or refresh a replay.
        if previous
            .expires_ms()?
            .is_some_and(|n| n <= Utc::now().timestamp_millis())
        {
            self.delete_record(c, &record.id, false)?;
        }
        Ok(false)
    }

    pub fn forum_post(&self, request: PostRequest) -> Result<Receipt> {
        self.forum_scope()?;
        let envelope = Envelope::new(&self.project, request.author, &request.post)
            .map_err(|_| Error::Invalid)?;
        let now = Utc::now().timestamp_millis();
        let record = MemoryRecord {
            namespace: forum::NAMESPACE.into(),
            timestamp_ms: now as u64,
            content: request.post.body,
            tags: Vec::new(),
            meta: Some(json!({(forum::META_KEY): envelope})),
            id: envelope.id(),
            project: envelope.project.clone(),
            provenance: Provenance {
                source: envelope.source(),
                session: Some(envelope.author.group.clone()),
            },
            sensitivity: Sensitivity::Normal,
            retention: Retention::MaxAgeDays(envelope.retention_days),
        };
        let receipt = |status, timestamp_ms| Receipt {
            id: envelope.id(),
            thread_id: envelope.thread_id.clone(),
            digest: envelope.digest.clone(),
            status,
            timestamp_ms,
        };
        let result = self.transaction(|c| {
            let (exists, tombstone) = self.forum_owners(c, &record.id)?;
            if tombstone {
                if exists { self.delete_record(c, &record.id, true)?; }
                return Ok(receipt(Status::Tombstoned, None));
            }
            let fp = Self::note_fingerprint(&record).ok_or(Error::Invalid)?;
            let suppressed: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM synaps_fingerprints WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND kind='note' AND digest=?2)", params![self.project, fp], |r| r.get(0))?;
            if suppressed {
                self.delete_record(c, &record.id, true)?;
                return Ok(receipt(Status::Tombstoned, None));
            }
            if exists {
                let sql = format!("SELECT {COLUMNS} FROM memories m WHERE m.id=?1");
                let (previous, prior) = c.query_row(&sql, [&record.id], Stored::row)?.decode(self)?;
                if prior != envelope || previous.content != record.content {
                    return Err(Error::Conflict);
                }
                if previous.expires_ms()?.is_some_and(|expiry| expiry <= now) {
                    self.delete_record(c, &record.id, false)?;
                    return Ok(receipt(Status::Tombstoned, None));
                }
                // Complete replay precedes reference validation: deleting a root
                // must not make its already-committed replies unreconcilable.
                return Ok(receipt(Status::Duplicate, Some(previous.timestamp_ms)));
            }
            if !envelope.is_root() {
                let (_, root) = self.forum_live(c, &envelope.thread_id, now)?;
                if !root.is_root() { return Err(Error::Invalid); }
                if let Some(parent) = &envelope.reply_to {
                    let (_, parent) = self.forum_live(c, parent, now)?;
                    if parent.thread_id != envelope.thread_id { return Err(Error::Invalid); }
                }
            }
            self.insert_record(c, &record, "standard", false)?;
            Ok(receipt(Status::Created, Some(record.timestamp_ms)))
        })?;
        acknowledge_commit(self.durable())?;
        Ok(result)
    }

    pub fn forum_read(&self, request: forum::Read) -> Result<forum::Page> {
        self.forum_scope()?;
        request.validate().map_err(|_| Error::Invalid)?;
        let c = self.brain.conn();
        // Read is not an ownership oracle: foreign and unknown threads both
        // produce an empty page through the same scoped query.
        // Shared disclosure, scope, TTL, capture and fingerprint gates are applied
        // before keyset/limit. No root-existence join: orphan replies stay readable.
        let sql = format!("SELECT {COLUMNS} FROM memories m WHERE {}
            AND json_extract(m.provenance,'$.record.namespace')='forum'
            AND ((?3 IS NULL AND json_extract(m.provenance,'$.record.meta._synaps_forum.thread_id')=m.id)
              OR json_extract(m.provenance,'$.record.meta._synaps_forum.thread_id')=?3)
            AND (?4 IS NULL OR instr(synaps_lower(m.content),?4)>0
              OR instr(synaps_lower(json_extract(m.provenance,'$.record.meta._synaps_forum.title')),?4)>0)
            AND (?5 IS NULL OR (json_extract(m.provenance,'$.record.timestamp_ms'),m.id)>(?5,?6))
            ORDER BY json_extract(m.provenance,'$.record.timestamp_ms') ASC,m.id ASC LIMIT ?7", Self::live_sql());
        let mut stmt = c.prepare(&sql)?;
        let rows = stmt.query_map(
            params![
                self.project,
                Utc::now().timestamp_millis(),
                request.thread_id,
                request.query.as_ref().map(|s| s.to_lowercase()),
                request.after.as_ref().map(|c| c.timestamp_ms as i64),
                request.after.as_ref().map(|c| c.id.as_str()),
                (request.limit + 1) as i64
            ],
            Stored::row,
        )?;
        let mut page = forum::Page {
            entries: Vec::new(),
            next: None,
        };
        let mut body_bytes = 0;
        let mut more = false;
        for row in rows {
            let (record, envelope) = row?.decode(self)?; // Digest checked on FULL body.
            if page.entries.len() == request.limit {
                more = true;
                break;
            }
            let mut body = record.content;
            let truncated = request.thread_id.is_none() && body.len() > forum::SNIPPET_BYTES;
            if truncated {
                let mut end = forum::SNIPPET_BYTES;
                while !body.is_char_boundary(end) {
                    end -= 1;
                }
                body.truncate(end);
            }
            let length = body.len();
            if body_bytes + length > forum::PAGE_BODY_BYTES {
                more = true;
                break;
            }
            let entry = forum::Entry {
                id: record.id,
                timestamp_ms: record.timestamp_ms,
                envelope,
                body,
                truncated,
            };
            page.next = Some(entry.cursor()); // Reserve cursor bytes while budgeting.
            page.entries.push(entry);
            if serde_json::to_vec(&page)?.len() > forum::PAGE_BYTES {
                page.entries.pop();
                more = true;
                break;
            }
            body_bytes += length;
        }
        if more && page.entries.is_empty() {
            return Err(Error::TooLarge);
        }
        // next is a has-more cursor, not a poll token. A fully drained caller
        // retains the last entry.cursor() for subsequent polling.
        page.next = if more {
            page.entries.last().map(forum::Entry::cursor)
        } else {
            None
        };
        Ok(page)
    }

    pub fn forum_forget(&self, id: &str) -> Result<bool> {
        self.forum_scope()?;
        if !forum::valid_id(id) {
            return Err(Error::Invalid);
        }
        let deleted = self.transaction(|c| {
            self.forum_owners(c, id)?;
            let deleted = self.delete_record(c, id, false)?;
            self.apply_suppression(c)?;
            Ok(deleted)
        })?;
        acknowledge_commit(self.durable())?;
        Ok(deleted)
    }
}
