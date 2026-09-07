use crate::{
    contract::*,
    database::{hash_parts, hex},
    service::{acknowledge_commit, Service},
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const MAX_HISTORY_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_HISTORY_RECORDS: usize = 128;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchivedMessage {
    pub source_index: usize,
    pub block_indices: Vec<usize>,
    pub message: Value,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seal {
    pub logical_id: String,
    pub source_message_count: usize,
    pub messages: Vec<ArchivedMessage>,
    pub note: String,
    pub digest: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryImport {
    pub id: String,
    pub logical_id: String,
    pub digest: String,
    #[serde(default)]
    pub source_message_count: usize,
    #[serde(default)]
    pub messages: Vec<ArchivedMessage>,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub tombstone: bool,
}
impl HistoryImport {
    fn seal(&self) -> Seal {
        Seal {
            logical_id: self.logical_id.clone(),
            source_message_count: self.source_message_count,
            messages: self.messages.clone(),
            note: self.note.clone(),
            digest: self.digest.clone(),
        }
    }
    pub(crate) fn validate(&self) -> Result<()> {
        if !hex(&self.id, 32) || !hex(&self.logical_id, 64) || !hex(&self.digest, 64) {
            return Err(Error::Invalid);
        }
        if self.tombstone {
            if !self.messages.is_empty() || !self.note.is_empty() {
                return Err(Error::Invalid);
            }
        } else {
            self.seal().validate()?;
        }
        Ok(())
    }
}
impl Seal {
    pub(crate) fn validate(&self) -> Result<()> {
        if !hex(&self.logical_id, 64) || !hex(&self.digest, 64) {
            return Err(Error::Invalid);
        }
        if self.source_message_count > 4096 || self.messages.len() > 4096 || self.note.len() > 8192
        {
            return Err(Error::TooLarge);
        }
        let bytes = serde_json::to_vec(&self.messages)?;
        if bytes.len() + self.note.len() > MAX_HISTORY_BYTES {
            return Err(Error::TooLarge);
        }
        let digest = hash_parts(&[
            self.logical_id.as_bytes(),
            &(self.source_message_count as u64).to_be_bytes(),
            &bytes,
        ]);
        if digest != self.digest {
            return Err(Error::Invalid);
        }
        let mut previous = None;
        for row in &self.messages {
            if row.source_index >= self.source_message_count
                || previous.is_some_and(|n| n >= row.source_index)
                || row.block_indices.windows(2).any(|p| p[0] >= p[1])
                || row.block_indices.iter().any(|n| *n > 1_000_000)
            {
                return Err(Error::Invalid);
            }
            previous = Some(row.source_index);
            validate_message(&row.message, &row.block_indices)?;
        }
        Ok(())
    }
    fn fingerprint(&self) -> Result<String> {
        // Drop logical ID and positional offsets, retain ordered eligible evidence.
        let values: Vec<&Value> = self.messages.iter().map(|m| &m.message).collect();
        Ok(hash_parts(&[&serde_json::to_vec(&values)?]))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistorySearch {
    #[serde(default)]
    pub query: String,
    pub limit: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryFetch {
    pub id: String,
    #[serde(default)]
    pub start: usize,
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset_bytes: usize,
}
impl Service {
    pub fn history_seal(&self, q: Seal) -> Result<Value> {
        q.validate()?;
        let result=self.transaction(|c| {
            // Check before the existing-row fast path: a different logical
            // history may have forgotten this exact projected evidence.
            if self.history_suppressed(c, &q.digest, Some(&q.fingerprint()?))? {
                return Err(Error::Conflict);
            }
            let existing:Option<(String,i64)>=c.query_row("SELECT id,tombstoned FROM synaps_history WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND logical=?2 AND digest=?3 ORDER BY tombstoned DESC,id ASC LIMIT 1",params![self.project,q.logical_id,q.digest],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            let id=if let Some((id,tomb))=existing {
                if tomb!=0{return Err(Error::Conflict)}
                id
            } else {
                let mut id=String::new();
                for _ in 0..8 {
                    let candidate=uuid::Uuid::new_v4().simple().to_string();
                    if !c.query_row("SELECT EXISTS(SELECT 1 FROM synaps_history WHERE id=?1)",[&candidate],|r|r.get::<_,bool>(0))?{id=candidate;break}
                }
                if id.is_empty(){return Err(Error::Conflict)}
                self.insert_history(c,&HistoryImport{id:id.clone(),logical_id:q.logical_id.clone(),digest:q.digest.clone(),source_message_count:q.source_message_count,messages:q.messages.clone(),note:q.note.clone(),tombstone:false})?;
                id
            };
            let (note,count,source):(String,usize,usize)=c.query_row("SELECT note,message_count,source_count FROM synaps_history WHERE id=?1 AND project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?2))",params![id,self.project],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            Ok(json!({"id":id,"message_count":count,"source_message_count":source,"note":note}))
        })?;
        acknowledge_commit(self.durable())?;
        Ok(result)
    }
    pub(crate) fn history_bounds(&self, c: &Connection) -> Result<()> {
        let (count,bytes):(usize,usize)=c.query_row("SELECT count(*),coalesce(sum(length(CAST(messages AS BLOB))+length(CAST(note AS BLOB))),0) FROM synaps_history WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1))",[&self.project],|r|Ok((r.get(0)?,r.get(1)?)))?;
        if count > MAX_HISTORY_RECORDS || bytes > 256 * 1024 * 1024 {
            return Err(Error::TooLarge);
        }
        Ok(())
    }
    fn history_suppressed(
        &self,
        c: &Connection,
        digest: &str,
        fingerprint: Option<&str>,
    ) -> Result<bool> {
        Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM synaps_history WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND tombstoned=1 AND (digest=?2 OR fingerprint=?3) UNION ALL SELECT 1 FROM synaps_fingerprints WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND kind='history' AND (digest=?2 OR digest=?3))",params![self.project,digest,fingerprint],|row|row.get::<_,bool>(0))?)
    }
    pub(crate) fn insert_history(&self, c: &Connection, r: &HistoryImport) -> Result<bool> {
        r.validate()?;
        let fingerprint = if r.tombstone {
            None
        } else {
            Some(r.seal().fingerprint()?)
        };
        let tombstone =
            r.tombstone || self.history_suppressed(c, &r.digest, fingerprint.as_deref())?;
        let prior: Option<(String, String, String, i64)> = c
            .query_row(
                "SELECT project_key,logical,digest,tombstoned FROM synaps_history WHERE id=?1",
                [&r.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        if let Some((project, logical, digest, tomb)) = prior {
            if !crate::scope::same_scope(c, &project, &self.project)?
                || logical != r.logical_id
                || digest != r.digest
            {
                return Err(Error::Conflict);
            }
            if tomb != 0 {
                // A bodyless legacy tombstone only knew the digest. Recover
                // newly supplied evidence before skipping a stale live replay,
                // retaining the tombstone's original owner across aliases.
                if let Some(fp) = &fingerprint {
                    c.execute(
                        "INSERT OR IGNORE INTO synaps_fingerprints VALUES(?1,'history',?2)",
                        params![project, fp],
                    )?;
                }
                return Ok(false);
            }
            if tombstone {
                return self.delete_history(c, &r.id);
            }
            let (messages, note): (String, String) = c.query_row(
                "SELECT messages,note FROM synaps_history WHERE id=?1",
                [&r.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if messages != serde_json::to_string(&r.messages)? || note != r.note {
                return Err(Error::Conflict);
            }
            return Ok(false);
        }
        if tombstone {
            c.execute(
                "INSERT OR IGNORE INTO synaps_fingerprints VALUES(?1,'history',?2)",
                params![self.project, r.digest],
            )?;
            if let Some(fp) = &fingerprint {
                c.execute(
                    "INSERT OR IGNORE INTO synaps_fingerprints VALUES(?1,'history',?2)",
                    params![self.project, fp],
                )?;
            }
        }
        // Restore stale live rows as body-free tombstones rather than refusing
        // the entire inventory. Identity conflicts above still fail closed.
        let messages = if tombstone {
            "[]".to_owned()
        } else {
            serde_json::to_string(&r.messages)?
        };
        let note = if tombstone { "" } else { r.note.as_str() };
        // Distinct imported IDs with the same projection are preserved. Lookup for
        // seal retries is deterministic; imported tombstones always take priority.
        c.execute("INSERT INTO synaps_history(project_key,id,logical,digest,fingerprint,source_count,message_count,messages,note,created_ms,tombstoned) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",params![self.project,r.id,r.logical_id,r.digest,fingerprint,r.source_message_count,r.messages.len(),messages,note,chrono::Utc::now().timestamp_millis(),tombstone])?;
        self.history_bounds(c)?;
        Ok(true)
    }
    pub(crate) fn delete_history(&self, c: &Connection, id: &str) -> Result<bool> {
        if !hex(id, 32) {
            return Err(Error::Invalid);
        }
        c.execute("INSERT OR IGNORE INTO synaps_fingerprints SELECT project_key,'history',digest FROM synaps_history WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND id=?2",params![self.project,id])?;
        c.execute("INSERT OR IGNORE INTO synaps_fingerprints SELECT project_key,'history',fingerprint FROM synaps_history WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND id=?2 AND fingerprint IS NOT NULL",params![self.project,id])?;
        Ok(c.execute("UPDATE synaps_history SET tombstoned=1,messages='[]',note='' WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND id=?2 AND tombstoned=0",params![self.project,id])?>0)
    }
    pub fn history_forget(&self, id: &str) -> Result<bool> {
        let result = self.transaction(|c| self.delete_history(c, id))?;
        acknowledge_commit(self.durable())?;
        Ok(result)
    }
    pub fn history_note(&self, id: &str) -> Result<String> {
        if !hex(id, 32) {
            return Err(Error::Invalid);
        }
        self.brain
            .conn()
            .query_row(
                "SELECT note FROM synaps_history WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND id=?2 AND tombstoned=0 AND digest NOT IN (SELECT digest FROM synaps_fingerprints WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND kind='history') AND (fingerprint IS NULL OR fingerprint NOT IN (SELECT digest FROM synaps_fingerprints WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND kind='history'))",
                params![self.project, id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(Error::NotFound)
    }
    pub fn history_fetch(&self, q: HistoryFetch) -> Result<Vec<ArchivedMessage>> {
        if !hex(&q.id, 32) || q.offset_bytes != 0 {
            return Err(Error::Invalid);
        }
        let text:String=self.brain.conn().query_row("SELECT messages FROM synaps_history WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND id=?2 AND tombstoned=0 AND digest NOT IN (SELECT digest FROM synaps_fingerprints WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND kind='history') AND (fingerprint IS NULL OR fingerprint NOT IN (SELECT digest FROM synaps_fingerprints WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND kind='history'))",params![self.project,q.id],|r|r.get(0)).optional()?.ok_or(Error::NotFound)?;
        let rows: Vec<ArchivedMessage> = serde_json::from_str(&text)?;
        Ok(rows
            .into_iter()
            .skip(q.start)
            .take(q.limit.unwrap_or(32).min(128))
            .collect())
    }
    pub fn history_search(&self, q: HistorySearch) -> Result<Value> {
        let mut stmt=self.brain.conn().prepare("SELECT id,message_count,source_count,messages FROM synaps_history WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND tombstoned=0 AND digest NOT IN (SELECT digest FROM synaps_fingerprints WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND kind='history') AND (fingerprint IS NULL OR fingerprint NOT IN (SELECT digest FROM synaps_fingerprints WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND kind='history')) AND instr(synaps_lower(messages),?2)>0 ORDER BY created_ms DESC,id ASC LIMIT ?3")?;
        let rows = stmt.query_map(
            params![
                self.project,
                q.query.to_lowercase(),
                q.limit.unwrap_or(8).min(32)
            ],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, usize>(1)?,
                    r.get::<_, usize>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )?;
        let mut result = Vec::new();
        for row in rows {
            let (id, count, source, body) = row?;
            result.push(json!({"id":id,"message_count":count,"source_message_count":source,"snippet":prefix(&body,256)}));
        }
        Ok(json!(result))
    }
}
pub(crate) fn prefix(s: &str, n: usize) -> &str {
    let mut end = s.len().min(n);
    while !s.is_char_boundary(end) {
        end -= 1
    }
    &s[..end]
}

// The service accepts only the host's projected wire grammar. This is defense
// in depth, not a replacement for the host redactor: free text is host-screened.
fn validate_message(value: &Value, indices: &[usize]) -> Result<()> {
    let msg = value.as_object().ok_or(Error::Invalid)?;
    if msg
        .keys()
        .any(|k| !matches!(k.as_str(), "role" | "content"))
    {
        return Err(Error::Invalid);
    }
    let role = msg
        .get("role")
        .and_then(Value::as_str)
        .ok_or(Error::Invalid)?;
    if !matches!(role, "user" | "assistant") {
        return Err(Error::Invalid);
    }
    match msg.get("content") {
        Some(Value::String(_)) if indices.is_empty() => Ok(()),
        Some(Value::Array(blocks)) if blocks.len() == indices.len() => {
            for block in blocks {
                let obj = block.as_object().ok_or(Error::Invalid)?;
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => validate_text(block)?,
                    Some("tool_use") if role == "assistant" => {
                        if obj
                            .keys()
                            .any(|k| !matches!(k.as_str(), "type" | "id" | "name" | "input"))
                            || !identifier(block, "id")
                            || !identifier(block, "name")
                        {
                            return Err(Error::Invalid);
                        }
                        let input = block.get("input").ok_or(Error::Invalid)?;
                        eligible_json(input, 0)?;
                        if block["name"] == "context_checkpoint"
                            && *input != json!({"_archive_withheld":true})
                        {
                            return Err(Error::Invalid);
                        }
                    }
                    Some("tool_result") if role == "user" => {
                        if obj.keys().any(|k| {
                            !matches!(k.as_str(), "type" | "tool_use_id" | "content" | "is_error")
                        }) || !identifier(block, "tool_use_id")
                            || block.get("is_error").is_some_and(|v| !v.is_boolean())
                        {
                            return Err(Error::Invalid);
                        }
                        match block.get("content") {
                            Some(Value::String(_)) => {}
                            Some(Value::Array(nested)) => {
                                for text in nested {
                                    validate_text(text)?;
                                }
                            }
                            _ => return Err(Error::Invalid),
                        }
                    }
                    _ => return Err(Error::Invalid),
                }
            }
            Ok(())
        }
        _ => Err(Error::Invalid),
    }
}
fn validate_text(v: &Value) -> Result<()> {
    let obj = v.as_object().ok_or(Error::Invalid)?;
    if obj.keys().any(|k| !matches!(k.as_str(), "type" | "text"))
        || v["type"] != "text"
        || !v["text"].is_string()
    {
        return Err(Error::Invalid);
    }
    Ok(())
}
fn identifier(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_str).is_some_and(|s| {
        !s.is_empty()
            && s.len() <= 128
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
    })
}
fn eligible_json(v: &Value, depth: usize) -> Result<()> {
    if depth > 32 {
        return Err(Error::TooLarge);
    }
    if let Some(obj) = v.as_object() {
        for (key, value) in obj {
            let safe = match key.as_str() {
                "channel" => value == "final",
                "content_class" => matches!(
                    value.as_str(),
                    Some("user_text" | "assistant_text" | "tool_calls" | "tool_results")
                ),
                "private" => value == false,
                "sensitivity" | "retention" | "retention_class" => matches!(
                    value.as_str(),
                    Some("normal" | "standard" | "model_visible")
                ),
                "disclosure" | "disclosure_class" => matches!(
                    value.as_str(),
                    Some("model_visible" | "model_visible_after_redaction")
                ),
                "type" => !matches!(
                    value.as_str(),
                    Some("thinking" | "redacted_thinking" | "reasoning")
                ),
                _ => true,
            };
            if !safe {
                return Err(Error::Invalid);
            }
            eligible_json(value, depth + 1)?;
        }
    } else if let Some(array) = v.as_array() {
        for value in array {
            eligible_json(value, depth + 1)?;
        }
    }
    Ok(())
}
