//! Durable, disclosure-filtered context segments; never a summary or a session log.
//!
//! The trusted host supplies an absolute base directory, scope key and logical
//! conversation id (never model/tool path arguments). Scope and logical id are
//! hashed; project scope determines the directory name. Only user/assistant text
//! and tool-call structure
//! are retained. System/developer messages, reasoning, unknown blocks, binary,
//! non-normal sensitivity and restricted/unknown disclosure are omitted.
//! Arguments/results are WITHHELD unless a trusted redactor is installed. All
//! retained strings pass the disclosure gate and a conservative credential
//! screen. This screen is not a universal secret detector: host classification
//! and the optional redactor remain the authority for unlabelled secrets.
//!
//! Each segment contains its hidden, screened note and source indices in ONE
//! atomic file. Notes are not searched or returned by message fetch. Eligible
//! text is otherwise exact: no truncation, whitespace rewriting or summarizing.
//! `fetch_with_provenance` reports indices into the original `seal` slice.
//!
//! Fixed slots avoid unbounded directory enumeration/indexes: at most 128
//! segments (including permanent tombstones), 256 MiB total, 16 MiB per file.
//! Input is limited to 4096 messages, 16 MiB of eligible strings/structure,
//! 65536 visited nodes and depth 32; excluded payload descendants are opaque.
//! Notes are limited to 8 KiB. Fetch returns <=128 messages and <=16 MiB;
//! search queries <=512 bytes, results <=32 with <=256-byte snippets. Every
//! operation scans at most 128 files / 256 MiB; lock acquisition is nonblocking.
//! Budget exhaustion and corruption fail closed, never silently truncate seal.
//!
//! Unix-only private, handle-relative no-symlink filesystem; other platforms
//! fail Unsupported. Live records are immutable; forget atomically replaces a
//! record with a content-free id/digest/scope tombstone. The digest covers ONLY
//! the eligible projection and source indices, not the note or forbidden data.
//! Identical projections in one logical conversation deduplicate permanently;
//! a changed note does not create a new segment. Retrying a tombstoned projection
//! returns PermissionDenied. A successful seal fsyncs data and directory before
//! returning; callers MUST keep source history on any error (even if publication
//! happened before a directory-fsync error). Advisory locks serialize cooperating
//! processes, not malicious processes running as the same OS user.

use crate::core::disclosure::{gate_for_model, may_persist, DisclosureClass, ModelVisibility};
use crate::core::stream_types::SharedMessage;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;

pub const MAX_SEGMENTS: usize = 128;
pub const MAX_SEGMENT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_STORE_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_MESSAGES: usize = 4096;
pub const MAX_NOTE_BYTES: usize = 8192;
pub const MAX_FETCH_MESSAGES: usize = 128;
pub const MAX_SEARCH_RESULTS: usize = 32;
const WITHHELD: &str = "[archive: withheld]";

/// Host-owned sanitizer, never supplied by model input. Must not expand output
/// beyond the archive budgets. Tool arguments are passed as serialized JSON;
/// if the returned text is not JSON the entire argument payload is withheld.
pub type ArchiveRedactor = dyn Fn(&str) -> String + Send + Sync;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveRef {
    pub id: String,
    /// Eligible messages, not the length of the source slice.
    pub message_count: usize,
    pub source_message_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveDescriptor {
    pub id: String,
    pub message_count: usize,
    pub source_message_count: usize,
    /// A bounded excerpt of eligible source JSON, never the hidden note.
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchivedMessage {
    /// Zero-based offset in the slice passed to seal, including omitted messages.
    pub source_index: usize,
    /// Original content-array indices; empty for string content.
    pub block_indices: Vec<usize>,
    pub message: SharedMessage,
}

/// Eligible, screened source evidence without filesystem state. `logical_id`
/// is the legacy 64-hex hash of the raw host session ID; the digest deliberately
/// excludes the hidden note and omitted content. Never log this payload.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveProjection {
    pub logical_id: String,
    pub source_message_count: usize,
    pub messages: Vec<ArchivedMessage>,
    pub note: String,
    pub digest: String,
}

/// Pure projection: no path discovery, directory creation, or archive I/O.
/// The trusted host supplies the raw logical session ID and optional sanitizer.
pub fn project_archive(
    logical_id: &str,
    messages: &[SharedMessage],
    note: &str,
    redactor: Option<Arc<ArchiveRedactor>>,
) -> io::Result<ArchiveProjection> {
    validate_key(logical_id)?;
    project_hashed(
        &digest_parts(&[logical_id.as_bytes()]),
        messages,
        note,
        redactor,
    )
}

fn project_hashed(
    logical_id: &str,
    messages: &[SharedMessage],
    note: &str,
    redactor: Option<Arc<ArchiveRedactor>>,
) -> io::Result<ArchiveProjection> {
    if messages.len() > MAX_MESSAGES || note.len() > MAX_NOTE_BYTES {
        return Err(invalid("archive message/note budget exceeded"));
    }
    let mut budget = InputBudget { bytes: 0, nodes: 0 };
    for message in messages {
        budget.message(message, redactor.is_some())?;
    }
    let projector = Projector { redactor };
    let mut eligible = Vec::new();
    for (source_index, message) in messages.iter().enumerate() {
        if let Some((message, block_indices)) = projector.project(message)? {
            eligible.push(ArchivedMessage {
                source_index,
                block_indices,
                message: Arc::new(message),
            });
        }
    }
    let digest = projection_digest(logical_id, messages.len(), &eligible)?;
    let note = projector.screen(note, DisclosureClass::ModelVisible)?;
    if note.len() > MAX_NOTE_BYTES {
        return Err(invalid("redacted note exceeds archive budget"));
    }
    Ok(ArchiveProjection {
        logical_id: logical_id.to_owned(),
        source_message_count: messages.len(),
        messages: eligible,
        note,
        digest,
    })
}

struct Projector {
    redactor: Option<Arc<ArchiveRedactor>>,
}

/// No Debug implementation: do not accidentally log redactor state or content.
pub struct ArchiveStore {
    fs: PrivateDir,
    scope: String,
    logical: String,
    redactor: Option<Arc<ArchiveRedactor>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    scope: String,
    logical: String,
    id: String,
    digest: String,
    body: Body,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum Body {
    Live {
        source_message_count: usize,
        messages: Vec<ArchivedMessage>,
        note: String,
    },
    Tombstone,
}

impl Record {
    fn reference(&self) -> io::Result<ArchiveRef> {
        match &self.body {
            Body::Live {
                source_message_count,
                messages,
                ..
            } => Ok(ArchiveRef {
                id: self.id.clone(),
                message_count: messages.len(),
                source_message_count: *source_message_count,
            }),
            Body::Tombstone => Err(not_found()),
        }
    }
}

impl ArchiveStore {
    pub fn new(base: &Path, scope_key: &str, logical_id: &str) -> io::Result<Self> {
        for key in [scope_key, logical_id] {
            validate_key(key)?;
        }
        let scope = digest_parts(&[scope_key.as_bytes()]);
        let logical = digest_parts(&[logical_id.as_bytes()]);
        let fs = PrivateDir::new(base, &scope)?;
        Ok(Self {
            fs,
            scope,
            logical,
            redactor: None,
        })
    }

    /// Opt in to screened tool evidence. Install the SAME trusted policy when
    /// reopening a store; retrieval never restores material withheld at seal.
    pub fn with_redactor(mut self, redactor: Arc<ArchiveRedactor>) -> Self {
        self.redactor = Some(redactor);
        self
    }

    pub fn seal(&self, messages: &[SharedMessage], note: &str) -> io::Result<ArchiveRef> {
        let projection = project_hashed(&self.logical, messages, note, self.redactor.clone())?;
        let ArchiveProjection {
            digest,
            source_message_count,
            messages: eligible,
            note,
            ..
        } = projection;
        let _lock = self.fs.lock()?;
        let (records, used) = self.scan()?;
        if let Some((_, record)) = records.iter().find(|(_, r)| r.digest == digest) {
            if matches!(record.body, Body::Tombstone) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "archive projection was forgotten",
                ));
            }
            // A previous call may have failed AFTER rename but BEFORE fsync.
            self.fs.sync()?;
            return record.reference();
        }
        let slot = (0..MAX_SEGMENTS)
            .find(|slot| !records.iter().any(|(s, _)| s == slot))
            .ok_or_else(|| invalid("archive segment budget exhausted (tombstones retained)"))?;
        let record = Record {
            version: 1,
            scope: self.scope.clone(),
            logical: self.logical.clone(),
            id: uuid::Uuid::new_v4().simple().to_string(),
            digest,
            body: Body::Live {
                source_message_count,
                messages: eligible,
                note,
            },
        };
        let bytes = bounded_json(&record, MAX_SEGMENT_BYTES)?;
        if used + bytes.len() > MAX_STORE_BYTES {
            return Err(invalid("archive storage byte budget exhausted"));
        }
        self.fs.publish(slot, &bytes)?;
        record.reference()
    }

    pub fn fetch(&self, id: &str, start: usize, limit: usize) -> io::Result<Vec<SharedMessage>> {
        Ok(self
            .fetch_with_provenance(id, start, limit)?
            .into_iter()
            .map(|m| m.message)
            .collect())
    }

    /// `start` indexes ELIGIBLE messages; source_index retains original offsets.
    /// Oversized limits are rejected rather than clamped; start beyond end is empty.
    pub fn fetch_with_provenance(
        &self,
        id: &str,
        start: usize,
        limit: usize,
    ) -> io::Result<Vec<ArchivedMessage>> {
        validate_id(id)?;
        if limit > MAX_FETCH_MESSAGES {
            return Err(invalid("archive fetch limit exceeds 128"));
        }
        let _lock = self.fs.lock()?;
        let (records, _) = self.scan()?;
        let record = find_record(records, id)?;
        match record.body {
            Body::Live { messages, .. } => {
                Ok(messages.into_iter().skip(start).take(limit).collect())
            }
            Body::Tombstone => Err(not_found()),
        }
    }

    /// Explicit host-only access to the hidden, screened note. Never include it
    /// in search descriptors or inject it as system/developer instructions.
    pub fn fetch_note(&self, id: &str) -> io::Result<String> {
        validate_id(id)?;
        let _lock = self.fs.lock()?;
        let (records, _) = self.scan()?;
        match find_record(records, id)?.body {
            Body::Live { note, .. } => Ok(note),
            Body::Tombstone => Err(not_found()),
        }
    }

    /// Case-insensitive literal substring search of eligible source JSON, in
    /// publication-slot order. Empty query lists descriptors. Not an exhaustive
    /// ranking service: output is bounded by limit, with no hidden-note matches.
    pub fn search(&self, query: &str, limit: usize) -> io::Result<Vec<ArchiveDescriptor>> {
        if query.len() > 512 || limit > MAX_SEARCH_RESULTS {
            return Err(invalid("archive search query/result budget exceeded"));
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let query = query.to_lowercase();
        let _lock = self.fs.lock()?;
        let (records, _) = self.scan()?;
        let mut result = Vec::new();
        for (_, record) in records {
            if let Body::Live {
                messages,
                source_message_count,
                ..
            } = &record.body
            {
                let mut snippet = None;
                for message in messages {
                    let text = serde_json::to_string(&message.message).map_err(json_error)?;
                    if query.is_empty() || text.to_lowercase().contains(&query) {
                        snippet = Some(prefix(&text, 256).to_owned());
                        break;
                    }
                }
                if snippet.is_some() || (query.is_empty() && messages.is_empty()) {
                    result.push(ArchiveDescriptor {
                        id: record.id,
                        message_count: messages.len(),
                        source_message_count: *source_message_count,
                        snippet: snippet.unwrap_or_default(),
                    });
                }
            }
            if result.len() == limit {
                break;
            }
        }
        Ok(result)
    }

    /// Durable, idempotent deletion. Replaces content in the SAME slot with a
    /// marker; it never frees the slot/digest for reuse. Unknown ids are NotFound.
    /// This is logical deletion, not secure erasure of filesystem snapshots.
    pub fn forget(&self, id: &str) -> io::Result<()> {
        validate_id(id)?;
        let _lock = self.fs.lock()?;
        let (records, _) = self.scan()?;
        let (slot, mut record) = records
            .into_iter()
            .find(|(_, r)| r.id == id)
            .ok_or_else(not_found)?;
        if matches!(record.body, Body::Tombstone) {
            return self.fs.sync();
        }
        record.body = Body::Tombstone;
        self.fs
            .publish(slot, &bounded_json(&record, MAX_SEGMENT_BYTES)?)
    }

    /// Migration-only export of validated eligible evidence and permanent
    /// tombstones. Includes hidden notes: this is HOST data, never a model tool.
    /// No files, directories, permissions or archive contents are changed.
    pub fn export_records(&self) -> io::Result<Vec<Value>> {
        let lock = self.fs.lock_existing()?;
        let (records, _) = self.scan_with(true)?;
        if lock.is_none() && !records.is_empty() {
            return Err(corrupt());
        }
        Ok(records
            .into_iter()
            .map(|(_, record)| {
                let (source_message_count, messages, note, tombstone) = match record.body {
                    Body::Live {
                        source_message_count,
                        messages,
                        note,
                    } => (source_message_count, messages, note, false),
                    Body::Tombstone => (0, Vec::new(), String::new(), true),
                };
                json!({
                    "id": record.id, "digest": record.digest, "logical_id": record.logical,
                    "source_message_count": source_message_count, "messages": messages,
                    "note": note, "tombstone": tombstone,
                })
            })
            .collect())
    }

    fn scan(&self) -> io::Result<(Vec<(usize, Record)>, usize)> {
        self.scan_with(false)
    }

    fn scan_with(&self, readonly: bool) -> io::Result<(Vec<(usize, Record)>, usize)> {
        let mut records = Vec::new();
        let mut used = 0;
        for slot in 0..MAX_SEGMENTS {
            let Some(bytes) = self.fs.read_with(slot, MAX_STORE_BYTES - used, readonly)? else {
                continue;
            };
            used += bytes.len();
            let record: Record = serde_json::from_slice(&bytes).map_err(|_| corrupt())?;
            if record.version != 1
                || record.scope != self.scope
                || record.logical.len() != 64
                || !record.logical.bytes().all(|b| b.is_ascii_hexdigit())
                || validate_id(&record.id).is_err()
                || record.digest.len() != 64
                || !record.digest.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err(corrupt());
            }
            if let Body::Live {
                messages,
                source_message_count,
                note,
            } = &record.body
            {
                if messages.len() > MAX_MESSAGES
                    || *source_message_count > MAX_MESSAGES
                    || note.len() > MAX_NOTE_BYTES
                    || messages
                        .iter()
                        .any(|m| m.source_index >= *source_message_count)
                    || messages
                        .windows(2)
                        .any(|m| m[0].source_index >= m[1].source_index)
                    || projection_digest(&record.logical, *source_message_count, messages)?
                        != record.digest
                {
                    return Err(corrupt());
                }
            }
            if records
                .iter()
                .any(|(_, r): &(usize, Record)| r.id == record.id || r.digest == record.digest)
            {
                return Err(corrupt());
            }
            records.push((slot, record));
        }
        Ok((records, used))
    }
}

