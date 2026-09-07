use crate::{contract::*, service::Service};
use serde_json::{json, Value};
use std::{
    io::{BufRead, Write},
    path::Path,
};

fn frame(reader: &mut impl BufRead, maximum: usize) -> Result<Option<Request>> {
    let mut bytes = Vec::new();
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(Error::Protocol)
            };
        }
        let count = chunk
            .iter()
            .position(|b| *b == b'\n')
            .map_or(chunk.len(), |p| p + 1);
        if bytes.len() + count > maximum {
            return Err(Error::TooLarge);
        }
        let done = chunk[count - 1] == b'\n';
        bytes.extend_from_slice(&chunk[..count]);
        reader.consume(count);
        if done {
            let request: Request = serde_json::from_slice(&bytes)?;
            let cap = if matches!(
                request.operation.as_str(),
                "history_seal" | "migration_apply"
            ) {
                MAX_LARGE_FRAME
            } else {
                MAX_FRAME
            };
            if bytes.len() > cap {
                return Err(Error::TooLarge);
            }
            return Ok(Some(request));
        }
    }
}
pub fn reply_bytes(project: &str, result: Result<Value>) -> Vec<u8> {
    reply_with_limit(project, result, MAX_FRAME)
}
fn reply_with_limit(project: &str, result: Result<Value>, maximum: usize) -> Vec<u8> {
    let envelope = match result {
        Ok(result) => json!({"schema":SCHEMA,"project":project,"ok":true,"result":result}),
        Err(error) => json!({"schema":SCHEMA,"project":project,"ok":false,"error":error.wire()}),
    };
    let mut bytes = serde_json::to_vec(&envelope).expect("JSON value is serializable");
    if bytes.len() + 1 > maximum {
        return reply_bytes(project, Err(Error::TooLarge));
    }
    bytes.push(b'\n');
    bytes
}
fn respond(writer: &mut impl Write, project: &str, result: Result<Value>) -> Result<()> {
    writer.write_all(&reply_bytes(project, result))?;
    writer.flush()?;
    Ok(())
}
fn check(request: &Request, project: &str) -> Result<()> {
    if request.schema != SCHEMA {
        return Err(Error::Invalid);
    }
    if request.project != project {
        return Err(Error::Scope);
    }
    Ok(())
}
enum Operation {
    Store(MemoryRecord),
    Search(Search),
    Fetch(Fetch),
    Forget(Forget),
    ForumPost(crate::forum_contract::PostRequest),
    ForumRead(crate::forum_contract::Read),
    ForumForget(Forget),
    HistorySeal(crate::history::Seal),
    HistorySearch(crate::history::HistorySearch),
    HistoryFetch(crate::history::HistoryFetch),
    HistoryNote(Forget),
    HistoryForget(Forget),
    Capture(Value),
    CaptureQuery(crate::capture::CaptureQuery),
    Migration(crate::migration::Migration),
    Stats,
    Sweep(crate::retention::Sweep),
    Export(crate::retention::Export),
    Capabilities,
    ScopeInfo,
    ScopeAlias(crate::scope::Alias),
    ScopeUpgrade(crate::scope::Upgrade),
    LegacyExport(crate::legacy::LegacyExport),
}
// Optional fields are absent, not explicit null. Shared pure serde types also
// serve in-process callers, so reject null at this wire boundary.
fn forum_payload(value: &Value) -> Result<()> {
    match value {
        Value::Null => Err(Error::Invalid),
        Value::Object(m) => {
            for v in m.values() {
                forum_payload(v)?;
            }
            Ok(())
        }
        Value::Array(a) => {
            for v in a {
                forum_payload(v)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}
fn parse(request: Request, project: &str, operator: bool) -> Result<Operation> {
    check(&request, project)?;
    // User-wide sharing is explicit short notes only, even for a direct
    // operator process. Reject before payload decoding or opening storage.
    if project == USER_PROJECT
        && !matches!(
            request.operation.as_str(),
            "store" | "search" | "fetch" | "forget" | "scope_info" | "capabilities"
        )
    {
        return Err(Error::Scope);
    }
    match request.operation.as_str() {
        "scope_info" if request.payload == json!({}) => Ok(Operation::ScopeInfo),
        "scope_alias" if operator => Ok(Operation::ScopeAlias(serde_json::from_value(
            request.payload,
        )?)),
        "scope_upgrade" if operator => Ok(Operation::ScopeUpgrade(serde_json::from_value(
            request.payload,
        )?)),
        "store" => {
            let r: MemoryRecord = serde_json::from_value(request.payload)?;
            if crate::forum::reserved(&r) {
                return Err(Error::Invalid);
            }
            r.validate(project)?;
            // Bound the exact success envelope BEFORE any irreversible commit.
            let result = serde_json::to_value(r.clone().disclosed())?;
            let bytes = serde_json::to_vec(
                &json!({"schema":SCHEMA,"project":project,"ok":true,"result":result}),
            )?;
            if bytes.len() + 1 > MAX_FRAME {
                return Err(Error::TooLarge);
            }
            Ok(Operation::Store(r))
        }
        "search" => Ok(Operation::Search(serde_json::from_value(request.payload)?)),
        "fetch" => {
            let q: Fetch = serde_json::from_value(request.payload)?;
            if q.ids.len() > 25 || q.ids.iter().any(|s| !valid_id(s)) {
                return Err(Error::Invalid);
            }
            Ok(Operation::Fetch(q))
        }
        "forget" => {
            let q: Forget = serde_json::from_value(request.payload)?;
            if !valid_id(&q.id) {
                return Err(Error::Invalid);
            }
            Ok(Operation::Forget(q))
        }
        "forum_post" => {
            forum_payload(&request.payload)?;
            let q: crate::forum_contract::PostRequest = serde_json::from_value(request.payload)?;
            q.author.validate().map_err(|_| Error::Invalid)?;
            q.post.validate().map_err(|_| Error::Invalid)?;
            Ok(Operation::ForumPost(q))
        }
        "forum_read" => {
            forum_payload(&request.payload)?;
            let mut payload = request.payload;
            payload
                .as_object_mut()
                .ok_or(Error::Invalid)?
                .entry("limit")
                .or_insert(json!(8));
            let q: crate::forum_contract::Read = serde_json::from_value(payload)?;
            q.validate().map_err(|_| Error::Invalid)?;
            Ok(Operation::ForumRead(q))
        }
        "forum_forget" => {
            let q: Forget = serde_json::from_value(request.payload)?;
            if !crate::forum_contract::valid_id(&q.id) {
                return Err(Error::Invalid);
            }
            Ok(Operation::ForumForget(q))
        }
        "history_seal" => {
            let q: crate::history::Seal = serde_json::from_value(request.payload)?;
            q.validate()?;
            Ok(Operation::HistorySeal(q))
        }
        "history_search" => Ok(Operation::HistorySearch(serde_json::from_value(
            request.payload,
        )?)),
        "history_fetch" => Ok(Operation::HistoryFetch(serde_json::from_value(
            request.payload,
        )?)),
        "history_note" => Ok(Operation::HistoryNote(serde_json::from_value(
            request.payload,
        )?)),
        "history_forget" => Ok(Operation::HistoryForget(serde_json::from_value(
            request.payload,
        )?)),
        "capture" => Ok(Operation::Capture(request.payload)),
        "capture_query" => Ok(Operation::CaptureQuery(serde_json::from_value(
            request.payload,
        )?)),
        "migration_apply" => Ok(Operation::Migration(serde_json::from_value(
            request.payload,
        )?)),
        "stats" if request.payload == json!({}) => Ok(Operation::Stats),
        "sweep" => Ok(Operation::Sweep(serde_json::from_value(request.payload)?)),
        "export" => Ok(Operation::Export(serde_json::from_value(request.payload)?)),
        "capabilities" if request.payload == json!({}) => Ok(Operation::Capabilities),
        "legacy_export" => Ok(Operation::LegacyExport(serde_json::from_value(
            request.payload,
        )?)),
        _ => Err(Error::Protocol),
    }
}
/// Exactly two requests per process. Hello is purely a capability handshake.
pub fn run(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    path: &Path,
    project: &str,
) -> Result<()> {
    run_with_options(reader, writer, path, project, false, false)
}
pub fn run_with_options(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    path: &Path,
    project: &str,
    operator: bool,
    user_scope: bool,
) -> Result<()> {
    let hello = (|| {
        if !valid_project(project) || (project == USER_PROJECT) != user_scope {
            return Err(Error::Scope);
        }
        let r = frame(reader, MAX_FRAME)?.ok_or(Error::Protocol)?;
        check(&r, project)?;
        if r.operation != "hello" || r.payload != json!({}) {
            return Err(Error::Protocol);
        }
        Ok(json!({"backend":"axel","revision":REVISION,"contract":SCHEMA}))
    })();
    let accepted = hello.is_ok();
    respond(writer, project, hello)?;
    if !accepted {
        return Ok(());
    }
    let request = match frame(reader, MAX_LARGE_FRAME) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(()),
        Err(e) => return respond(writer, project, Err(e)),
    };
    let maximum = if matches!(
        request.operation.as_str(),
        "history_fetch" | "export" | "legacy_export"
    ) {
        MAX_LARGE_FRAME
    } else {
        MAX_FRAME
    };
    let result = (|| {
        let op = parse(request, project, operator)?;
        if matches!(op, Operation::Capabilities) {
            if project == USER_PROJECT {
                return Ok(
                    json!({"operations":["store","search","fetch","forget","scope_info"],"operator_operations":[],"scope_model":"immutable_membership/1","user_project":USER_PROJECT,"record_schema":RECORD_SCHEMA,"max_frame_bytes":MAX_FRAME,"max_large_frame_bytes":MAX_LARGE_FRAME,"history_logical_id":"sha256_length_prefixed","history_fetch":"archived_message_array"}),
                );
            }
            return Ok(
                json!({"operations":["store","search","fetch","forget","forum_post","forum_read","forum_forget","history_seal","history_search","history_fetch","history_note","history_forget","capture","capture_query","migration_apply","stats","sweep","export","legacy_export","scope_info"],"operator_operations":["scope_upgrade","scope_alias"],"scope_model":"immutable_membership/1","user_project":USER_PROJECT,"record_schema":RECORD_SCHEMA,"max_frame_bytes":MAX_FRAME,"max_large_frame_bytes":MAX_LARGE_FRAME,"history_logical_id":"sha256_length_prefixed","history_fetch":"archived_message_array","forum":{"schema":1}}),
            );
        }
        if let Operation::LegacyExport(q) = op {
            return crate::legacy::export(q, project);
        }
        if let Operation::ScopeUpgrade(q) = op {
            return crate::scope::upgrade(path, project, q, user_scope);
        }
        let mut service = Service::open_with_user_scope(path, project, user_scope)?;
        match op {
            Operation::ScopeInfo => Ok(service.scope_info()),
            Operation::ScopeAlias(q) => service.scope_alias(q),
            Operation::ScopeUpgrade(_) => unreachable!("handled before normal open"),
            Operation::Store(r) => Ok(serde_json::to_value(service.store(r)?)?),
            Operation::Search(q) => Ok(serde_json::to_value(service.search(q)?)?),
            Operation::Fetch(q) => Ok(serde_json::to_value(service.fetch(&q.ids)?)?),
            Operation::Forget(q) => Ok(json!(service.forget(&q.id)?)),
            Operation::ForumPost(q) => Ok(serde_json::to_value(service.forum_post(q)?)?),
            Operation::ForumRead(q) => Ok(serde_json::to_value(service.forum_read(q)?)?),
            Operation::ForumForget(q) => Ok(json!(service.forum_forget(&q.id)?)),
            Operation::HistorySeal(q) => service.history_seal(q),
            Operation::HistorySearch(q) => service.history_search(q),
            Operation::HistoryFetch(q) => Ok(serde_json::to_value(service.history_fetch(q)?)?),
            Operation::HistoryNote(q) => Ok(json!(service.history_note(&q.id)?)),
            Operation::HistoryForget(q) => Ok(json!(service.history_forget(&q.id)?)),
            Operation::Capture(q) => service.capture(q),
            Operation::CaptureQuery(q) => service.capture_query(q),
            Operation::Migration(q) => service.migration_apply(q),
            Operation::Stats => service.stats(),
            Operation::Sweep(q) => service.sweep(q),
            Operation::Export(q) => service.export(q),
            Operation::Capabilities | Operation::LegacyExport(_) => {
                unreachable!("handled before opening target")
            }
        }
    })();
    writer.write_all(&reply_with_limit(project, result, maximum))?;
    writer.flush()?;
    Ok(())
}
