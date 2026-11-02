use crate::{
    contract::*,
    database::{hash_parts, hex},
    service::{acknowledge_commit, Service},
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const MAX_HISTORY_BYTES: usize = 16 * 1024 * 1024;
/// History rows (tombstones included) per authorized scope group.
pub const MAX_HISTORY_RECORDS: usize = 256;
/// Serialized messages+note bytes per authorized scope group.
pub const MAX_HISTORY_SCOPE_BYTES: usize = 256 * 1024 * 1024;
/// Seal eviction: rows older than this go before any superseded window.
pub const HISTORY_STALE_MS: i64 = 72 * 3_600_000;
/// Seal eviction: a logical whose latest row is younger than this is active,
/// and its first and latest rows are floor-protected.
pub const HISTORY_ACTIVE_MS: i64 = 24 * 3_600_000;
/// `brain_meta` key: how many history rows have been deleted from this brain
/// (all scopes), counted by SQLite through [`EVICTIONS_TRIGGER`]. Created at
/// `'0'` with a new brain only, together with the trigger; a brain without
/// both reports `null` ("unknown") and nothing ever creates either later.
/// Every applied migration removes the row (its source may have evicted
/// windows the inventory doesn't hold, even one with no history), so the
/// brain reads unknown too.
pub(crate) const EVICTIONS_KEY: &str = "synaps-axel/2/history-evictions";
/// `AFTER DELETE ON synaps_history` trigger that raises [`EVICTIONS_KEY`] by
/// one per deleted row, whichever binary deletes it: an older sidecar that
/// evicts without counting (a rollback) is still counted. Any other delete
/// over-counts, which only weakens "never evicted" and never fakes it.
pub(crate) const EVICTIONS_TRIGGER: &str = "synaps_history_evictions";
/// New brains only, after `synaps_history` exists: the counter row and its
/// trigger in one transaction, so one never exists without the other.
pub(crate) fn track_evictions(c: &Connection) -> Result<()> {
    let tx = c.unchecked_transaction()?;
    tx.execute("INSERT INTO brain_meta(key,value) VALUES(?1,'0')", [EVICTIONS_KEY])?;
    tx.execute_batch(&format!(
        "CREATE TRIGGER IF NOT EXISTS {EVICTIONS_TRIGGER} AFTER DELETE ON synaps_history FOR EACH ROW BEGIN UPDATE brain_meta SET value=CAST(CAST(value AS INTEGER)+1 AS TEXT) WHERE key='{EVICTIONS_KEY}'; END"
    ))?;
    tx.commit()?;
    Ok(())
}
/// The next seal-capacity victim in the scope group of ?1 at clock ?2 (ms):
/// the oldest `(created_ms, id)` row of the first non-empty class
/// 1 tombstoned, 2 stale (older than ?3 ms), 3 superseded (its logical has a
/// newer row), 4 newest (each the latest row of an idle logical), 5 floor.
/// Floor: a logical's first and latest row while that latest row is younger
/// than ?4 ms. The floor wins over stale; tombstones are never protected and
/// always go first, so the windows only consider live rows.
const EVICTION_VICTIM: &str = "WITH scoped AS (
  SELECT id,logical,tombstoned,created_ms FROM synaps_history WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1))
), live AS (
  SELECT id,created_ms,
    row_number() OVER (PARTITION BY logical ORDER BY created_ms ASC,id ASC) AS from_first,
    row_number() OVER (PARTITION BY logical ORDER BY created_ms DESC,id DESC) AS from_latest,
    max(created_ms) OVER (PARTITION BY logical) AS latest_ms
  FROM scoped WHERE tombstoned=0
), ranked AS (
  SELECT id,created_ms,1 AS class FROM scoped WHERE tombstoned=1
  UNION ALL
  SELECT id,created_ms,CASE
    WHEN (from_first=1 OR from_latest=1) AND latest_ms>?2-?4 THEN 5
    WHEN created_ms<?2-?3 THEN 2
    WHEN from_latest>1 THEN 3
    ELSE 4 END FROM live
)
SELECT id,created_ms,class FROM ranked ORDER BY class ASC,created_ms ASC,id ASC LIMIT 1";
/// One row a seal deleted for capacity (docs/specs/archive-size-fit.md).
#[derive(Debug)]
pub(crate) struct Eviction {
    pub(crate) id: String,
    /// Eviction-time clock minus the row's created_ms.
    pub(crate) age_ms: i64,
    /// tombstoned|stale|superseded|newest|floor
    pub(crate) class: &'static str,
}
impl Eviction {
    /// Diagnostic line: scope key prefix, class, whole hours of age and the
    /// evicted row ID only. Never content, note, logical ID or a path.
    pub(crate) fn log_line(&self, scope: &str) -> String {
        format!(
            "axel_history_evict scope={} class={} age_h={} id={}",
            prefix(scope, 9),
            self.class,
            self.age_ms.max(0) / 3_600_000,
            self.id
        )
    }
}
#[cfg(test)]
thread_local! {
    static EVICTION_LOG: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}