impl Projector {
    fn screen(&self, text: &str, class: DisclosureClass) -> io::Result<String> {
        if class == DisclosureClass::ModelVisibleAfterRedaction && self.redactor.is_none() {
            return Ok(WITHHELD.to_owned());
        }
        let redactor = |text: &str| {
            let text = self
                .redactor
                .as_ref()
                .map_or_else(|| text.to_owned(), |r| r(text));
            if credential_like(&text) {
                WITHHELD.to_owned()
            } else {
                text
            }
        };
        // All text is screened, including the ordinary model-visible class.
        let text = redactor(text);
        if text.len() > MAX_SEGMENT_BYTES {
            return Err(invalid("redactor output exceeds archive budget"));
        }
        match gate_for_model(class, &text, false, Some(&|s| s.to_owned())) {
            ModelVisibility::Visible(text) => Ok(text),
            ModelVisibility::Withheld(_) => Ok(WITHHELD.to_owned()),
        }
    }

    fn project(&self, message: &Value) -> io::Result<Option<(Value, Vec<usize>)>> {
        let Some((class, role, content)) = projection_content(message) else {
            return Ok(None);
        };
        if let Some(text) = content.as_str() {
            return Ok(Some((
                json!({"role": role, "content": self.screen(text, class)?}),
                Vec::new(),
            )));
        }
        let Some(blocks) = content.as_array() else {
            return Ok(None);
        };
        let mut output = Vec::new();
        let mut indices = Vec::new();
        for (index, block) in blocks.iter().enumerate() {
            let Some(mut block_class) = eligible_class(block) else {
                continue;
            };
            if class == DisclosureClass::ModelVisibleAfterRedaction {
                block_class = class;
            }
            let kept = match block.get("type").and_then(Value::as_str) {
                Some("text") => block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|text| {
                        self.screen(text, block_class)
                            .map(|text| json!({"type":"text", "text":text}))
                    })
                    .transpose()?,
                Some("tool_use") if role == "assistant" => {
                    if let (Some(id), Some(name)) =
                        (identifier(block, "id"), identifier(block, "name"))
                    {
                        let input = if name == "context_checkpoint" {
                            // The working note has its own hidden field. Do not
                            // expose it a second time via tool arguments.
                            json!({"_archive_withheld":true})
                        } else if self.redactor.is_some() {
                            self.tool_input(block.get("input"), block_class)?
                        } else {
                            json!({"_archive_withheld":true})
                        };
                        Some(json!({"type":"tool_use", "id":id, "name":name, "input":input}))
                    } else {
                        None
                    }
                }
                Some("tool_result") if role == "user" => {
                    if let Some(id) = identifier(block, "tool_use_id") {
                        let content = if self.redactor.is_some() {
                            self.tool_result(&block["content"], block_class)?
                        } else {
                            Value::String(WITHHELD.to_owned())
                        };
                        let mut result =
                            json!({"type":"tool_result", "tool_use_id":id, "content":content});
                        if let Some(error) = block.get("is_error").and_then(Value::as_bool) {
                            result["is_error"] = json!(error);
                        }
                        Some(result)
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some(kept) = kept {
                output.push(kept);
                indices.push(index);
            }
        }
        if output.is_empty() {
            Ok(None)
        } else {
            Ok(Some((json!({"role":role,"content":output}), indices)))
        }
    }

    fn tool_input(&self, input: Option<&Value>, class: DisclosureClass) -> io::Result<Value> {
        if let Some(input) = input {
            // The full preflight already enforced aggregate limits. Reuse its
            // guarded admission predicate so recursion and withholding cannot
            // drift. JSON escaping expands at most sixfold (node allowance
            // covers punctuation/numbers); redactors may shrink that scratch
            // string below the unchanged final output limit.
            let mut budget = InputBudget { bytes: 0, nodes: 0 };
            if let Some(bytes) = budget.input(input, 3)? {
                budget.charge(bytes)?;
                let limit = bytes
                    .checked_mul(6)
                    .ok_or_else(|| invalid("archive input byte budget exceeded"))?;
                let encoded = bounded_json(input, limit)?;
                let text =
                    self.screen(std::str::from_utf8(&encoded).map_err(|_| corrupt())?, class)?;
                return Ok(serde_json::from_str(&text)
                    .unwrap_or_else(|_| json!({"_archive_withheld":true})));
            }
        }
        Ok(json!({"_archive_withheld":true}))
    }

    fn tool_result(&self, content: &Value, class: DisclosureClass) -> io::Result<Value> {
        if let Some(text) = content.as_str() {
            return Ok(json!(self.screen(text, class)?));
        }
        let mut output = Vec::new();
        if let Some(blocks) = content.as_array() {
            for block in blocks {
                if let Some(class) = eligible_class(block) {
                    if block["type"] == "text" {
                        if let Some(text) = block["text"].as_str() {
                            output.push(json!({"type":"text", "text":self.screen(text, class)?}));
                        }
                    }
                }
            }
        }
        if output.is_empty() {
            Ok(json!(WITHHELD))
        } else {
            Ok(json!(output))
        }
    }
}

/// Read-only migration preview for an existing project archive. A missing
/// archive is empty; corrupt, unsafe or locked paths fail closed. In particular,
/// this never invokes `ArchiveStore::new`, mkdir, chmod, or lock-file creation.
pub fn export_in(base: &Path, scope_key: &str) -> io::Result<Vec<Value>> {
    validate_key(scope_key)?;
    let scope = digest_parts(&[scope_key.as_bytes()]);
    let fs = match PrivateDir::open_existing(base, &scope) {
        Ok(fs) => fs,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    ArchiveStore {
        fs,
        scope,
        logical: String::new(), // export scans all logical conversations in this scope
        redactor: None,
    }
    .export_records()
}

/// Project-wide archive lookup; base and scope are trusted host values, never
/// model-supplied paths. The same budgets and disclosure policy apply.
pub fn project_search(
    base: &Path,
    scope_key: &str,
    query: &str,
    limit: usize,
) -> io::Result<Vec<ArchiveDescriptor>> {
    ArchiveStore::new(base, scope_key, "project-reader")?.search(query, limit)
}

/// Fetch an opaque reference from any logical conversation in this project.
pub fn project_fetch(
    base: &Path,
    scope_key: &str,
    id: &str,
    start: usize,
    limit: usize,
) -> io::Result<Vec<SharedMessage>> {
    ArchiveStore::new(base, scope_key, "project-reader")?.fetch(id, start, limit)
}

fn eligible_class(value: &Value) -> Option<DisclosureClass> {
    // Provider/UI envelopes sometimes label private reasoning as assistant text.
    // Do not rely on the role or content-block type alone.
    if value
        .get("channel")
        .is_some_and(|v| !matches!(v.as_str(), Some("final")))
        || value.get("content_class").is_some_and(|v| {
            !matches!(
                v.as_str(),
                Some("user_text" | "assistant_text" | "tool_calls" | "tool_results")
            )
        })
        || value
            .get("private")
            .is_some_and(|v| v != &Value::Bool(false))
    {
        return None;
    }
    for key in ["sensitivity", "retention", "retention_class"] {
        if let Some(v) = value.get(key) {
            if !matches!(v.as_str(), Some("normal" | "standard" | "model_visible")) {
                return None;
            }
        }
    }
    let mut class = DisclosureClass::ModelVisible;
    for key in ["disclosure", "disclosure_class"] {
        if let Some(v) = value.get(key) {
            let parsed = DisclosureClass::parse(v.as_str()?)?;
            if !may_persist(parsed)
                || !matches!(
                    parsed,
                    DisclosureClass::ModelVisible | DisclosureClass::ModelVisibleAfterRedaction
                )
            {
                return None;
            }
            class = parsed;
        }
    }
    Some(class)
}

/// Shared message-envelope selection for preflight and projection. Host
/// continuation notes are derived working context, never source evidence.
fn projection_content(message: &Value) -> Option<(DisclosureClass, &str, &Value)> {
    if message.get("_synaps_context").is_some() {
        return None;
    }
    let class = eligible_class(message)?;
    let role @ ("user" | "assistant") = message.get("role")?.as_str()? else {
        return None;
    };
    Some((class, role, message.get("content")?))
}

fn identifier<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    let text = value.get(key)?.as_str()?;
    (!text.is_empty()
        && text.len() <= 128
        && !credential_like(text)
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b)))
    .then_some(text)
}

