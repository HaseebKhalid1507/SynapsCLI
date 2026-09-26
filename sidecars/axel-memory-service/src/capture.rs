use crate::{
    contract::*,
    database::{hash_parts, hex},
    history::prefix,
    service::{acknowledge_commit, Service},
};
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureQuery {
    pub capture_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tool {
    name: String,
    summary: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Chat {
    schema: String,
    capture_id: String,
    project_id: String,
    session_id: String,
    turn_id: String,
    turn_ordinal: u64,
    source_digest: String,
    user: String,
    assistant: String,
    tools: Vec<Tool>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TurnRange {
    first: u64,
    last: u64,
    digest: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Summary {
    schema: String,
    capture_id: String,
    project_id: String,
    source_session_id: String,
    source_message_count: usize,
    source_turn_range: TurnRange,
    summary: String,
    summary_provider: Option<String>,
    summary_model: Option<String>,
    local_only: bool,
    prompt_stack_digest: String,
    redaction_policy: String,
    content_classes: Vec<String>,
    summarized_at_unix_ms: u64,
}
struct Prepared {
    id: String,
    source: String,
    session: String,
    body: String,
    withheld: bool,
    timestamp: u64,
}
fn prepare(payload: &Value, project: &str) -> Result<Prepared> {
    if payload.get("project_id").and_then(Value::as_str) != Some(project) {
        return Err(Error::Scope);
    }
    let p = match payload.get("schema").and_then(Value::as_str) {
        Some("chat_turn_capture/1") => {
            let q: Chat = serde_json::from_value(payload.clone())?;
            if q.schema != "chat_turn_capture/1"
                || q.project_id != project
                || q.turn_id.is_empty()
                || q.turn_id.len() > 4096
                || q.tools.len() > 128
            {
                return Err(Error::Invalid);
            }
            let mut body = format!("User: {}\nAssistant: {}", q.user, q.assistant);
            for tool in q.tools {
                body.push_str(&format!("\nTool {}: {}", tool.name, tool.summary));
            }
            let _ordinal = q.turn_ordinal;
            Prepared {
                id: q.capture_id,
                source: q.source_digest,
                session: q.session_id,
                body,
                withheld: false,
                timestamp: chrono::Utc::now().timestamp_millis() as u64,
            }
        }
        Some("conversation_summary/1") => {
            let q: Summary = serde_json::from_value(payload.clone())?;
            if q.schema != "conversation_summary/1"
                || q.project_id != project
                || !hex(&q.prompt_stack_digest, 64)
                || q.source_turn_range.first > q.source_turn_range.last
                || q.source_message_count == 0
                || q.summarized_at_unix_ms > i64::MAX as u64
                || !matches!(
                    q.redaction_policy.as_str(),
                    "truncation_only" | "policy_exclusions"
                )
            {
                return Err(Error::Invalid);
            }
            // Host is responsible for screening actual source text. Defense in
            // depth: reject declared private reasoning / never-persist classes.
            if q.content_classes.iter().any(|s| {
                matches!(
                    s.as_str(),
                    "thinking" | "private_reasoning" | "never_persist"
                )
            }) {
                return Err(Error::Invalid);
            }
            let restricted = q.content_classes.iter().any(|s| {
                matches!(
                    s.as_str(),
                    "secret"
                        | "sensitive"
                        | "local_only"
                        | "visible_after_consent"
                        | "persist_never_transmit"
                )
            });
            if q.content_classes.iter().any(|s| {
                !matches!(
                    s.as_str(),
                    "user_text"
                        | "assistant_text"
                        | "tool_calls"
                        | "tool_results"
                        | "file_paths"
                        | "event_data"
                        | "secret"
                        | "sensitive"
                        | "local_only"
                        | "visible_after_consent"
                        | "persist_never_transmit"
                )
            }) {
                return Err(Error::Invalid);
            }
            if !q.local_only
                && (q.summary_provider.as_deref().is_none_or(str::is_empty)
                    || q.summary_model.as_deref().is_none_or(str::is_empty))
            {
                return Err(Error::Invalid);
            }
            Prepared {
                id: q.capture_id,
                source: q.source_turn_range.digest,
                session: q.source_session_id,
                body: q.summary,
                withheld: q.local_only || restricted,
                timestamp: q.summarized_at_unix_ms,
            }
        }
        _ => return Err(Error::Invalid),
    };
    if !hex(&p.id, 64) || !hex(&p.source, 64) || p.session.is_empty() || p.session.len() > 4096 {
        return Err(Error::Invalid);
    }
    Ok(p)
}
impl Service {
    pub fn capture_query(&self, q: CaptureQuery) -> Result<Value> {
        if !hex(&q.capture_id, 64) {
            return Err(Error::Invalid);
        }
        let state: Option<bool> = self
            .brain
            .conn()
            .query_row(
                "SELECT tombstoned FROM synaps_captures WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND capture_id=?2",
                params![self.project, q.capture_id],
                |r| r.get(0),
            )
            .optional()?;
        // A migrated nonexistent note tombstone also suppresses worker retries.
        let note_tomb: bool = self.brain.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM memory_tombstones WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND id=?2)",
            params![self.project, format!("mem-cap-{}", q.capture_id)],
            |r| r.get(0),
        )?;
        Ok(
            json!({"capture_id":q.capture_id,"committed":state.is_some()||note_tomb,"tombstoned":state.unwrap_or(false)||note_tomb}),
        )
    }
    pub fn capture(&self, payload: Value) -> Result<Value> {
        let evidence = serde_json::to_string(&payload)?;
        if evidence.len() > 128 * 1024 {
            return Err(Error::TooLarge);
        }
        let source_project = payload["project_id"].as_str().ok_or(Error::Scope)?;
        if !self.members.iter().any(|p| p == source_project) {
            return Err(Error::Scope);
        }
        let p = prepare(&payload, source_project)?;
        let digest = hash_parts(&[evidence.as_bytes()]);
        let id = format!("mem-cap-{}", p.id);
        self.transaction(|c|{
            self.check_id_owner(c, "synaps_captures", "capture_id", &p.id)?;
            self.check_id_owner(c, "memory_tombstones", "id", &id)?;
            self.check_id_owner(c, "memories", "id", &id)?;
            let prior:Option<String>=c.query_row("SELECT payload_digest FROM synaps_captures WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND capture_id=?2",params![self.project,p.id],|r|r.get(0)).optional()?;
            if let Some(prior)=prior {return if prior==digest{Ok(())}else{Err(Error::Conflict)}}
            let suppressed:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM memory_tombstones WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND id=?2 UNION ALL SELECT 1 FROM synaps_fingerprints WHERE project_key IN (SELECT member FROM synaps_scope_members WHERE canonical=(SELECT canonical FROM synaps_scope_members WHERE member=?1)) AND kind='capture' AND digest=?3)",params![self.project,id,p.source],|r|r.get(0))?;
            if !suppressed {
                let record=MemoryRecord {
                    namespace:"captures".into(),timestamp_ms:p.timestamp,content:prefix(&p.body,MAX_CONTENT).to_owned(),
                    tags:vec!["capture".into(),payload["schema"].as_str().unwrap_or_default().into()],
                    meta:Some(json!({"capture_id":p.id,"source_digest":p.source,"capture_schema":payload["schema"],"local_only":p.withheld})),
                    id:id.clone(),project:source_project.to_owned(),provenance:Provenance{source:"capture".into(),session:Some(p.session)},
                    sensitivity:if p.withheld{Sensitivity::Secret}else{Sensitivity::Normal},retention:Retention::Standard,
                };
                self.insert_record(c,&record,if p.withheld{"local_only"}else{"standard"},false)?;
            }
            c.execute("INSERT INTO synaps_captures(project_key,capture_id,note_id,payload_digest,source_digest,evidence,withheld,tombstoned) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![source_project,p.id,id,digest,p.source,if suppressed{""}else{&evidence},p.withheld,suppressed])?;
            if suppressed {
                // Capture the newly learned source fingerprint after inserting
                // evidence metadata, then erase earlier equivalent captures.
                self.delete_record(c,&id,true)?;
                self.apply_suppression(c)?;
            }
            Ok(())
        })?;
        acknowledge_commit(self.durable())?;
        Ok(json!({"capture_id":p.id,"committed":true}))
    }
}

pub(crate) fn validate_import(
    v: &Value,
    project: &str,
    source: &str,
    withheld: bool,
) -> Result<()> {
    let p = prepare(v, project)?;
    if p.source != source || (p.withheld && !withheld) {
        return Err(Error::Invalid);
    }
    Ok(())
}