/// One stderr line per committed eviction. Stdout is the protocol channel;
/// stderr is the service's only diagnostic stream. A failed write never
/// fails the (already committed) seal.
fn log_evictions(scope: &str, evicted: &[Eviction]) {
    use std::io::Write;
    let mut stderr = std::io::stderr().lock();
    for eviction in evicted {
        let line = eviction.log_line(scope);
        let _ = writeln!(stderr, "{line}");
        #[cfg(test)]
        EVICTION_LOG.with(|log| log.borrow_mut().push(line));
    }
}
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
    /// `history_owned/1`: restrict the scan to these exact IDs before
    /// matching, ordering, the limit and snippets. `[]` finds nothing.
    pub ids: Option<Vec<String>>,
}
/// Most IDs one `history_owned/1` request may carry.
pub const MAX_OWNED_IDS: usize = 128;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryInspect {
    pub ids: Vec<String>,
    /// `history_owned/2`: `2` answers `{"entries":[..],"evictions":n|null}`;
    /// absent keeps the version 1 bare array.
    #[serde(default)]
    pub version: Option<u8>,
}
/// Exact, unique, lowercase 32-hex history IDs, at most [`MAX_OWNED_IDS`].
pub fn owned_ids(ids: &[String]) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    if ids.len() > MAX_OWNED_IDS || ids.iter().any(|id| !hex(id, 32) || !seen.insert(id)) {
        return Err(Error::Invalid);
    }
    Ok(())
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
        let (result,evicted)=self.transaction(|c| {
            // Check before the existing-row fast path: a different logical
            // history may have forgotten this exact projected evidence.
            if self.history_suppressed(c, &q.digest, Some(&q.fingerprint()?))? {
                return Err(Error::Conflict);
            }
            let existing:Option<(String,i64)>=c.query_row("SELECT id,tombstoned FROM synaps_history WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND logical=?2 AND digest=?3 ORDER BY tombstoned DESC,id ASC LIMIT 1",params![self.project,q.logical_id,q.digest],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            let mut evicted=Vec::new();
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
                // Seals evict for capacity (bounded ring); imports stay strict.
                evicted=self.evict_history_for(c,&q)?;
                self.insert_history(c,&HistoryImport{id:id.clone(),logical_id:q.logical_id.clone(),digest:q.digest.clone(),source_message_count:q.source_message_count,messages:q.messages.clone(),note:q.note.clone(),tombstone:false})?;
                id
            };
            let (note,count,source):(String,usize,usize)=c.query_row("SELECT note,message_count,source_count FROM synaps_history WHERE id=?1 AND project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?2))",params![id,self.project],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            Ok((json!({"id":id,"message_count":count,"source_message_count":source,"note":note}),evicted))
        })?;
        // Committed: report each capacity deletion before durability sync.
        log_evictions(&self.canonical, &evicted);
        acknowledge_commit(self.durable())?;
        Ok(result)
    }
    /// Make room for one incoming seal (docs/specs/archive-size-fit.md).
    fn evict_history_for(&self, c: &Connection, q: &Seal) -> Result<Vec<Eviction>> {
        let incoming = serde_json::to_vec(&q.messages)?.len() + q.note.len();
        self.evict_history(
            c,
            incoming,
            self.clock.now_ms(),
            MAX_HISTORY_RECORDS,
            MAX_HISTORY_SCOPE_BYTES,
        )
    }
    /// Delete one [`EVICTION_VICTIM`] per iteration until one more row of
    /// `incoming` bytes fits both caps, or return when the scope is empty.
    /// Every iteration deletes exactly one row or returns, so the loop ends;
    /// a seal that cannot fit alone still fails closed via history_bounds.
    /// Evicted rows are deleted, never tombstoned or fingerprinted: capacity
    /// reclamation is not a forget.
    fn evict_history(
        &self,
        c: &Connection,
        incoming: usize,
        now: i64,
        max_records: usize,
        max_bytes: usize,
    ) -> Result<Vec<Eviction>> {
        const CLASSES: [&str; 5] = ["tombstoned", "stale", "superseded", "newest", "floor"];
        let mut evicted = Vec::new();
        loop {
            let (count, bytes) = self.history_usage(c)?;
            if count < max_records && bytes + incoming <= max_bytes {
                break;
            }
            let params = params![self.project, now, HISTORY_STALE_MS, HISTORY_ACTIVE_MS];
            let victim: Option<(String, i64, usize)> = c
                .query_row(EVICTION_VICTIM, params, |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .optional()?;
            let Some((id, created_ms, class)) = victim else {
                break;
            };
            let class = *CLASSES.get(class.wrapping_sub(1)).ok_or(Error::Storage)?;
            if c.execute("DELETE FROM synaps_history WHERE id=?1 AND project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?2))", params![id, self.project])? != 1 {
                return Err(Error::Storage);
            }
            evicted.push(Eviction {
                id,
                age_ms: now.saturating_sub(created_ms),
                class,
            });
        }
        // Nothing here counts: EVICTIONS_TRIGGER raises the counter for each
        // DELETE above, in this transaction. Counting here too would count
        // each victim twice.
        Ok(evicted)
    }
    /// Row count and serialized messages+note bytes of the authorized scope group.
    fn history_usage(&self, c: &Connection) -> Result<(usize, usize)> {
        Ok(c.query_row("SELECT count(*),coalesce(sum(length(CAST(messages AS BLOB))+length(CAST(note AS BLOB))),0) FROM synaps_history WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1))",[&self.project],|r|Ok((r.get(0)?,r.get(1)?)))?)
    }
    pub(crate) fn history_bounds(&self, c: &Connection) -> Result<()> {
        let (count, bytes) = self.history_usage(c)?;
        if count > MAX_HISTORY_RECORDS || bytes > MAX_HISTORY_SCOPE_BYTES {
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
        c.execute("INSERT INTO synaps_history(project_key,id,logical,digest,fingerprint,source_count,message_count,messages,note,created_ms,tombstoned) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",params![self.project,r.id,r.logical_id,r.digest,fingerprint,r.source_message_count,r.messages.len(),messages,note,self.clock.now_ms(),tombstone])?;
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
    /// Host-only ownership view of exact IDs: the stored hashed logical ID
    /// and whether the row is live (not tombstoned, not suppressed by a
    /// deletion fingerprint). Unknown IDs are omitted. Never returns bodies.
    pub fn history_inspect(&self, q: HistoryInspect) -> Result<Value> {
        owned_ids(&q.ids)?;
        let v2 = match q.version {
            None | Some(1) => false,
            Some(2) => true,
            Some(_) => return Err(Error::Invalid),
        };
        // Entries BEFORE the count: an eviction racing this read can then only raise the count (a weakening), never pair a missing row with a stale zero. Never reorder.
        let entries = self.history_inspect_entries(&q.ids)?;
        if !v2 {
            return Ok(entries);
        }
        // Brain-wide deletion count; null unless both the row and its trigger
        // exist (a brain that predates the counter, or a half-removed one).
        let evictions: Option<String> = self
            .brain
            .conn()
            .query_row(
                "SELECT value FROM brain_meta WHERE key=?1 AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='trigger' AND name=?2 AND tbl_name='synaps_history')",
                [EVICTIONS_KEY, EVICTIONS_TRIGGER],
                |r| r.get(0),
            )
            .optional()?;
        let evictions = evictions
            .map(|text| text.parse::<u64>())
            .transpose()
            .map_err(|_| Error::Storage)?;
        Ok(json!({"entries":entries,"evictions":evictions}))
    }
    /// The version 1 `history_inspect` array.
    fn history_inspect_entries(&self, ids: &[String]) -> Result<Value> {
        if ids.is_empty() {
            return Ok(json!([]));
        }
        let mut stmt=self.brain.conn().prepare("SELECT id,logical,(tombstoned=0 AND digest NOT IN (SELECT digest FROM synaps_fingerprints WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND kind='history') AND (fingerprint IS NULL OR fingerprint NOT IN (SELECT digest FROM synaps_fingerprints WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND kind='history'))) FROM synaps_history WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND id IN (SELECT value FROM json_each(?2)) ORDER BY id ASC")?;
        let rows = stmt.query_map(params![self.project, serde_json::to_string(ids)?], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
            ))
        })?;
        let mut result = Vec::new();
        for row in rows {
            let (id, logical_id, live) = row?;
            result.push(json!({"id":id,"logical_id":logical_id,"live":live}));
        }
        Ok(json!(result))
    }
    pub fn history_search(&self, q: HistorySearch) -> Result<Value> {
        if let Some(ids) = &q.ids {
            owned_ids(ids)?;
            if ids.is_empty() {
                return Ok(json!([]));
            }
        }
        let ids = q.ids.as_ref().map(serde_json::to_string).transpose()?;
        let mut stmt=self.brain.conn().prepare("SELECT id,message_count,source_count,messages FROM synaps_history WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND tombstoned=0 AND digest NOT IN (SELECT digest FROM synaps_fingerprints WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND kind='history') AND (fingerprint IS NULL OR fingerprint NOT IN (SELECT digest FROM synaps_fingerprints WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND kind='history')) AND (?4 IS NULL OR id IN (SELECT value FROM json_each(?4))) AND instr(synaps_lower(messages),?2)>0 ORDER BY created_ms DESC,id ASC LIMIT ?3")?;
        let rows = stmt.query_map(
            params![
                self.project,
                q.query.to_lowercase(),
                q.limit.unwrap_or(8).min(32),
                ids
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

#[cfg(test)]
mod eviction_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const H: i64 = 3_600_000;
    const T0: i64 = 1_800_000_000_000;
    const PROJECT: &str = "p0123456789abcdef";

    fn open() -> (tempfile::TempDir, Service) {
        // The binary sets this umask before SQLite can create a file (main.rs);
        // the service refuses a group/world-accessible brain, WAL or SHM.
        unsafe {
            libc::umask(0o077);
        }
        // Symlink-free private base, as the service requires.
        let dir = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let service = Service::open(&dir.path().join("synthetic.r8"), PROJECT).unwrap();
        (dir, service)
    }
    fn seal(logical: &str, text: &str) -> Seal {
        let logical_id = hash_parts(&[logical.as_bytes()]);
        let messages = vec![ArchivedMessage {
            source_index: 0,
            block_indices: vec![],
            message: json!({"role":"user","content":text}),
        }];
        let bytes = serde_json::to_vec(&messages).unwrap();
        let digest = hash_parts(&[logical_id.as_bytes(), &1u64.to_be_bytes(), &bytes]);
        Seal {
            logical_id,
            source_message_count: 1,
            messages,
            note: format!("note {text}"),
            digest,
        }
    }
    /// Seal through the real operation with the service clock pinned at `at`.
    fn seal_at(s: &mut Service, at: i64, logical: &str, text: &str) -> String {
        s.clock.fixed_ms = Some(at);
        s.history_seal(seal(logical, text)).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    fn rows(s: &Service) -> Vec<String> {
        let c = s.brain.conn();
        let mut stmt = c
            .prepare("SELECT id FROM synaps_history ORDER BY created_ms,id")
            .unwrap();
        let ids = stmt.query_map([], |r| r.get(0)).unwrap();
        ids.collect::<std::result::Result<_, _>>().unwrap()
    }
    /// Run the eviction loop at `now` with explicit caps, then roll back.
    fn evict(
        s: &Service,
        now: i64,
        max_records: usize,
        max_bytes: usize,
        incoming: usize,
    ) -> Vec<(&'static str, String)> {
        let tx = s.brain.conn().unchecked_transaction().unwrap();
        let out = s
            .evict_history(&tx, incoming, now, max_records, max_bytes)
            .unwrap();
        tx.rollback().unwrap();
        out.into_iter().map(|e| (e.class, e.id)).collect()
    }
    /// The complete victim order: a record cap of 1 empties the scope.
    fn drain(s: &Service, now: i64) -> Vec<(&'static str, String)> {
        evict(s, now, 1, MAX_HISTORY_SCOPE_BYTES, 0)
    }
    fn log() -> Vec<String> {
        EVICTION_LOG.with(|l| l.borrow().clone())
    }

    #[test]
    fn victim_order_is_tombstoned_stale_superseded_newest_then_floor() {
        let (_d, mut s) = open();
        let now = T0;
        let a = seal_at(&mut s, now - 100 * H, "dead", "a");
        let o1 = seal_at(&mut s, now - 50 * H, "old", "o1");
        let o2 = seal_at(&mut s, now - 40 * H, "old", "o2");
        let i1 = seal_at(&mut s, now - 30 * H, "idle", "i1");
        let w1 = seal_at(&mut s, now - 10 * H, "worker", "w1");
        let w2 = seal_at(&mut s, now - 5 * H, "worker", "w2");
        let x = seal_at(&mut s, now - 2 * H, "forgotten", "x");
        assert!(s.history_forget(&x).unwrap());
        let w3 = seal_at(&mut s, now - H, "worker", "w3");
        assert_eq!(
            drain(&s, now),
            vec![
                ("tombstoned", x),
                ("stale", a),
                ("superseded", o1),
                ("superseded", w2),
                ("newest", o2),
                ("newest", i1),
                ("floor", w1),
                ("floor", w3),
            ]
        );
    }

    #[test]
    fn stale_goes_before_recent_superseded_but_floor_beats_stale() {
        let (_d, mut s) = open();
        let now = T0;
        // An active lineage whose FIRST row is older than 72 h.
        let f1 = seal_at(&mut s, now - 100 * H, "long", "f1");
        let stale = seal_at(&mut s, now - 73 * H, "done", "s");
        let f2 = seal_at(&mut s, now - 50 * H, "long", "f2");
        let r1 = seal_at(&mut s, now - 3 * H, "live", "r1");
        let r2 = seal_at(&mut s, now - 2 * H, "live", "r2");
        let r3 = seal_at(&mut s, now - H, "live", "r3");
        let f3 = seal_at(&mut s, now - H / 2, "long", "f3");
        // One slot at the real record cap of this scope: the stale row goes.
        assert_eq!(
            evict(&s, now, 7, MAX_HISTORY_SCOPE_BYTES, 0),
            vec![("stale", stale.clone())]
        );
        assert_eq!(
            drain(&s, now),
            vec![
                ("stale", stale),
                ("superseded", f2),
                ("superseded", r2),
                ("floor", f1),
                ("floor", r1),
                ("floor", r3),
                ("floor", f3),
            ]
        );
    }

    #[test]
    fn floor_protects_active_first_and_latest_until_the_logical_goes_idle() {
        let (_d, mut s) = open();
        let now = T0;
        let w1 = seal_at(&mut s, now - 60 * H, "worker", "w1");
        let d1 = seal_at(&mut s, now - 30 * H, "done", "d1");
        let w2 = seal_at(&mut s, now - 20 * H, "worker", "w2");
        let w3 = seal_at(&mut s, now - H, "worker", "w3");
        // Active (latest 1 h old): w1 outlives the younger idle d1.
        assert_eq!(
            drain(&s, now),
            vec![
                ("superseded", w2.clone()),
                ("newest", d1.clone()),
                ("floor", w1.clone()),
                ("floor", w3.clone()),
            ]
        );
        // 30 h later the worker's latest row is 31 h old: no floor any more.
        assert_eq!(
            drain(&s, now + 30 * H),
            vec![
                ("stale", w1),
                ("superseded", w2),
                ("newest", d1),
                ("newest", w3),
            ]
        );
    }

    #[test]
    fn cap_is_256_and_a_full_floor_scope_evicts_one_oldest_row_per_seal() {
        let (_d, mut s) = open();
        let mut ids = Vec::new();
        for i in 0..256i64 {
            let text = format!("SECRET-BODY-{i}");
            ids.push(seal_at(&mut s, T0 - 23 * H + i * 1000, &format!("s{i}"), &text));
        }
        // Row 256 fits without eviction; every row is some active logical's
        // first and latest row, so the whole scope is floor-protected.
        assert_eq!(rows(&s).len(), 256);
        assert!(log().is_empty(), "{:?}", log());
        // Row 257 evicts exactly one row: the oldest floor row.
        let newest = seal_at(&mut s, T0, "s256", "SECRET-BODY-256");
        let kept = rows(&s);
        assert_eq!(kept.len(), 256);
        assert!(!kept.contains(&ids[0]));
        assert!(kept.contains(&newest) && ids[1..].iter().all(|id| kept.contains(id)));
        assert_eq!(
            log(),
            vec![format!(
                "axel_history_evict scope=p01234567 class=floor age_h=23 id={}",
                ids[0]
            )]
        );
        // The next seal takes the next oldest, again exactly one.
        seal_at(&mut s, T0 + 1000, "s257", "SECRET-BODY-257");
        assert_eq!(rows(&s).len(), 256);
        assert!(!rows(&s).contains(&ids[1]));
        let lines = log();
        assert_eq!(lines.len(), 2);
        let logical = hash_parts(&[b"s0".as_slice()]);
        assert!(lines[1].contains("class=floor") && lines[1].contains(&ids[1]));
        assert!(lines
            .iter()
            .all(|l| !l.contains("SECRET") && !l.contains("note") && !l.contains(&logical)));
    }

    #[test]
    fn byte_cap_evicts_floor_rows_until_the_seal_fits_and_oversize_fails_closed() {
        let (_d, mut s) = open();
        let mut ids = Vec::new();
        for i in 0..4i64 {
            ids.push(seal_at(&mut s, T0 + i * 1000, &format!("b{i}"), &format!("byte-{i}")));
        }
        let (count, bytes) = s.history_usage(s.brain.conn()).unwrap();
        assert_eq!(count, 4);
        let row = bytes / 4;
        assert_eq!(row * 4, bytes, "equal-size rows");
        // Every row is floor-protected; a two-row seal under a four-row byte
        // cap evicts exactly the two oldest floor rows, one per iteration.
        assert_eq!(
            evict(&s, T0 + 5000, MAX_HISTORY_RECORDS, 4 * row, 2 * row),
            vec![("floor", ids[0].clone()), ("floor", ids[1].clone())]
        );
        // A seal that cannot fit alone empties the candidates and the loop
        // ends; history_bounds then refuses it and the transaction rolls back.
        assert_eq!(evict(&s, T0 + 5000, MAX_HISTORY_RECORDS, 4 * row, 5 * row).len(), 4);
        // At the real caps an oversize seal fails before any eviction.
        let mut big = seal("b-big", "x");
        big.messages[0].message = json!({"role":"user","content":"x".repeat(MAX_HISTORY_BYTES)});
        s.clock.fixed_ms = Some(T0 + 6000);
        assert_eq!(s.history_seal(big).unwrap_err(), Error::TooLarge);
        assert_eq!(rows(&s), ids);
        assert!(log().is_empty());
    }

    #[test]
    fn log_line_carries_scope_prefix_class_and_whole_hours_only() {
        let e = Eviction {
            id: "a".repeat(32),
            age_ms: 100 * H + 59 * 60_000,
            class: "stale",
        };
        assert_eq!(
            e.log_line(PROJECT),
            format!("axel_history_evict scope=p01234567 class=stale age_h=100 id={}", "a".repeat(32))
        );
    }

    fn counter(s: &Service) -> Option<String> {
        s.brain
            .conn()
            .query_row("SELECT value FROM brain_meta WHERE key=?1", [EVICTIONS_KEY], |r| {
                r.get(0)
            })
            .optional()
            .unwrap()
    }
    fn inspect(s: &Service, ids: &[String], version: Option<u8>) -> Value {
        s.history_inspect(HistoryInspect {
            ids: ids.to_vec(),
            version,
        })
        .unwrap()
    }
    fn trigger(s: &Service) -> bool {
        s.brain
            .conn()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='trigger' AND name='synaps_history_evictions' AND tbl_name='synaps_history')",
                [],
                |r| r.get(0),
            )
            .unwrap()
    }
    /// The 5a876241 sidecar's eviction DELETE, verbatim (its src/history.rs:291).
    /// That binary never touches the counter itself.
    const OLD_BINARY_EVICTION: &str = "DELETE FROM synaps_history WHERE id=?1 AND project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?2))";
    /// A second connection to the brain file, as another sidecar process
    /// opens it (the old binary sets the same busy_timeout and trusted_schema).
    /// It names the stock "unix" VFS: this process's default is the anchored
    /// VFS, which only opens the held directory's path, and that lock is the
    /// live service's.
    fn other_process(dir: &tempfile::TempDir) -> Connection {
        let c = Connection::open_with_flags_and_vfs(
            dir.path().join("synthetic.r8"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            "unix",
        )
        .unwrap();
        c.execute_batch("PRAGMA busy_timeout=5000; PRAGMA trusted_schema=OFF;")
            .unwrap();
        c
    }
    /// Run the eviction loop at `now` and commit it, as a seal would.
    fn evict_committed(s: &Service, now: i64, max_records: usize) -> usize {
        let tx = s.brain.conn().unchecked_transaction().unwrap();
        let out = s
            .evict_history(&tx, 0, now, max_records, MAX_HISTORY_SCOPE_BYTES)
            .unwrap();
        tx.commit().unwrap();
        out.len()
    }

    #[test]
    fn a_new_brain_counts_from_zero_and_each_eviction_adds_its_victim_count() {
        let (_d, mut s) = open();
        assert_eq!(counter(&s).as_deref(), Some("0"));
        // SQLite counts (the trigger); the eviction code adds nothing, so
        // each victim below counts exactly once.
        assert!(trigger(&s));
        for i in 0..256i64 {
            seal_at(&mut s, T0 - 23 * H + i * 1000, &format!("c{i}"), &format!("c-{i}"));
        }
        // A full ring that has not evicted yet still reads zero.
        assert_eq!(counter(&s).as_deref(), Some("0"));
        // Seal 257 evicts exactly one row, in the seal's transaction.
        seal_at(&mut s, T0, "c256", "c-256");
        assert_eq!(counter(&s).as_deref(), Some("1"));
        seal_at(&mut s, T0 + 1000, "c257", "c-257");
        assert_eq!(counter(&s).as_deref(), Some("2"));
        // A seal retry reuses its row and evicts nothing.
        seal_at(&mut s, T0 + 2000, "c257", "c-257");
        assert_eq!(counter(&s).as_deref(), Some("2"));
        // A rolled-back eviction leaves the count alone; a committed one
        // adds its whole victim count at once.
        assert_eq!(evict(&s, T0 + 3000, 250, MAX_HISTORY_SCOPE_BYTES, 0).len(), 7);
        assert_eq!(counter(&s).as_deref(), Some("2"));
        assert_eq!(evict_committed(&s, T0 + 3000, 250), 7);
        assert_eq!(counter(&s).as_deref(), Some("9"));
        assert_eq!(rows(&s).len(), 249);
        assert_eq!(inspect(&s, &[], Some(2)), json!({"entries":[],"evictions":9}));
    }

    #[test]
    fn a_brain_without_the_counter_stays_unknown_and_eviction_never_creates_it() {
        let (dir, mut s) = open();
        // A brain created before the counter: no row and no trigger.
        s.brain
            .conn()
            .execute_batch("DROP TRIGGER IF EXISTS synaps_history_evictions")
            .unwrap();
        s.brain
            .conn()
            .execute("DELETE FROM brain_meta WHERE key=?1", [EVICTIONS_KEY])
            .unwrap();
        for i in 0..3i64 {
            seal_at(&mut s, T0 + i * 1000, &format!("u{i}"), &format!("u-{i}"));
        }
        assert_eq!(evict_committed(&s, T0 + 5000, 1), 3);
        assert!(rows(&s).is_empty());
        assert_eq!(counter(&s), None);
        assert_eq!(inspect(&s, &[], Some(2)), json!({"entries":[],"evictions":null}));
        // Reopening an existing brain never adds the row or the trigger, so
        // a later delete by any binary counts nothing and it stays unknown.
        drop(s);
        let mut s = Service::open(&dir.path().join("synthetic.r8"), PROJECT).unwrap();
        assert_eq!(counter(&s), None);
        assert!(!trigger(&s));
        let id = seal_at(&mut s, T0 + 9000, "u9", "u-9");
        assert_eq!(other_process(&dir).execute(OLD_BINARY_EVICTION, params![id, PROJECT]).unwrap(), 1);
        assert_eq!(counter(&s), None);
        assert!(!trigger(&s));
        assert_eq!(inspect(&s, &[], Some(2)), json!({"entries":[],"evictions":null}));
    }

    #[test]
    fn a_raw_delete_by_a_binary_that_counts_nothing_still_counts() {
        let (dir, mut s) = open();
        assert!(trigger(&s));
        let ids: Vec<String> = (0..5i64)
            .map(|i| seal_at(&mut s, T0 + i * 1000, &format!("o{i}"), &format!("o-{i}")))
            .collect();
        assert_eq!(counter(&s).as_deref(), Some("0"));
        // The old binary's eviction DELETE, on its own connection, with no
        // counter UPDATE: SQLite runs the trigger on any connection.
        let old = other_process(&dir);
        assert_eq!(old.execute(OLD_BINARY_EVICTION, params![ids[0], PROJECT]).unwrap(), 1);
        assert_eq!(counter(&s).as_deref(), Some("1"));
        // One statement deleting two rows counts each row once.
        assert_eq!(
            old.execute("DELETE FROM synaps_history WHERE id IN (?1,?2)", params![ids[1], ids[2]])
                .unwrap(),
            2
        );
        assert_eq!(counter(&s).as_deref(), Some("3"));
        // A rolled-back delete counts nothing.
        let tx = old.unchecked_transaction().unwrap();
        tx.execute("DELETE FROM synaps_history WHERE id=?1", [&ids[3]]).unwrap();
        tx.rollback().unwrap();
        assert_eq!(counter(&s).as_deref(), Some("3"));
        drop(old);
        assert_eq!(inspect(&s, &[], Some(2)), json!({"entries":[],"evictions":3}));
        // The existing-brain open path (the old binary runs the same one)
        // accepts the trigger, keeps it, and the count keeps rising.
        drop(s);
        let s = Service::open(&dir.path().join("synthetic.r8"), PROJECT).unwrap();
        assert!(trigger(&s));
        other_process(&dir)
            .execute("DELETE FROM synaps_history WHERE id=?1", [&ids[3]])
            .unwrap();
        assert_eq!(counter(&s).as_deref(), Some("4"));
        assert_eq!(rows(&s), vec![ids[4].clone()]);
        assert_eq!(inspect(&s, &[], Some(2)), json!({"entries":[],"evictions":4}));
    }

    #[test]
    fn the_count_is_unknown_unless_both_the_row_and_the_trigger_exist() {
        // The row without the trigger: deletes would go uncounted.
        let (dir, mut s) = open();
        let a = seal_at(&mut s, T0, "k0", "k-0");
        assert_eq!(inspect(&s, &[], Some(2)), json!({"entries":[],"evictions":0}));
        assert!(trigger(&s));
        other_process(&dir)
            .execute_batch("DROP TRIGGER synaps_history_evictions")
            .unwrap();
        assert_eq!(inspect(&s, &[], Some(2)), json!({"entries":[],"evictions":null}));
        other_process(&dir)
            .execute("DELETE FROM synaps_history WHERE id=?1", [&a])
            .unwrap();
        assert_eq!(counter(&s).as_deref(), Some("0"));
        assert_eq!(inspect(&s, &[], Some(2)), json!({"entries":[],"evictions":null}));
        // The trigger without the row: nothing to count into.
        let (dir, mut s) = open();
        let b = seal_at(&mut s, T0, "k1", "k-1");
        other_process(&dir)
            .execute("DELETE FROM brain_meta WHERE key=?1", [EVICTIONS_KEY])
            .unwrap();
        assert!(trigger(&s));
        other_process(&dir)
            .execute("DELETE FROM synaps_history WHERE id=?1", [&b])
            .unwrap();
        assert_eq!(counter(&s), None);
        assert_eq!(inspect(&s, &[], Some(2)), json!({"entries":[],"evictions":null}));
    }

    /// One brain's full export applied to another, as an Axel to Axel
    /// migration does; `histories` filters what it carries.
    fn migrate(
        from: &Service,
        to: &Service,
        key: char,
        histories: impl Fn(&Value) -> bool,
    ) -> Value {
        let mut inventory = from.export(crate::retention::Export { full: true }).unwrap();
        let kept: Vec<Value> = inventory["histories"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|h| histories(h))
            .cloned()
            .collect();
        inventory["histories"] = json!(kept);
        inventory["migration_id"] = json!(key.to_string().repeat(64));
        inventory["manifest_digest"] = json!("f".repeat(64));
        to.migration_apply(serde_json::from_value(inventory).unwrap())
            .unwrap()
    }

    /// The source may have evicted windows its export no longer holds, so
    /// a brain that imports any history (live or tombstoned) can't say how
    /// many of its windows are gone: the count becomes unknown, for good.
    #[test]
    fn a_history_import_marks_the_count_unknown() {
        let (_a, mut source) = open();
        for i in 0..3i64 {
            seal_at(&mut source, T0 + i * 1000, &format!("m{i}"), &format!("m-{i}"));
        }
        assert_eq!(evict_committed(&source, T0 + 5000, 3), 1);
        assert_eq!(inspect(&source, &[], Some(2)), json!({"entries":[],"evictions":1}));
        let kept = rows(&source);
        assert_eq!(kept.len(), 2);

        // A new brain counts from zero.
        let (_b, mut target) = open();
        assert_eq!(inspect(&target, &[], Some(2)), json!({"entries":[],"evictions":0}));

        // Importing the source's history: unknown, not a false 0.
        assert_eq!(migrate(&source, &target, 'b', |_| true)["histories"], 2);
        assert_eq!(counter(&target), None);
        assert_eq!(
            inspect(&target, &kept, Some(2)),
            json!({"entries":inspect(&source, &kept, None),"evictions":null})
        );
        // Version 1 is unchanged: the same rows the source reports.
        assert_eq!(inspect(&target, &kept, None), inspect(&source, &kept, None));
        // A replay, a later seal and a later eviction never bring it back.
        assert_eq!(migrate(&source, &target, 'b', |_| true)["histories"], 2);
        seal_at(&mut target, T0 + 9000, "m9", "m-9");
        assert_eq!(evict_committed(&target, T0 + 9000, 1), 3);
        assert_eq!(counter(&target), None);
        assert_eq!(inspect(&target, &[], Some(2)), json!({"entries":[],"evictions":null}));

        // A tombstone is history too.
        assert!(source.history_forget(&kept[0]).unwrap());
        let (_c, tombs) = open();
        assert_eq!(
            migrate(&source, &tombs, 'c', |h| h["tombstone"] == json!(true))["histories"],
            1
        );
        assert_eq!(inspect(&tombs, &[], Some(2)), json!({"entries":[],"evictions":null}));

        // A brain nothing imported into still counts.
        let (_d, mut fresh) = open();
        for i in 0..2i64 {
            seal_at(&mut fresh, T0 + i * 1000, &format!("n{i}"), &format!("n-{i}"));
        }
        assert_eq!(evict_committed(&fresh, T0 + 5000, 2), 1);
        assert_eq!(inspect(&fresh, &[], Some(2)), json!({"entries":[],"evictions":1}));
    }

    /// An export can carry no history although its source evicted: victims
    /// are picked across the whole scope group and an export takes one
    /// project, so an alias member whose rows were all evicted exports
    /// `histories:[]`. Here the source evicts everything it sealed. So every
    /// applied migration marks the count unknown, with or without history.
    #[test]
    fn a_migration_without_history_marks_the_count_unknown_too() {
        let (_a, mut source) = open();
        for i in 0..2i64 {
            seal_at(&mut source, T0 + i * 1000, &format!("e{i}"), &format!("e-{i}"));
        }
        assert_eq!(evict_committed(&source, T0 + 5000, 1), 2);
        assert!(rows(&source).is_empty());
        assert_eq!(inspect(&source, &[], Some(2)), json!({"entries":[],"evictions":2}));

        let (_b, target) = open();
        assert_eq!(inspect(&target, &[], Some(2)), json!({"entries":[],"evictions":0}));
        assert_eq!(migrate(&source, &target, 'a', |_| true)["histories"], 0);
        assert_eq!(counter(&target), None);
        assert_eq!(inspect(&target, &[], Some(2)), json!({"entries":[],"evictions":null}));
        // An exact replay changes nothing.
        assert_eq!(migrate(&source, &target, 'a', |_| true)["histories"], 0);
        assert_eq!(counter(&target), None);

        // A host-built payload with nothing in it but its keys, too.
        let (_c, bare) = open();
        let empty = json!({"migration_id":"b".repeat(64),"manifest_digest":"f".repeat(64)});
        let applied = bare.migration_apply(serde_json::from_value(empty).unwrap()).unwrap();
        assert_eq!(applied, json!({"records":0,"tombstones":0,"histories":0}));
        assert_eq!(counter(&bare), None);
        assert_eq!(inspect(&bare, &[], Some(2)), json!({"entries":[],"evictions":null}));
    }

    /// Re-opening the brain after an import never recreates the row: only a
    /// new brain gets it.
    #[test]
    fn reopening_a_brain_after_an_import_keeps_the_count_unknown() {
        let (_a, mut source) = open();
        let id = seal_at(&mut source, T0, "r0", "r-0");
        let (dir, target) = open();
        assert_eq!(migrate(&source, &target, 'a', |_| true)["histories"], 1);
        assert_eq!(counter(&target), None);

        drop(target);
        let mut target = Service::open(&dir.path().join("synthetic.r8"), PROJECT).unwrap();
        assert_eq!(counter(&target), None);
        assert!(trigger(&target));
        let entries = inspect(&target, std::slice::from_ref(&id), None);
        assert_eq!(entries.as_array().map(Vec::len), Some(1));
        assert_eq!(
            inspect(&target, std::slice::from_ref(&id), Some(2)),
            json!({"entries":entries,"evictions":null})
        );
        // Nor do its later seals and evictions, before or after another open.
        seal_at(&mut target, T0 + 1000, "r1", "r-1");
        assert_eq!(evict_committed(&target, T0 + 5000, 1), 2);
        assert_eq!(counter(&target), None);
        drop(target);
        let target = Service::open(&dir.path().join("synthetic.r8"), PROJECT).unwrap();
        assert_eq!(counter(&target), None);
        assert_eq!(inspect(&target, &[], Some(2)), json!({"entries":[],"evictions":null}));
    }

    /// A migration that fails after the row is removed (here a capture that
    /// fails validation, after the history is inserted) rolls the removal
    /// back with the import: the count keeps its prior value.
    #[test]
    fn a_failed_import_rolls_back_and_keeps_the_prior_count() {
        let (_a, mut source) = open();
        let id = seal_at(&mut source, T0, "f0", "f-0");
        let (_b, mut target) = open();
        seal_at(&mut target, T0, "t0", "t-0");
        seal_at(&mut target, T0 + 1000, "t1", "t-1");
        assert_eq!(evict_committed(&target, T0 + 5000, 1), 2);
        assert_eq!(counter(&target).as_deref(), Some("2"));

        let mut inventory = source.export(crate::retention::Export { full: true }).unwrap();
        assert_eq!(inventory["histories"].as_array().map(Vec::len), Some(1));
        inventory["migration_id"] = json!("a".repeat(64));
        inventory["manifest_digest"] = json!("f".repeat(64));
        inventory["captures"] = json!([{
            "capture_id": "not-hex",
            "note_id": "mem-cap-not-hex",
            "payload_digest": "0".repeat(64),
            "source_digest": "0".repeat(64),
            "evidence": null,
            "withheld": false,
            "tombstoned": true
        }]);
        let failed = target.migration_apply(serde_json::from_value(inventory.clone()).unwrap());
        assert_eq!(failed.unwrap_err(), Error::Invalid);
        assert_eq!(counter(&target).as_deref(), Some("2"));
        assert!(rows(&target).is_empty(), "the history import rolled back too");
        assert_eq!(
            inspect(&target, std::slice::from_ref(&id), Some(2)),
            json!({"entries":[],"evictions":2})
        );

        // Nothing was recorded under the key: the corrected payload applies.
        inventory["captures"] = json!([]);
        let applied = target.migration_apply(serde_json::from_value(inventory).unwrap()).unwrap();
        assert_eq!(applied["histories"], 1);
        assert_eq!(counter(&target), None);
    }

    #[test]
    fn version_1_inspect_is_unchanged_and_version_2_adds_the_count() {
        let (_d, mut s) = open();
        let live = seal_at(&mut s, T0, "i0", "i-0");
        let gone = seal_at(&mut s, T0 + 1000, "i0", "i-1");
        assert!(s.history_forget(&gone).unwrap());
        let ids = vec![live.clone(), gone.clone(), "f".repeat(32)];
        let logical = hash_parts(&[b"i0".as_slice()]);
        let v1 = json!([
            {"id":live,"logical_id":logical,"live":true},
            {"id":gone,"logical_id":logical,"live":false},
        ]);
        let v1 = if live < gone {
            v1
        } else {
            json!([v1[1].clone(), v1[0].clone()])
        };
        // No version: exactly the version 1 bytes; an explicit 1 is the same.
        let bytes = serde_json::to_vec(&v1).unwrap();
        assert_eq!(serde_json::to_vec(&inspect(&s, &ids, None)).unwrap(), bytes);
        assert_eq!(serde_json::to_vec(&inspect(&s, &ids, Some(1))).unwrap(), bytes);
        assert_eq!(inspect(&s, &[], None), json!([]));
        // Version 2 wraps the same rows with the brain's eviction count.
        assert_eq!(
            inspect(&s, &ids, Some(2)),
            json!({"entries":v1,"evictions":0})
        );
        assert_eq!(
            s.history_inspect(HistoryInspect {
                ids: ids.clone(),
                version: Some(3),
            })
            .unwrap_err(),
            Error::Invalid
        );
        // Unknown request fields are still refused, with or without version.
        for bad in [
            json!({"ids":[], "extra":1}),
            json!({"ids":[], "version":2, "extra":1}),
            json!({"version":2}),
        ] {
            assert!(serde_json::from_value::<HistoryInspect>(bad.clone()).is_err(), "{bad}");
        }
        let parsed: HistoryInspect = serde_json::from_value(json!({"ids":[]})).unwrap();
        assert_eq!(parsed.version, None);
    }
}
