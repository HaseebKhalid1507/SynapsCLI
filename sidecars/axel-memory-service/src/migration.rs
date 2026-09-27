use crate::{
    contract::*,
    database::{hash_parts, hex},
    history::HistoryImport,
    service::{acknowledge_commit, Service},
};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Migration {
    pub migration_id: String,
    pub manifest_digest: String,
    // Optional for existing host-built migration payloads; exported inventories
    // carry all three. They participate in the immutable apply digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_digest: Option<String>,
    #[serde(default)]
    pub records: Vec<MemoryRecord>,
    #[serde(default)]
    pub tombstones: Vec<String>,
    #[serde(default)]
    pub histories: Vec<HistoryImport>,
    #[serde(default)]
    pub captures: Vec<CaptureExport>,
    #[serde(default)]
    pub fingerprints: Vec<Fingerprint>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Fingerprint {
    pub kind: String,
    pub digest: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureExport {
    pub capture_id: String,
    pub note_id: String,
    pub payload_digest: String,
    pub source_digest: String,
    pub evidence: Option<Value>,
    pub withheld: bool,
    pub tombstoned: bool,
}
impl Service {
    pub fn migration_apply(&self, q: Migration) -> Result<Value> {
        match (&q.format, &q.source_project, &q.target_project) {
            (None, None, None) if q.source_digest.is_none() => {}
            (Some(format), Some(source), Some(target)) => {
                if format != EXPORT_FORMAT || source.is_empty() || source.len() > 4096 {
                    return Err(Error::Invalid);
                }
                if !self.members.contains(target) {
                    return Err(Error::Scope);
                }
                if q.source_digest.as_ref().is_some_and(|d| !hex(d, 64)) {
                    return Err(Error::Invalid);
                }
            }
            _ => return Err(Error::Invalid),
        }
        if !hex(&q.migration_id, 64) || !hex(&q.manifest_digest, 64) {
            return Err(Error::Invalid);
        }
        if q.records.len() + q.tombstones.len() > 16384
            || q.histories.len() > 128
            || q.captures.len() > 16384
            || q.fingerprints.len() > 32768
        {
            return Err(Error::TooLarge);
        }
        for record in &q.records {
            self.validate_record(record)?;
        }
        for id in &q.tombstones {
            if !valid_id(id) {
                return Err(Error::Invalid);
            }
        }
        for history in &q.histories {
            history.validate()?;
        }
        let request_digest = hash_parts(&[&serde_json::to_vec(&q)?]);
        let result=self.transaction(|c|{
            self.check_id_owner(c,"synaps_migrations","migration_id",&q.migration_id)?;
            let previous:Option<(String,String,String)>=c.query_row("SELECT manifest_digest,apply_digest,counts FROM synaps_migrations WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND migration_id=?2 AND committed=1",params![self.project,q.migration_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            if let Some((manifest,digest,counts))=previous {
                if manifest!=q.manifest_digest||digest!=request_digest{return Err(Error::Conflict)}
                return Ok(serde_json::from_str(&counts)?)
            }
            for fp in &q.fingerprints {
                if !matches!(fp.kind.as_str(),"note"|"capture"|"history")||!hex(&fp.digest,64){return Err(Error::Invalid)}
                c.execute("INSERT OR IGNORE INTO synaps_fingerprints VALUES(?1,?2,?3)",params![self.project,fp.kind,fp.digest])?;
            }
            self.apply_suppression(c)?;
            // Tombstones dominate live records, even if the source exported both.
            let mut tombstones=0usize;
            for id in &q.tombstones {
                let exists:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM memory_tombstones WHERE id=?1)",[id],|r|r.get(0))?;
                self.delete_record(c,id,true)?;
                tombstones+=usize::from(!exists);
            }
            let mut records=0usize;
            for r in &q.records {records+=usize::from(self.insert_record(c,r,"standard",true)?);}
            let mut histories=0usize;
            // Live first permits importing a content fingerprint alongside a
            // same-ID tombstone without ever exposing the intermediate body.
            for h in q.histories.iter().filter(|h|!h.tombstone){histories+=usize::from(self.insert_history(c,h)?);}
            for h in q.histories.iter().filter(|h|h.tombstone){histories+=usize::from(self.insert_history(c,h)?);}
            for capture in &q.captures {
                self.check_id_owner(c,"synaps_captures","capture_id",&capture.capture_id)?;
                self.check_id_owner(c,"memory_tombstones","id",&capture.note_id)?;
                self.check_id_owner(c,"memories","id",&capture.note_id)?;
                if !hex(&capture.capture_id,64)||!hex(&capture.payload_digest,64)||!hex(&capture.source_digest,64)||capture.note_id!=format!("mem-cap-{}",capture.capture_id){return Err(Error::Invalid)}
                let evidence=match &capture.evidence {
                    Some(v) if !capture.tombstoned=>{
                        if !v["project_id"].as_str().is_some_and(|p|self.members.iter().any(|m|m==p)) || v["capture_id"]!=capture.capture_id{return Err(Error::Scope)}
                        let text=serde_json::to_string(v)?;
                        if text.len()>128*1024{return Err(Error::TooLarge)}
                        if hash_parts(&[text.as_bytes()])!=capture.payload_digest{return Err(Error::Invalid)}
                        crate::capture::validate_import(v,v["project_id"].as_str().ok_or(Error::Scope)?,&capture.source_digest,capture.withheld)?;
                        text
                    },
                    None if capture.tombstoned=>String::new(),
                    _=>return Err(Error::Invalid),
                };
                let prior:Option<String>=c.query_row("SELECT payload_digest FROM synaps_captures WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND capture_id=?2",params![self.project,capture.capture_id],|r|r.get(0)).optional()?;
                if prior.as_ref().is_some_and(|p|p!=&capture.payload_digest){return Err(Error::Conflict)}
                let note_tomb:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM memory_tombstones WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND id=?2)",params![self.project,capture.note_id],|r|r.get(0))?;
                let source_tomb:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM synaps_fingerprints WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND kind='capture' AND digest=?2)",params![self.project,capture.source_digest],|r|r.get(0))?;
                let tomb=capture.tombstoned||note_tomb||source_tomb;
                if !tomb && !c.query_row("SELECT EXISTS(SELECT 1 FROM memories WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND id=?2)",params![self.project,capture.note_id],|r|r.get::<_,bool>(0))?{return Err(Error::Invalid)}
                if prior.is_none(){c.execute("INSERT INTO synaps_captures VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![capture.evidence.as_ref().and_then(|v|v["project_id"].as_str()).unwrap_or(&self.project),capture.capture_id,capture.note_id,capture.payload_digest,capture.source_digest,if tomb{""}else{&evidence},capture.withheld,tomb])?;}
                if tomb {self.delete_record(c,&capture.note_id,true)?;}
                if capture.withheld {
                    c.execute("UPDATE memories SET retention='local_only' WHERE id=?1 AND project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?2))",params![capture.note_id,self.project])?;
                    c.execute("DELETE FROM memories_fts WHERE mem_id=?1",[&capture.note_id])?;
                }
            }
            // Recovered tombstone evidence can arrive after a matching live
            // row in any inventory; erase those earlier bodies before commit.
            self.apply_suppression(c)?;
            let result=json!({"records":records,"tombstones":tombstones,"histories":histories});
            c.execute("INSERT INTO synaps_migrations(project_key,migration_id,manifest_digest,committed,counts,apply_digest) VALUES(?1,?2,?3,1,?4,?5)",params![self.project,q.migration_id,q.manifest_digest,serde_json::to_string(&result)?,request_digest])?;
            Ok(result)
        })?;
        acknowledge_commit(self.durable())?;
        Ok(result)
    }
}
