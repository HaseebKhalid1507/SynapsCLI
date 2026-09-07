//! Immutable, operator-authorized scope membership. Record identities never move.
use crate::{
    contract::*,
    private_path::PrivatePath,
    service::{acknowledge_commit, Service, MARKER},
};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub(crate) const MULTI_MARKER: &str = "synaps-axel/2/multi-project";

pub(crate) fn check_mode(c: &Connection, project: &str) -> Result<bool> {
    let marker = |key| -> Result<Option<String>> {
        c.query_row("SELECT value FROM brain_meta WHERE key=?1", [key], |r| {
            r.get(0)
        })
        .optional()
        .map_err(|_| Error::Unsupported)
    };
    let multi = marker(MULTI_MARKER)?;
    let pin = marker(MARKER)?;
    match (multi.as_deref(), pin.as_deref()) {
        (Some("1"), None) => Ok(true),
        (None, Some(p)) if p == project => Ok(false),
        _ => Err(Error::Unsupported),
    }
}
pub(crate) fn initialize(c: &Connection, project: &str) -> Result<(bool, String, Vec<String>)> {
    let shared = check_mode(c, project)?;
    c.execute_batch("CREATE TABLE IF NOT EXISTS synaps_scope_members(member TEXT PRIMARY KEY,canonical TEXT NOT NULL); CREATE INDEX IF NOT EXISTS synaps_scope_group ON synaps_scope_members(canonical);")?;
    c.execute(
        "INSERT OR IGNORE INTO synaps_scope_members VALUES(?1,?1)",
        [project],
    )?;
    let canonical: String = c.query_row(
        "SELECT canonical FROM synaps_scope_members WHERE member=?1",
        [project],
        |r| r.get(0),
    )?;
    let members = c
        .prepare("SELECT member FROM synaps_scope_members WHERE canonical=?1 ORDER BY member")?
        .query_map([&canonical], |r| r.get(0))?
        .collect::<std::result::Result<Vec<String>, _>>()?;
    if !valid_project(&canonical)
        || !members.iter().all(|p| valid_project(p))
        || !members.contains(&canonical)
        || (!shared && members != [project])
        || (members.contains(&USER_PROJECT.to_owned()) && members.len() != 1)
    {
        return Err(Error::Unsupported);
    }
    Ok((shared, canonical, members))
}
pub(crate) fn same_scope(c: &Connection, a: &str, b: &str) -> Result<bool> {
    Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM synaps_scope_members a JOIN synaps_scope_members b ON a.canonical=b.canonical WHERE a.member=?1 AND b.member=?2)", params![a,b], |r| r.get(0))?)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upgrade {
    pub expected_project: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Alias {
    pub alias_project: String,
    pub alias_root: PathBuf,
    pub canonical_root: PathBuf,
}
impl Service {
    pub(crate) fn validate_record(&self, r: &MemoryRecord) -> Result<()> {
        self.validate_record_metadata(r)?;
        crate::forum::validate_body(r)
    }
    pub(crate) fn validate_record_metadata(&self, r: &MemoryRecord) -> Result<()> {
        if !self.members.contains(&r.project) {
            return Err(Error::Scope);
        }
        r.validate(&r.project)
    }
    pub fn scope_info(&self) -> Value {
        json!({"canonical_project":self.canonical,"members":self.members,"mode":if self.shared {"multi_project"} else {"pinned"},"user_scope":self.project==USER_PROJECT})
    }
    pub fn scope_alias(&mut self, q: Alias) -> Result<Value> {
        if !self.shared || self.canonical != self.project {
            return Err(Error::Scope);
        }
        crate::repository_proof::verify(&q, &self.project)?;
        self.transaction(|c| {
            let owner: Option<String> = c
                .query_row(
                    "SELECT canonical FROM synaps_scope_members WHERE member=?1",
                    [&q.alias_project],
                    |r| r.get(0),
                )
                .optional()?;
            match owner.as_deref() {
                Some(p) if p == self.project => return Ok(()), // verified retry
                Some(p) if p == q.alias_project => {}
                Some(_) => return Err(Error::Conflict),
                None => return Err(Error::NotFound),
            }
            let size: usize = c.query_row(
                "SELECT count(*) FROM synaps_scope_members WHERE canonical=?1",
                [&q.alias_project],
                |r| r.get(0),
            )?;
            if size != 1 {
                return Err(Error::Conflict);
            }
            c.execute(
                "UPDATE synaps_scope_members SET canonical=?1 WHERE member=?2 AND canonical=?2",
                params![self.project, q.alias_project],
            )?;
            // Deletion fingerprints become group-wide atomically, without editing
            // any surviving record, archive projection or capture payload.
            self.apply_suppression(c)?;
            self.history_bounds(c)?;
            Ok(())
        })?;
        (self.shared, self.canonical, self.members) = initialize(self.brain.conn(), &self.project)?;
        acknowledge_commit(self.durable())?;
        Ok(self.scope_info())
    }
}
/// Operator-only path: normal open NEVER calls this. Existing files only.
pub fn upgrade(path: &Path, project: &str, q: Upgrade, user_scope: bool) -> Result<Value> {
    if q.expected_project != project
        || !valid_project(project)
        || (project == USER_PROJECT) != user_scope
    {
        return Err(Error::Scope);
    }
    let private = PrivatePath::acquire(path)?;
    if !private.path.try_exists()? {
        return Err(Error::NotFound);
    }
    let mut c = Connection::open_with_flags(
        &private.path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    let shared = check_mode(&c, project)?;
    if shared {
        // Retry is valid only for the original pin, not any other scope.
        let prior: Option<String> = c
            .query_row(
                "SELECT value FROM brain_meta WHERE key='synaps-axel/2/upgraded-from'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if prior.as_deref() != Some(project) {
            return Err(Error::Conflict);
        }
    } else {
        c.execute_batch("PRAGMA synchronous=FULL; PRAGMA busy_timeout=5000;")?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        initialize(&tx, project)?;
        tx.execute(
            "DELETE FROM brain_meta WHERE key=?1 AND value=?2",
            params![MARKER, project],
        )?;
        tx.execute("INSERT INTO brain_meta VALUES(?1,'1')", [MULTI_MARKER])?;
        tx.execute(
            "INSERT INTO brain_meta VALUES('synaps-axel/2/upgraded-from',?1)",
            [project],
        )?;
        tx.commit()?;
    }
    let (busy, _, _): (i64, i64, i64) = c.query_row("PRAGMA wal_checkpoint(FULL)", [], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    })?;
    if busy != 0 {
        return Err(Error::CommitUnknown);
    }
    acknowledge_commit(private.sync())?;
    drop(c);
    drop(private);
    Ok(Service::open_with_user_scope(path, project, user_scope)?.scope_info())
}