/// Conservatively withhold an entire field rather than risk partial redaction.
fn credential_like(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "password",
        "passwd",
        "api_key",
        "apikey",
        "api-key",
        "access_token",
        "refresh_token",
        "secret",
        "authorization",
        "bearer ",
        "private key",
        "sk-",
        "ghp_",
        "github_pat_",
        "xoxb-",
        "akia",
        "eyjh",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

struct InputBudget {
    bytes: usize,
    nodes: usize,
}
impl InputBudget {
    fn node(&mut self, depth: usize) -> io::Result<()> {
        self.nodes += 1;
        if depth > 32 || self.nodes > 65536 {
            return Err(invalid("archive input structural budget exceeded"));
        }
        Ok(())
    }

    fn charge(&mut self, bytes: usize) -> io::Result<()> {
        self.bytes = self.bytes.saturating_add(bytes);
        if self.bytes > MAX_INPUT_BYTES {
            return Err(invalid("archive input byte budget exceeded"));
        }
        Ok(())
    }

    // Only selected fields are charged; ignored metadata is opaque. Include the
    // old per-node/key allowance, never more than the corresponding raw subtree.
    fn field(&mut self, key: &str, text: &str, depth: usize) -> io::Result<()> {
        self.node(depth)?;
        self.charge(key.len().saturating_add(8).saturating_add(text.len()))
    }

    fn text_field(
        &mut self,
        key: &str,
        text: &str,
        class: DisclosureClass,
        redactor: bool,
        depth: usize,
    ) -> io::Result<()> {
        if class == DisclosureClass::ModelVisibleAfterRedaction && !redactor {
            return self.node(depth); // screen() returns the fixed placeholder
        }
        self.field(key, text, depth)
    }

    /// Allocation-free semantic preflight. Complete for ALL messages before
    /// invoking a redactor or materializing the projection. Keep the selected
    /// branches in sync with Projector; skipped payload descendants are opaque.
    fn message(&mut self, message: &Value, redactor: bool) -> io::Result<()> {
        self.node(0)?;
        let Some((class, role, content)) = projection_content(message) else {
            return Ok(());
        };
        self.charge(8)?;
        self.field("role", role, 1)?;
        if let Some(text) = content.as_str() {
            return self.text_field("content", text, class, redactor, 1);
        }
        self.node(1)?;
        let Some(blocks) = content.as_array() else {
            return Ok(());
        };
        self.charge("content".len() + 8)?;
        for block in blocks {
            self.node(2)?;
            let Some(mut block_class) = eligible_class(block) else {
                continue;
            };
            if class == DisclosureClass::ModelVisibleAfterRedaction {
                block_class = class;
            }
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        self.charge(8)?;
                        self.field("type", "text", 3)?;
                        self.text_field("text", text, block_class, redactor, 3)?;
                    }
                }
                Some("tool_use") if role == "assistant" => {
                    if let (Some(id), Some(name)) =
                        (identifier(block, "id"), identifier(block, "name"))
                    {
                        self.charge(8)?;
                        self.field("type", "tool_use", 3)?;
                        self.field("id", id, 3)?;
                        self.field("name", name, 3)?;
                        if name != "context_checkpoint" && redactor {
                            if let Some(input) = block.get("input") {
                                if let Some(bytes) = self.input(input, 3)? {
                                    self.charge("input".len().saturating_add(bytes))?;
                                }
                            }
                        }
                    }
                }
                Some("tool_result") if role == "user" => {
                    if let Some(id) = identifier(block, "tool_use_id") {
                        self.charge(8)?;
                        self.field("type", "tool_result", 3)?;
                        self.field("tool_use_id", id, 3)?;
                        if block.get("is_error").and_then(Value::as_bool).is_some() {
                            self.field("is_error", "", 3)?;
                        }
                        if redactor {
                            if let Some(content) = block.get("content") {
                                self.tool_result(content, block_class)?;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn tool_result(&mut self, content: &Value, class: DisclosureClass) -> io::Result<()> {
        if let Some(text) = content.as_str() {
            return self.text_field("content", text, class, true, 3);
        }
        self.node(3)?;
        if let Some(blocks) = content.as_array() {
            self.charge("content".len() + 8)?;
            for block in blocks {
                self.node(4)?;
                if let Some(class) = eligible_class(block) {
                    if block["type"] == "text" {
                        if let Some(text) = block["text"].as_str() {
                            self.charge(8)?;
                            self.field("type", "text", 5)?;
                            self.text_field("text", text, class, true, 5)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Guarded whole-argument admission, shared with Projector. Byte accounting
    /// is provisional until every visited child is eligible: a later excluded
    /// child withholds the WHOLE input, regardless of tentative byte cost.
    /// Structural visits are never rolled back. Arbitrary tool JSON is not a
    /// content-block array; unclassified data/base64 fields remain eligible.
    fn input(&mut self, value: &Value, depth: usize) -> io::Result<Option<usize>> {
        self.node(depth)?;
        if eligible_class(value).is_none() {
            return Ok(None);
        }
        let mut bytes: usize = 8;
        match value {
            Value::String(text) => bytes = bytes.saturating_add(text.len()),
            Value::Array(values) => {
                for value in values {
                    let Some(cost) = self.input(value, depth + 1)? else {
                        return Ok(None);
                    };
                    bytes = bytes.saturating_add(cost);
                }
            }
            Value::Object(values) => {
                for (key, value) in values {
                    let Some(cost) = self.input(value, depth + 1)? else {
                        return Ok(None);
                    };
                    bytes = bytes.saturating_add(key.len()).saturating_add(cost);
                }
            }
            _ => {}
        }
        Ok(Some(bytes))
    }
}

fn projection_digest(
    scope: &str,
    count: usize,
    messages: &[ArchivedMessage],
) -> io::Result<String> {
    let bytes = bounded_json(messages, MAX_SEGMENT_BYTES)?;
    Ok(digest_parts(&[
        scope.as_bytes(),
        &(count as u64).to_be_bytes(),
        &bytes,
    ]))
}
fn digest_parts(parts: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    format!("{:x}", hash.finalize())
}
fn bounded_json<T: Serialize + ?Sized>(value: &T, limit: usize) -> io::Result<Vec<u8>> {
    struct Sink {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit - self.bytes.len() {
                return Err(invalid("archive serialized byte budget exceeded"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut sink = Sink {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut sink, value).map_err(json_error)?;
    Ok(sink.bytes)
}
fn find_record(records: Vec<(usize, Record)>, id: &str) -> io::Result<Record> {
    records
        .into_iter()
        .find(|(_, r)| r.id == id)
        .map(|(_, r)| r)
        .ok_or_else(not_found)
}
fn prefix(text: &str, bytes: usize) -> &str {
    let mut end = bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
fn validate_key(key: &str) -> io::Result<()> {
    if key.is_empty() || key.len() > 4096 {
        return Err(invalid("archive scope/logical id must be 1..4096 bytes"));
    }
    Ok(())
}

fn validate_id(id: &str) -> io::Result<()> {
    if id.len() != 32
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Err(invalid("invalid opaque archive id"))
    } else {
        Ok(())
    }
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn corrupt() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid archive record")
}
fn not_found() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        "archive not available in this scope",
    )
}
fn json_error(_: serde_json::Error) -> io::Error {
    invalid("archive JSON serialization failed or exceeded budget")
}

// ConfinedDir has no public directory fsync or bounded enumeration API. Keep a
// private directory fd using the same openat/O_NOFOLLOW policy and modes as
// private_fs, and use fixed numbered slots instead of an unbounded index scan.
#[cfg(unix)]
struct PrivateDir(std::fs::File);
#[cfg(unix)]
impl PrivateDir {
    fn new(base: &Path, scope: &str) -> io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        if !base.is_absolute() || base == Path::new("/") {
            return Err(invalid("archive base must be an absolute non-root path"));
        }
        let mut dir = Self(
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open("/")?,
        );
        let components: Vec<_> = base.components().collect();
        for (index, component) in components.iter().enumerate() {
            match component {
                std::path::Component::RootDir => {}
                std::path::Component::Normal(name) => {
                    let name = name
                        .to_str()
                        .ok_or_else(|| invalid("archive path must be UTF-8"))?;
                    dir = dir.child(name, index == components.len() - 1)?;
                }
                _ => return Err(invalid("archive base contains traversal")),
            }
        }
        dir = dir.child("context-archives", true)?;
        dir.child(scope, true)
    }
    fn open_existing(base: &Path, scope: &str) -> io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        if !base.is_absolute() || base == Path::new("/") {
            return Err(invalid("archive base must be an absolute non-root path"));
        }
        let mut dir = Self(
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open("/")?,
        );
        for component in base.components() {
            match component {
                std::path::Component::RootDir => {}
                std::path::Component::Normal(name) => {
                    let name = c_name(
                        name.to_str()
                            .ok_or_else(|| invalid("archive path must be UTF-8"))?,
                    )?;
                    dir = Self(dir.open(&name, libc::O_RDONLY | libc::O_DIRECTORY)?);
                }
                _ => return Err(invalid("archive base contains traversal")),
            }
        }
        for name in ["context-archives", scope] {
            dir = Self(dir.open(&c_name(name)?, libc::O_RDONLY | libc::O_DIRECTORY)?);
        }
        Ok(dir)
    }

    fn fd(&self) -> libc::c_int {
        use std::os::fd::AsRawFd;
        self.0.as_raw_fd()
    }
    fn child(&self, name: &str, private: bool) -> io::Result<Self> {
        use crate::core::private_fs::DIR_MODE;
        use std::os::unix::fs::PermissionsExt;
        let name = c_name(name)?;
        let created = unsafe { libc::mkdirat(self.fd(), name.as_ptr(), DIR_MODE) } == 0;
        if !created {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EEXIST) {
                return Err(error);
            }
        }
        let file = self.open(&name, libc::O_RDONLY | libc::O_DIRECTORY)?;
        if private || created {
            file.set_permissions(std::fs::Permissions::from_mode(DIR_MODE))?;
        }
        // A previous mkdir may have succeeded but its parent fsync failed.
        // EEXIST on retry is not evidence that the directory entry is durable.
        self.sync()?;
        Ok(Self(file))
    }
    fn open(&self, name: &std::ffi::CStr, flags: i32) -> io::Result<std::fs::File> {
        use std::os::fd::FromRawFd;
        let fd = unsafe {
            libc::openat(
                self.fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                crate::core::private_fs::FILE_MODE,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { std::fs::File::from_raw_fd(fd) })
        }
    }
    fn check_readonly(file: &std::fs::File) -> io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.nlink() != 1 || meta.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "archive requires owned regular single-link files",
            ));
        }
        Ok(())
    }
    fn check(file: &std::fs::File) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        Self::check_readonly(file)?;
        file.set_permissions(std::fs::Permissions::from_mode(
            crate::core::private_fs::FILE_MODE,
        ))
    }
    fn lock_existing(&self) -> io::Result<Option<std::fs::File>> {
        let file = match self.open(&c_name("lock")?, libc::O_RDONLY) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        Self::check_readonly(&file)?;
        if !fs4::fs_std::FileExt::try_lock_shared(&file)? {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "archive busy; retry export",
            ));
        }
        Ok(Some(file))
    }
    fn lock(&self) -> io::Result<std::fs::File> {
        let file = self.open(&c_name("lock")?, libc::O_RDWR | libc::O_CREAT)?;
        Self::check(&file)?;
        if !fs4::fs_std::FileExt::try_lock_exclusive(&file)? {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "archive busy; retain source and retry",
            ));
        }
        Ok(file)
    }
    fn read_with(
        &self,
        slot: usize,
        remaining: usize,
        readonly: bool,
    ) -> io::Result<Option<Vec<u8>>> {
        let file = match self.open(&c_name(&format!("{slot:03}.json"))?, libc::O_RDONLY) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if readonly {
            Self::check_readonly(&file)?;
        } else {
            Self::check(&file)?;
        }
        let limit = remaining.min(MAX_SEGMENT_BYTES);
        if file.metadata()?.len() > limit as u64 {
            return Err(corrupt());
        }
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            return Err(corrupt());
        }
        Ok(Some(bytes))
    }
    fn publish(&self, slot: usize, bytes: &[u8]) -> io::Result<()> {
        let target = c_name(&format!("{slot:03}.json"))?;
        let tmp = c_name("pending.tmp")?;
        // Under the lock, an old temporary file is an unpublished failed write.
        // unlinkat never follows a link; existing content is never reused.
        if unsafe { libc::unlinkat(self.fd(), tmp.as_ptr(), 0) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::NotFound {
                return Err(error);
            }
        }
        let result = (|| {
            let mut file = self.open(&tmp, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            // Refuse any target that appeared as a symlink or hardlink.
            match self.open(&target, libc::O_RDONLY) {
                Ok(existing) => Self::check(&existing)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            if unsafe { libc::renameat(self.fd(), tmp.as_ptr(), self.fd(), target.as_ptr()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            self.sync()
        })();
        if result.is_err() {
            unsafe {
                libc::unlinkat(self.fd(), tmp.as_ptr(), 0);
            }
        }
        result
    }
    fn sync(&self) -> io::Result<()> {
        self.0.sync_all()
    }
}
#[cfg(unix)]
fn c_name(name: &str) -> io::Result<std::ffi::CString> {
    if name.is_empty() || matches!(name, "." | "..") || name.contains(['/', '\\']) {
        return Err(invalid("invalid archive component"));
    }
    std::ffi::CString::new(name).map_err(|_| invalid("invalid archive component"))
}

#[cfg(not(unix))]
struct PrivateDir;
#[cfg(not(unix))]
impl PrivateDir {
    fn unsupported<T>() -> io::Result<T> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private context archive requires Unix",
        ))
    }
    fn new(_: &Path, _: &str) -> io::Result<Self> {
        Self::unsupported()
    }
    fn lock(&self) -> io::Result<std::fs::File> {
        Self::unsupported()
    }
    fn open_existing(_: &Path, _: &str) -> io::Result<Self> {
        Self::unsupported()
    }
    fn lock_existing(&self) -> io::Result<Option<std::fs::File>> {
        Self::unsupported()
    }
    fn read_with(&self, _: usize, _: usize, _: bool) -> io::Result<Option<Vec<u8>>> {
        Self::unsupported()
    }
    fn publish(&self, _: usize, _: &[u8]) -> io::Result<()> {
        Self::unsupported()
    }
    fn sync(&self) -> io::Result<()> {
        Self::unsupported()
    }
}

#[cfg(test)]
mod projection_tests {
    use super::*;

    fn excluded_fixture(blob: &str) -> Vec<SharedMessage> {
        vec![
            Arc::new(json!({"role":"system","content":blob})),
            Arc::new(json!({"role":"developer","content":blob})),
            Arc::new(json!({"role":"user","_synaps_context":{"note":blob},"content":blob})),
            Arc::new(json!({"role":"user","retention":"local_only","content":blob})),
            Arc::new(json!({"role":"assistant","metadata":blob,"content":[
                {"type":"thinking","thinking":blob},
                {"type":"text","private":true,"text":blob},
                {"type":"image","source":{"data":blob}},
                {"type":"document","source":{"data":blob}},
                {"type":"text","text":" exact evidence αβ\n","metadata":blob},
                {"type":"tool_use","id":"cp","name":"context_checkpoint","input":{"note":blob}}
            ]})),
            Arc::new(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"r1","metadata":blob,"content":[
                    {"type":"image","source":{"data":blob}},
                    {"type":"text","sensitivity":"secret","text":blob},
                    {"type":"text","text":"exact tool evidence\n","metadata":blob}
                ]}
            ]})),
        ]
    }

    #[test]
    fn excluded_aggregate_does_not_consume_eligible_budget_or_change_digest() {
        // Every blob fits by itself; collectively the raw input exceeds 16MiB.
        let messages = excluded_fixture(&"x".repeat(2 * 1024 * 1024));
        let source = messages.clone();
        for redactor in [None, Some(Arc::new(str::to_owned) as Arc<ArchiveRedactor>)] {
            let small =
                project_archive("excluded", &excluded_fixture("x"), "note", redactor.clone())
                    .unwrap();
            let large = project_archive("excluded", &messages, "note", redactor).unwrap();
            assert!(small == large);
            assert_eq!(large.source_message_count, 6);
            assert_eq!(large.messages[0].source_index, 4);
            assert_eq!(large.messages[0].block_indices, vec![4, 5]);
        }
        for (old, new) in source.iter().zip(&messages) {
            assert!(Arc::ptr_eq(old, new));
        }
    }

    #[test]
    fn eligible_aggregate_refuses_before_any_redactor_call() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let redactor: Arc<ArchiveRedactor> = Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            "small".to_owned()
        });
        let piece = "x".repeat(MAX_INPUT_BYTES / 2);
        for messages in [
            vec![
                Arc::new(json!({"role":"user","content":piece})),
                Arc::new(json!({"role":"assistant","content":piece})),
            ],
            vec![Arc::new(json!({"role":"user","content":[{
                "type":"tool_result","tool_use_id":"r1","content":[
                    {"type":"text","text":piece}, {"type":"text","text":piece}
                ]
            }]}))],
        ] {
            let before = messages.clone();
            let error = project_archive("overflow", &messages, "", Some(redactor.clone()))
                .err()
                .unwrap();
            assert!(error.to_string().contains("input byte budget"));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(before, messages);
        }
    }

    #[test]
    fn large_tool_results_exact_with_redactor_opaque_without_it() {
        let text = "evidence αβ\n".repeat(250_000);
        let make = |text: &str| {
            vec![Arc::new(json!({"role":"user","content":[{
                "type":"tool_result","tool_use_id":"r1","content":text
            }]}))]
        };
        let p = project_archive("logs", &make(&text), "", Some(Arc::new(str::to_owned))).unwrap();
        assert_eq!(p.messages[0].message["content"][0]["content"], text);
        let huge = make(&"x".repeat(MAX_INPUT_BYTES + 1));
        let p = project_archive("logs", &huge, "", None).unwrap();
        assert_eq!(p.messages[0].message["content"][0]["content"], WITHHELD);
        assert!(project_archive("logs", &huge, "", Some(Arc::new(str::to_owned))).is_err());
    }

    #[test]
    fn whole_argument_withholding_discards_tentative_bytes_not_structural_guards() {
        let huge = "x".repeat(MAX_INPUT_BYTES + 1);
        let make = |input: Value| {
            vec![Arc::new(json!({"role":"assistant","content":[{
                "type":"tool_use","id":"r1","name":"read","input":input
            }]}))]
        };
        // Both orders: the otherwise eligible field can precede the exclusion.
        for input in [
            json!({"a":huge,"z":{"sensitivity":"secret","body":huge}}),
            json!({"a":{"sensitivity":"secret","body":huge},"z":huge}),
        ] {
            let p =
                project_archive("input", &make(input), "", Some(Arc::new(str::to_owned))).unwrap();
            assert_eq!(
                p.messages[0].message["content"][0]["input"],
                json!({"_archive_withheld":true})
            );
        }
        // Arbitrary binary-looking tool arguments are NOT content blocks.
        let input = json!({"type":"image","data":"YWJj"});
        let p = project_archive(
            "input",
            &make(input.clone()),
            "",
            Some(Arc::new(str::to_owned)),
        )
        .unwrap();
        assert_eq!(p.messages[0].message["content"][0]["input"], input);
        let large = make(json!({"type":"image","data":huge}));
        assert!(project_archive("input", &large, "", Some(Arc::new(str::to_owned))).is_err());
        assert!(project_archive("input", &large, "", None).is_ok());
    }

    #[test]
    fn visited_structure_is_bounded_but_excluded_descendants_are_opaque() {
        let mut deep = json!("leaf");
        for _ in 0..40 {
            deep = json!([deep]);
        }
        let make = |input: Value| {
            vec![Arc::new(json!({"role":"assistant","content":[{
                "type":"tool_use","id":"r1","name":"read","input":input
            }]}))]
        };
        let identity = Some(Arc::new(str::to_owned) as Arc<ArchiveRedactor>);
        assert!(project_archive("depth", &make(deep.clone()), "", identity.clone()).is_err());
        assert!(project_archive("depth", &make(deep.clone()), "", None).is_ok());
        assert!(project_archive(
            "depth",
            &make(json!({"private":true,"blob":deep})),
            "",
            identity.clone()
        )
        .is_ok());
        // A malformed content block isn't recursively interpreted as a block array.
        assert!(project_archive(
            "depth",
            &[Arc::new(json!({"role":"user","content":deep}))],
            "",
            identity.clone()
        )
        .is_ok());
        let wide = vec![Value::Null; 65536];
        assert!(project_archive("nodes", &make(json!(wide)), "", identity.clone()).is_err());
        let excluded = vec![Arc::new(json!({"role":"user","content":wide}))];
        assert!(project_archive("nodes", &excluded, "", identity).is_err());
    }

    #[test]
    fn withheld_after_redaction_text_is_not_scanned_without_redactor() {
        let messages = vec![Arc::new(json!({"role":"user",
            "disclosure":"model_visible_after_redaction", "content":"x".repeat(MAX_INPUT_BYTES+1)
        }))];
        let p = project_archive("withheld", &messages, "", None).unwrap();
        assert_eq!(p.messages[0].message["content"], WITHHELD);
        assert!(project_archive("withheld", &messages, "", Some(Arc::new(str::to_owned))).is_err());
    }

    #[test]
    fn json_escape_scratch_can_exceed_segment_limit_before_redaction() {
        // 3MiB source ->18MiB JSON, accepted by the old raw preflight and then
        // reduced by the trusted redactor. Do not introduce a new 16MiB scratch cap.
        let input = json!({"text":"\0".repeat(3 * 1024 * 1024)});
        let messages = vec![Arc::new(json!({"role":"assistant","content":[{
            "type":"tool_use","id":"r1","name":"read","input":input
        }]}))];
        let redactor: Arc<ArchiveRedactor> = Arc::new(|s| {
            if s.starts_with('{') {
                assert!(s.len() > MAX_SEGMENT_BYTES);
                "{\"text\":\"screened\"}".to_owned()
            } else {
                s.to_owned()
            }
        });
        let p = project_archive("escaping", &messages, "", Some(redactor)).unwrap();
        assert_eq!(
            p.messages[0].message["content"][0]["input"],
            json!({"text":"screened"})
        );
        assert!(project_archive("escaping", &messages, "", Some(Arc::new(str::to_owned))).is_err());
    }

    fn compatibility_fixture() -> Vec<SharedMessage> {
        vec![
            Arc::new(json!({"role":"system","content":"omitted"})),
            Arc::new(json!({"role":"user","content":" exact αβ\n"})),
            Arc::new(json!({"role":"assistant","content":[
                {"type":"thinking","thinking":"private"},
                {"type":"text","text":"classified-value"},
                {"type":"tool_use","id":"r1","name":"read","input":{"path":"src/lib.rs","n":7,"binary":{"type":"image","data":"YWJj"}}},
                {"type":"tool_use","id":"c1","name":"context_checkpoint","input":{"note":"hidden"}},
                {"type":"tool_use","id":"x1","name":"read","input":{"nested":{"disclosure":"local_only","body":"omitted"}}}
            ]})),
            Arc::new(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"r1","is_error":false,"content":[
                    {"type":"image","data":"omitted"},
                    {"type":"text","text":"exact result\n"},
                    {"type":"text","text":"classified-value","disclosure":"model_visible_after_redaction"}
                ]}
            ]})),
            Arc::new(
                json!({"role":"user","disclosure":"model_visible_after_redaction","content":"classified-value"}),
            ),
            Arc::new(json!({"role":"user","_synaps_context":{},"content":"hidden"})),
        ]
    }

    #[test]
    fn legacy_projection_golden() {
        // Captured against the legacy raw-input preflight before this fix.
        for (redactor, digest) in [
            (
                None,
                "aeb544a71629f741def049575eefb1a765117d65aa914bdcc1657fa677622031",
            ),
            (
                Some(
                    Arc::new(|s: &str| s.replace("classified-value", "[redacted]"))
                        as Arc<ArchiveRedactor>,
                ),
                "d70b321210ca3d2bc531b31a35cc02ee267eeb572e8a6dd88f1fb62725b082ec",
            ),
        ] {
            let p = project_archive(
                "compat-session",
                &compatibility_fixture(),
                "hidden note",
                redactor,
            )
            .unwrap();
            assert_eq!(p.digest, digest);
        }
    }

    #[test]
    fn pure_projection_keeps_legacy_logical_digest_and_never_opens_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let logical = tmp
            .path()
            .join("not-created/session")
            .to_str()
            .unwrap()
            .to_owned();
        let messages = vec![
            Arc::new(json!({"role":"system","content":"OMITTED"})),
            Arc::new(json!({"role":"assistant","content":[
                {"type":"thinking","thinking":"OMITTED"},
                {"type":"text","text":" exact αβ evidence\n"}
            ]})),
        ];
        let source = serde_json::to_vec(&messages).unwrap();
        let first = project_archive(&logical, &messages, "hidden note", None).unwrap();
        assert_eq!(first.logical_id, digest_parts(&[logical.as_bytes()]));
        assert_eq!(
            first.digest,
            projection_digest(&first.logical_id, 2, &first.messages).unwrap()
        );
        assert_eq!(first.messages[0].source_index, 1);
        assert_eq!(first.messages[0].block_indices, vec![1]);
        assert_eq!(first.source_message_count, 2);
        let mut changed = messages.clone();
        changed[0] = Arc::new(json!({"role":"developer","content":"DIFFERENT OMITTED"}));
        let second = project_archive(&logical, &changed, "changed note", None).unwrap();
        assert_eq!(first.digest, second.digest);
        assert_ne!(
            first.digest,
            project_archive("other-session", &messages, "hidden note", None)
                .unwrap()
                .digest
        );
        assert_eq!(serde_json::to_vec(&messages).unwrap(), source);
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn pure_projection_shares_disclosure_redaction_and_input_limits() {
        let redactor: Arc<ArchiveRedactor> =
            Arc::new(|s| s.replace("classified-value", "[redacted]"));
        let messages = vec![
            Arc::new(json!({"role":"assistant","content":[
                {"type":"tool_use","id":"read_1","name":"read","input":{"path":"src/lib.rs"}},
                {"type":"tool_use","id":"cp","name":"context_checkpoint","input":{"note":"NOTE_ONLY"}}
            ]})),
            Arc::new(
                json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"read_1","content":"classified-value"}]}),
            ),
            Arc::new(json!({"role":"user","sensitivity":"secret","content":"OMITTED"})),
            Arc::new(json!({"role":"user","_synaps_context":{},"content":"NOTE_ONLY"})),
        ];
        let projected =
            project_archive("session", &messages, "classified-value", Some(redactor)).unwrap();
        assert_eq!(projected.note, "[redacted]");
        let json = serde_json::to_string(&projected.messages).unwrap();
        assert!(!json.contains("classified-value"));
        assert!(!json.contains("NOTE_ONLY"));
        assert!(!json.contains("OMITTED"));
        assert!(json.contains("src/lib.rs"));
        assert_eq!(projected.messages.len(), 2);
        let withheld = project_archive("session", &messages, "password=bad", None).unwrap();
        assert_eq!(withheld.note, WITHHELD);
        assert!(!serde_json::to_string(&withheld.messages)
            .unwrap()
            .contains("src/lib.rs"));
        assert!(project_archive("", &[], "", None).is_err());
        assert!(project_archive(&"l".repeat(4097), &[], "", None).is_err());
        assert!(project_archive("s", &[], &"n".repeat(MAX_NOTE_BYTES + 1), None).is_err());
        assert!(
            project_archive("s", &vec![messages[0].clone(); MAX_MESSAGES + 1], "", None).is_err()
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn msg(role: &str, content: Value) -> SharedMessage {
        Arc::new(json!({"role":role,"content":content}))
    }
    fn setup() -> (tempfile::TempDir, ArchiveStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store =
            ArchiveStore::new(tmp.path(), "synthetic-project", "synthetic-logical").unwrap();
        (tmp, store)
    }
    fn directory(tmp: &tempfile::TempDir, store: &ArchiveStore) -> std::path::PathBuf {
        tmp.path().join("context-archives").join(&store.scope)
    }

    #[test]
    fn export_preview_is_read_only_and_matches_projection_including_tombstones() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        assert!(export_in(&tmp.path().join("missing"), "synthetic-project")
            .unwrap()
            .is_empty());
        assert!(export_in(tmp.path(), "synthetic-project")
            .unwrap()
            .is_empty());
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
        let store =
            ArchiveStore::new(tmp.path(), "synthetic-project", "synthetic-logical").unwrap();
        let dir = directory(&tmp, &store);
        // Even a newly opened empty store has no lock until its first operation.
        assert!(export_in(tmp.path(), "synthetic-project")
            .unwrap()
            .is_empty());
        assert!(!dir.join("lock").exists());
        let messages = vec![msg("user", json!("migration evidence"))];
        let reference = store.seal(&messages, "hidden note").unwrap();
        let projection =
            project_archive("synthetic-logical", &messages, "hidden note", None).unwrap();
        let tombstone = store
            .seal(&[msg("assistant", json!("remove me"))], "remove note")
            .unwrap();
        store.forget(&tombstone.id).unwrap();
        let mut paths = vec![
            tmp.path().to_path_buf(),
            tmp.path().join("context-archives"),
            dir.clone(),
        ];
        paths.extend(std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()));
        // Export must not silently repair permissions, unlike normal archive IO.
        for path in &paths {
            let mode = if path.is_dir() { 0o750 } else { 0o640 };
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let snapshot = || {
            paths
                .iter()
                .map(|p| {
                    let m = std::fs::metadata(p).unwrap();
                    (
                        m.mode(),
                        m.mtime(),
                        m.mtime_nsec(),
                        if m.is_file() {
                            std::fs::read(p).unwrap()
                        } else {
                            vec![]
                        },
                    )
                })
                .collect::<Vec<_>>()
        };
        let before = snapshot();
        let rows = export_in(tmp.path(), "synthetic-project").unwrap();
        assert_eq!(rows, store.export_records().unwrap());
        assert_eq!(before, snapshot());
        assert_eq!(
            rows[0],
            json!({"id":reference.id,"digest":projection.digest,"logical_id":projection.logical_id,
            "source_message_count":1,"messages":projection.messages,"note":"hidden note","tombstone":false})
        );
        assert_eq!(rows[1]["id"], tombstone.id);
        assert_eq!(rows[1]["tombstone"], true);
        assert_eq!(rows[1]["source_message_count"], 0);
        assert_eq!(rows[1]["messages"], json!([]));
        assert_eq!(rows[1]["note"], "");
        let lock = store.fs.lock().unwrap();
        assert_eq!(
            export_in(tmp.path(), "synthetic-project")
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(lock);
        std::fs::write(dir.join("000.json"), b"corrupt").unwrap();
        assert!(export_in(tmp.path(), "synthetic-project").is_err());
    }

    #[test]
    fn export_preview_refuses_links_and_never_creates_a_missing_lock() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let r = store
            .seal(&[msg("user", json!("evidence"))], "note")
            .unwrap();
        let path = dir.join("000.json");
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(dir.join("lock")).unwrap();
        assert!(export_in(tmp.path(), "synthetic-project").is_err());
        assert!(!dir.join("lock").exists());
        // The normal API may recreate its lock, but preview cannot.
        assert!(store.fetch(&r.id, 0, 1).is_ok());
        std::fs::remove_file(&path).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::write(&outside, &bytes).unwrap();
        symlink(&outside, &path).unwrap();
        assert!(export_in(tmp.path(), "synthetic-project").is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::hard_link(&outside, &path).unwrap();
        assert!(export_in(tmp.path(), "synthetic-project").is_err());
        let alias = tmp.path().join("alias");
        symlink(tmp.path(), &alias).unwrap();
        assert!(export_in(&alias, "synthetic-project").is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), bytes);
    }

    #[test]
    fn attachments_remain_outside_eligible_memory_archive() {
        let (tmp, store) = setup();
        let messages = vec![msg(
            "user",
            json!([
                {"type":"text","text":"Compare the selected files"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"ATTACHMENT_IMAGE_SECRET"}},
                {"type":"document","title":"ATTACHMENT_FILENAME_SECRET","source":{"type":"base64","media_type":"application/pdf","data":"ATTACHMENT_PDF_SECRET"}},
                {"type":"document","source":{"type":"text","media_type":"text/plain","data":"ATTACHMENT_TEXT_SECRET"}}
            ]),
        )];
        let reference = store.seal(&messages, "safe checkpoint").unwrap();
        let fetched = store.fetch(&reference.id, 0, 10).unwrap();
        let serialized = serde_json::to_string(&fetched).unwrap();
        assert!(serialized.contains("Compare the selected files"));
        assert!(!serialized.contains("ATTACHMENT_"));
        let raw = std::fs::read_to_string(directory(&tmp, &store).join("000.json")).unwrap();
        assert!(!raw.contains("ATTACHMENT_"));
        assert!(messages[0].to_string().contains("ATTACHMENT_TEXT_SECRET"));
    }

    #[test]
    fn exact_text_source_indices_note_and_restart() {
        let (tmp, store) = setup();
        let messages = vec![
            msg("system", json!("FORBIDDEN SYSTEM")),
            msg("user", json!(" exact\n αβ  ")),
            msg(
                "assistant",
                json!([
                    {"type":"thinking","thinking":"FORBIDDEN REASONING"},
                    {"type":"text","text":"evidence✓", "cache_control":{"type":"ephemeral"}},
                    {"type":"image","data":"FORBIDDEN IMAGE"}
                ]),
            ),
            msg("developer", json!("FORBIDDEN DEVELOPER")),
        ];
        let reference = store.seal(&messages, "hidden routing note").unwrap();
        assert_eq!(reference.message_count, 2);
        assert_eq!(reference.source_message_count, 4);
        let reopened =
            ArchiveStore::new(tmp.path(), "synthetic-project", "synthetic-logical").unwrap();
        let fetched = reopened
            .fetch_with_provenance(&reference.id, 0, 128)
            .unwrap();
        assert_eq!(fetched[0].message, messages[1]);
        assert_eq!(fetched[0].source_index, 1);
        assert_eq!(fetched[1].source_index, 2);
        assert_eq!(fetched[1].block_indices, vec![1]);
        assert_eq!(
            reopened.fetch_note(&reference.id).unwrap(),
            "hidden routing note"
        );
        assert!(reopened.search("hidden routing", 10).unwrap().is_empty());
        assert_eq!(reopened.search("evidence", 10).unwrap()[0].id, reference.id);
        assert!(reopened
            .fetch(&reference.id, usize::MAX, 10)
            .unwrap()
            .is_empty());
        let raw = std::fs::read_to_string(directory(&tmp, &store).join("000.json")).unwrap();
        assert!(!raw.contains("FORBIDDEN"));
        assert!(!raw.contains("cache_control"));
        assert_eq!(messages[0]["content"], "FORBIDDEN SYSTEM");
    }

    #[test]
    fn duplicate_retry_ignores_note_but_not_logical_id() {
        let (tmp, store) = setup();
        let messages = vec![msg("user", json!("same eligible words"))];
        let first = store.seal(&messages, "first").unwrap();
        assert_eq!(store.seal(&messages, "second").unwrap(), first);
        assert_eq!(store.fetch_note(&first.id).unwrap(), "first");
        let other = ArchiveStore::new(tmp.path(), "synthetic-project", "other-logical").unwrap();
        let second = other.seal(&messages, "").unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(
            project_search(tmp.path(), "synthetic-project", "", 10)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            project_fetch(tmp.path(), "synthetic-project", &first.id, 0, 10).unwrap(),
            messages
        );
        assert_eq!(other.fetch(&first.id, 0, 1).unwrap(), messages);
        store.forget(&first.id).unwrap();
        assert!(store.seal(&messages, "third").is_err());
        assert_eq!(other.fetch(&second.id, 0, 1).unwrap(), messages);
        assert_eq!(other.seal(&messages, "third").unwrap(), second);
    }

    #[test]
    fn deletion_is_content_free_durable_and_cannot_resurrect() {
        let (tmp, store) = setup();
        let messages = vec![msg("user", json!("uniquely removable content"))];
        let reference = store.seal(&messages, "removable note").unwrap();
        store.forget(&reference.id).unwrap();
        store.forget(&reference.id).unwrap();
        let raw = std::fs::read_to_string(directory(&tmp, &store).join("000.json")).unwrap();
        assert!(raw.contains("tombstone"));
        assert!(!raw.contains("removable"));
        assert!(!raw.contains("source_message_count"));
        let store =
            ArchiveStore::new(tmp.path(), "synthetic-project", "synthetic-logical").unwrap();
        assert_eq!(
            store.fetch(&reference.id, 0, 1).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(store.fetch_note(&reference.id).is_err());
        assert!(store.search("", 10).unwrap().is_empty());
        assert_eq!(
            store.seal(&messages, "changed note").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn foreign_scope_copied_records_and_traversal_fail_closed() {
        let (tmp, store) = setup();
        let reference = store
            .seal(&[msg("user", json!("scope evidence"))], "")
            .unwrap();
        let foreign =
            ArchiveStore::new(tmp.path(), "foreign-project", "synthetic-logical").unwrap();
        assert!(foreign.fetch(&reference.id, 0, 1).is_err());
        for id in [
            "../000.json",
            "/tmp/file",
            "",
            "a/../b",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ] {
            assert_eq!(
                store.fetch(id, 0, 1).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert!(store.forget(id).is_err());
        }
        std::fs::copy(
            directory(&tmp, &store).join("000.json"),
            directory(&tmp, &foreign).join("000.json"),
        )
        .unwrap();
        assert_eq!(
            foreign.search("", 1).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(ArchiveStore::new(&tmp.path().join("../escape"), "p", "l").is_err());
    }

    #[test]
    fn disclosure_and_raw_tool_data_are_withheld() {
        let (tmp, store) = setup();
        let mut messages = vec![
            msg(
                "assistant",
                json!([{"type":"tool_use","id":"call_1","name":"read","input":{"path":"unclassified raw argument"}}]),
            ),
            msg(
                "user",
                json!([{"type":"tool_result","tool_use_id":"call_1","content":"unclassified raw output"}]),
            ),
            msg("user", json!("password=synthetic-credential")),
        ];
        for class in [
            "local_only",
            "persist_never_transmit",
            "never_persist",
            "model_visible_after_consent",
            "unknown",
        ] {
            messages.push(Arc::new(
                json!({"role":"user","content":"FORBIDDEN DISCLOSURE","disclosure":class}),
            ));
        }
        messages.push(Arc::new(
            json!({"role":"user","content":"FORBIDDEN SENSITIVITY","sensitivity":"secret"}),
        ));
        let reference = store.seal(&messages, "").unwrap();
        assert_eq!(reference.message_count, 3);
        let raw = std::fs::read_to_string(directory(&tmp, &store).join("000.json")).unwrap();
        for forbidden in ["unclassified raw", "synthetic-credential", "FORBIDDEN"] {
            assert!(!raw.contains(forbidden));
        }
        assert!(raw.contains("call_1"));
        assert!(raw.contains("_archive_withheld"));
    }

    #[test]
    fn derived_context_envelope_is_not_searchable_source() {
        let (_tmp, store) = setup();
        let first = store
            .seal(
                &[msg("assistant", json!("original evidence"))],
                "NOTE_ONLY_SENTINEL",
            )
            .unwrap();
        let second=store.seal(&[
            Arc::new(json!({"role":"user","_synaps_context":{"schema":"synaps-context-window/1","archive":first.id},"content":"NOTE_ONLY_SENTINEL"})),
            Arc::new(json!({"role":"assistant","content":[{"type":"tool_use","id":"checkpoint-note","name":"context_checkpoint","input":{"phase":"plan","note":"NOTE_ONLY_SENTINEL"}}]})),
            msg("assistant",json!("second-window evidence")),
        ],"next note").unwrap();
        assert!(store.search("NOTE_ONLY_SENTINEL", 10).unwrap().is_empty());
        let fetched = store.fetch_with_provenance(&second.id, 0, 10).unwrap();
        assert_eq!(fetched.len(), 2);
        assert_eq!(fetched[0].source_index, 1);
        assert_eq!(fetched[1].source_index, 2);
        assert!(!serde_json::to_string(&fetched)
            .unwrap()
            .contains("NOTE_ONLY_SENTINEL"));
    }

    #[test]
    fn checkpoint_tool_note_stays_hidden_even_with_redactor() {
        let (_tmp, store) = setup();
        let store = store.with_redactor(Arc::new(str::to_owned));
        let reference = store
            .seal(
                &[msg(
                    "assistant",
                    json!([
                        {"type":"tool_use","id":"checkpoint-note","name":"context_checkpoint",
                         "input":{"phase":"plan","note":"NOTE_ARGUMENT_SENTINEL"}}
                    ]),
                )],
                "NOTE_ARGUMENT_SENTINEL",
            )
            .unwrap();
        assert!(store
            .search("NOTE_ARGUMENT_SENTINEL", 10)
            .unwrap()
            .is_empty());
        assert!(
            !serde_json::to_string(&store.fetch(&reference.id, 0, 10).unwrap())
                .unwrap()
                .contains("NOTE_ARGUMENT_SENTINEL")
        );
        assert_eq!(
            store.fetch_note(&reference.id).unwrap(),
            "NOTE_ARGUMENT_SENTINEL"
        );
    }

    #[test]
    fn trusted_redactor_retains_exact_eligible_tool_evidence() {
        let (_tmp, store) = setup();
        let store = store.with_redactor(Arc::new(|s| s.replace("classified-value", "[redacted]")));
        let messages = vec![
            msg(
                "assistant",
                json!([{"type":"tool_use","id":"call_1","name":"read","input":{"path":"src/lib.rs","line":7}}]),
            ),
            msg(
                "user",
                json!([{"type":"tool_result","tool_use_id":"call_1","is_error":false,"content":"line 7\n  exact evidence\nclassified-value"}]),
            ),
        ];
        let reference = store.seal(&messages, "").unwrap();
        let fetched = store.fetch(&reference.id, 0, 10).unwrap();
        assert_eq!(fetched[0], messages[0]);
        assert_eq!(
            fetched[1]["content"][0]["content"],
            "line 7\n  exact evidence\n[redacted]"
        );
    }

    #[test]
    fn oversized_input_output_limits_and_depth_leave_source_intact() {
        let (_tmp, store) = setup();
        let huge = vec![msg("user", json!("x".repeat(MAX_INPUT_BYTES + 1)))];
        assert!(store.seal(&huge, "").is_err());
        assert_eq!(
            huge[0]["content"].as_str().unwrap().len(),
            MAX_INPUT_BYTES + 1
        );
        let file_too_large = vec![msg("user", json!("\n".repeat(MAX_SEGMENT_BYTES / 2)))];
        assert!(store.seal(&file_too_large, "").is_err());
        assert!(store
            .seal(&vec![msg("user", json!("a")); MAX_MESSAGES + 1], "")
            .is_err());
        assert!(store.seal(&[], &"n".repeat(MAX_NOTE_BYTES + 1)).is_err());
        let mut deep = json!("value");
        for _ in 0..40 {
            deep = json!([deep]);
        }
        // Unknown content blocks are opaque; admitted tool JSON still has a
        // strict recursion limit before serialization or redaction.
        let deep = vec![msg(
            "assistant",
            json!([{
                "type":"tool_use", "id":"r1", "name":"read", "input":deep
            }]),
        )];
        let store = store.with_redactor(Arc::new(str::to_owned));
        assert!(store.seal(&deep, "").is_err());
        assert!(store.search(&"q".repeat(513), 1).is_err());
        assert!(store.search("", MAX_SEARCH_RESULTS + 1).is_err());
        assert!(store
            .fetch(&"a".repeat(32), 0, MAX_FETCH_MESSAGES + 1)
            .is_err());
        assert!(store.search("", 10).unwrap().is_empty());
    }

    #[test]
    fn torn_final_fails_closed_stale_temporary_is_not_visible() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        std::fs::write(dir.join("pending.tmp"), b"unpublished partial").unwrap();
        assert!(store.search("", 1).unwrap().is_empty());
        let reference = store.seal(&[msg("user", json!("complete"))], "").unwrap();
        assert!(!dir.join("pending.tmp").exists());
        std::fs::write(dir.join("000.json"), b"{\"version\":1,").unwrap();
        assert_eq!(
            store.fetch(&reference.id, 0, 1).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(store.search("", 1).is_err());
        assert!(store.seal(&[msg("user", json!("next"))], "").is_err());
        assert!(store.forget(&reference.id).is_err());
    }

    #[test]
    fn private_modes_symlink_hardlink_fifo_and_lock_refusal() {
        let (tmp, store) = setup();
        let dir = directory(&tmp, &store);
        let reference = store.seal(&[msg("user", json!("private"))], "").unwrap();
        for path in [
            tmp.path().to_path_buf(),
            tmp.path().join("context-archives"),
            dir.clone(),
        ] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        for name in ["000.json", "lock"] {
            assert_eq!(
                std::fs::metadata(dir.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let lock = store.fs.lock().unwrap();
        assert_eq!(
            store.fetch(&reference.id, 0, 1).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(lock);
        let victim = tmp.path().join("synthetic-victim");
        std::fs::write(&victim, "do not change").unwrap();
        std::fs::remove_file(dir.join("000.json")).unwrap();
        symlink(&victim, dir.join("000.json")).unwrap();
        assert!(store.search("", 1).is_err());
        assert!(store.seal(&[msg("user", json!("private"))], "").is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "do not change");
        std::fs::remove_file(dir.join("000.json")).unwrap();
        std::fs::hard_link(&victim, dir.join("000.json")).unwrap();
        assert!(store.search("", 1).is_err());
        std::fs::remove_file(dir.join("000.json")).unwrap();
        let fifo = std::ffi::CString::new(dir.join("000.json").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(store.search("", 1).is_err());
        let alias = tmp.path().join("alias");
        symlink(tmp.path().join("context-archives"), &alias).unwrap();
        assert!(ArchiveStore::new(&alias.join("nested"), "p", "l").is_err());
        std::fs::remove_file(dir.join("lock")).unwrap();
        symlink(&victim, dir.join("lock")).unwrap();
        assert!(store.fs.lock().is_err());
    }

    #[test]
    fn multi_mib_context_roundtrips_without_truncation() {
        let (_tmp, store) = setup();
        let text = "eligible exact evidence\n".repeat(400_000);
        assert!(text.len() > 8 * 1024 * 1024);
        let messages = vec![msg("user", json!(text))];
        let reference = store.seal(&messages, "large source window").unwrap();
        assert_eq!(store.fetch(&reference.id, 0, 1).unwrap(), messages);
        assert_eq!(store.seal(&messages, "retry").unwrap(), reference);
    }

    #[test]
    fn private_reasoning_disguised_as_assistant_text_is_omitted() {
        let (_tmp, store) = setup();
        let messages = vec![
            Arc::new(json!({"role":"assistant","channel":"analysis","content":"private analysis"})),
            Arc::new(
                json!({"role":"assistant","content_class":"private_reasoning","content":"private reasoning"}),
            ),
            Arc::new(json!({"role":"assistant","private":true,"content":"private text"})),
            Arc::new(json!({"role":"assistant","channel":"final","content":"public evidence"})),
        ];
        let reference = store.seal(&messages, "").unwrap();
        assert_eq!(reference.message_count, 1);
        let fetched = store.fetch_with_provenance(&reference.id, 0, 1).unwrap();
        assert_eq!(fetched[0].source_index, 3);
        assert_eq!(fetched[0].message["content"], "public evidence");
    }

    #[test]
    fn after_redaction_requires_host_redactor_including_parent_class() {
        let (_tmp, store) = setup();
        let messages = vec![Arc::new(json!({
            "role":"assistant", "disclosure":"model_visible_after_redaction",
            "content":[{"type":"text", "text":"sensitive host classified material"}]
        }))];
        let reference = store.seal(&messages, "").unwrap();
        let fetched = store.fetch(&reference.id, 0, 1).unwrap();
        assert_eq!(fetched[0]["content"][0]["text"], WITHHELD);
    }

    #[test]
    fn bounded_search_utf8_and_permanent_capacity() {
        let (_tmp, store) = setup();
        for index in 0..MAX_SEGMENTS {
            store
                .seal(
                    &[msg("user", json!(format!("{index} {}", "é".repeat(200))))],
                    "",
                )
                .unwrap();
        }
        let descriptors = store.search("", MAX_SEARCH_RESULTS).unwrap();
        assert_eq!(descriptors.len(), MAX_SEARCH_RESULTS);
        assert!(descriptors.iter().all(|d| d.snippet.len() <= 256));
        assert!(store.seal(&[msg("user", json!("overflow"))], "").is_err());
        store.forget(&descriptors[0].id).unwrap();
        assert!(store.seal(&[msg("user", json!("still full"))], "").is_err());
    }
}
