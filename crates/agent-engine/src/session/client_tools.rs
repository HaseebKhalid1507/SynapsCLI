//! Client tools v1 on the wire (docs/client-tools.md): the checks of
//! `register_client_tools` and `tool_return`, and the four frames the
//! session actor addresses to one client only (`client_tools`,
//! `tool_invoke`, `tool_cancel`, `tool_return_ack`).
//!
//! The overlay itself (registration, pending calls, the handler) is
//! [`crate::runtime::client_tools`]. Nothing here is persisted, replayed or
//! traced; the manual `Debug`s never print a description, an input schema,
//! call arguments or returned content.
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::types::ClientId;
use crate::runtime::client_tools::{
    mint_registration_id, Registration, ToolDef, MAX_CONTENT_BYTES, MAX_PENDING, MAX_TOOLS, TIMEOUT,
};

/// The contract version `register_client_tools` must name.
pub const CONTRACT: u64 = 1;
/// The `Welcome.features` entry.
pub const FEATURE: &str = "client_tools_v1";
/// A whole `register_client_tools`, serialized.
pub const MAX_COMMAND_BYTES: usize = 262_144;
/// One tool's `input_schema`, serialized.
pub const MAX_SCHEMA_BYTES: usize = 16_384;
/// One tool's `description`.
pub const MAX_DESCRIPTION_BYTES: usize = 4_096;
/// Outcome codes per tool.
pub const MAX_OUTCOMES: usize = 32;
/// The prefix of the daemon's own outcome codes.
const RESERVED_OUTCOME_PREFIX: &str = "synaps_";

static GENERATION: AtomicU64 = AtomicU64::new(1);

/// Record this daemon image's reload generation (`Welcome.generation`).
/// An in-process host never sets it: 1.
pub fn set_daemon_generation(generation: u64) {
    GENERATION.store(generation, Ordering::SeqCst);
}

/// The generation `register_client_tools` must name and the frames carry.
pub fn daemon_generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

/// Why a `register_client_tools` was refused (the closed `code` list).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    NotOwner,
    WrongGeneration,
    FeatureOff,
    ContractUnsupported,
    Busy,
    NoConversation,
    NotRootSession,
    Invalid,
    Limit,
    NameCollision,
    ExtensionMismatch,
    DefChanged,
    Omitted,
    NotMarked,
}

impl Refusal {
    pub fn code(self) -> &'static str {
        match self {
            Self::NotOwner => "not_owner",
            Self::WrongGeneration => "wrong_generation",
            Self::FeatureOff => "feature_off",
            Self::ContractUnsupported => "contract_unsupported",
            Self::Busy => "busy",
            Self::NoConversation => "no_conversation",
            Self::NotRootSession => "not_root_session",
            Self::Invalid => "invalid",
            Self::Limit => "limit",
            Self::NameCollision => "name_collision",
            Self::ExtensionMismatch => "extension_mismatch",
            Self::DefChanged => "def_changed",
            Self::Omitted => "omitted",
            Self::NotMarked => "not_marked",
        }
    }
}

/// A refusal and its message (names and limits only, never a
/// description, a schema or other submitted text).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    pub refusal: Refusal,
    pub message: String,
}

fn refused(refusal: Refusal, message: impl Into<String>) -> Refused {
    Refused { refusal, message: message.into() }
}

/// `^[a-z][rest]{0,max-1}$`, hand-rolled.
fn ident(s: &str, max: usize, rest: impl Fn(u8) -> bool) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= max
        && b[0].is_ascii_lowercase()
        && b[1..].iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || rest(c))
}

fn extension_id_ok(s: &str) -> bool {
    ident(s, 32, |c| c == b'-')
}

fn tool_name_ok(s: &str) -> bool {
    ident(s, 64, |c| c == b'_')
}

fn family_ok(s: &str) -> bool {
    ident(s, 48, |c| c == b'_' || c == b'.')
}

fn outcome_ok(s: &str) -> bool {
    ident(s, 48, |c| c == b'_') && !s.starts_with(RESERVED_OUTCOME_PREFIX)
}

fn serialized_len(value: &impl Serialize) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |v| v.len())
}

/// `contract` must be the integer 1 (absent or anything else:
/// `contract_unsupported`).
pub fn check_contract(fields: &Map<String, Value>) -> Result<(), Refused> {
    match fields.get("contract").and_then(Value::as_u64) {
        Some(CONTRACT) => Ok(()),
        _ => Err(refused(
            Refusal::ContractUnsupported,
            format!("this daemon speaks client tools contract {CONTRACT} only"),
        )),
    }
}

/// `daemon_generation` must be this image's (absent or anything else:
/// `wrong_generation`).
pub fn check_generation(fields: &Map<String, Value>) -> Result<(), Refused> {
    let current = daemon_generation();
    match fields.get("daemon_generation").and_then(Value::as_u64) {
        Some(g) if g == current => Ok(()),
        _ => Err(refused(
            Refusal::WrongGeneration,
            format!("the daemon's generation is {current}; take it from Welcome and register again"),
        )),
    }
}

/// The session's own root conversation only (`no_conversation` /
/// `not_root_session`): a conversation id, not a fork child, and not a
/// worker (a delegation parent), whose calls the §4 gate would refuse.
pub fn check_root(
    session_id: Option<&str>,
    is_fork: bool,
    delegation_parent: Option<&str>,
) -> Result<(), Refused> {
    match (session_id, is_fork, delegation_parent) {
        (None, _, _) => Err(refused(Refusal::NoConversation, "the session has no conversation id")),
        (_, true, _) => Err(refused(
            Refusal::NotRootSession,
            "this conversation is a fork child; client tools register in a root session only",
        )),
        (_, _, Some(_)) => Err(refused(
            Refusal::NotRootSession,
            "this session is a worker; client tools register in a root session only",
        )),
        _ => Ok(()),
    }
}

/// A shape-checked registration request.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    pub extension_id: String,
    pub require_memory_only: bool,
    pub tools: Vec<ToolDef>,
}

/// The field rules of `register_client_tools` (`invalid` / `limit`).
/// Unknown keys are ignored.
pub fn parse_register(fields: &Map<String, Value>) -> Result<Parsed, Refused> {
    if serialized_len(fields) > MAX_COMMAND_BYTES {
        return Err(refused(Refusal::Limit, format!("the command is over {MAX_COMMAND_BYTES} bytes")));
    }
    let invalid = |m: String| refused(Refusal::Invalid, m);
    let extension_id = match fields.get("extension_id").and_then(Value::as_str) {
        Some(id) if extension_id_ok(id) => id.to_string(),
        _ => return Err(invalid("extension_id must match ^[a-z][a-z0-9-]{0,31}$".into())),
    };
    let require_memory_only = match fields.get("require_memory_only") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(invalid("require_memory_only must be a boolean".into())),
    };
    let Some(list) = fields.get("tools").and_then(Value::as_array) else {
        return Err(invalid("tools must be an array".into()));
    };
    if list.is_empty() || list.len() > MAX_TOOLS {
        return Err(refused(Refusal::Limit, format!("tools must hold 1 to {MAX_TOOLS} tools")));
    }
    let mut tools: Vec<ToolDef> = Vec::with_capacity(list.len());
    for (i, tool) in list.iter().enumerate() {
        let def = parse_tool(i, tool)?;
        if tools.iter().any(|t| t.name == def.name) {
            return Err(invalid(format!("tools[{i}].name repeats an earlier tool's name")));
        }
        tools.push(def);
    }
    Ok(Parsed { extension_id, require_memory_only, tools })
}

fn parse_tool(i: usize, tool: &Value) -> Result<ToolDef, Refused> {
    let invalid = |m: String| refused(Refusal::Invalid, m);
    let Some(tool) = tool.as_object() else {
        return Err(invalid(format!("tools[{i}] must be an object")));
    };
    let name = match tool.get("name").and_then(Value::as_str) {
        Some(n) if tool_name_ok(n) => n.to_string(),
        _ => return Err(invalid(format!("tools[{i}].name must match ^[a-z][a-z0-9_]{{0,63}}$"))),
    };
    let description = match tool.get("description").and_then(Value::as_str) {
        Some(d)
            if !d.is_empty()
                && d.len() <= MAX_DESCRIPTION_BYTES
                && !d.chars().any(|c| c.is_control() && c != '\n' && c != '\t') =>
        {
            d.to_string()
        }
        _ => {
            return Err(invalid(format!(
                "tools[{i}].description must be 1 to {MAX_DESCRIPTION_BYTES} bytes with no control characters but LF and TAB"
            )))
        }
    };
    let input_schema = match tool.get("input_schema") {
        Some(s) if s.as_object().is_some_and(|o| o.get("type").and_then(Value::as_str) == Some("object")) => {
            s.clone()
        }
        _ => return Err(invalid(format!("tools[{i}].input_schema must be an object with \"type\":\"object\""))),
    };
    if serialized_len(&input_schema) > MAX_SCHEMA_BYTES {
        return Err(refused(
            Refusal::Limit,
            format!("tools[{i}].input_schema is over {MAX_SCHEMA_BYTES} bytes"),
        ));
    }
    let result_family = match tool.get("result_family").and_then(Value::as_str) {
        Some(f) if family_ok(f) => f.to_string(),
        _ => return Err(invalid(format!("tools[{i}].result_family must match ^[a-z][a-z0-9_.]{{0,47}}$"))),
    };
    let codes = tool.get("outcomes").and_then(Value::as_array);
    let mut outcomes: Vec<String> = Vec::new();
    let bad_outcomes = || {
        invalid(format!(
            "tools[{i}].outcomes must hold 1 to {MAX_OUTCOMES} unique codes matching ^[a-z][a-z0-9_]{{0,47}}$, none starting {RESERVED_OUTCOME_PREFIX}"
        ))
    };
    match codes {
        Some(codes) if !codes.is_empty() && codes.len() <= MAX_OUTCOMES => {
            for code in codes {
                match code.as_str() {
                    Some(c) if outcome_ok(c) && !outcomes.iter().any(|o| o == c) => outcomes.push(c.to_string()),
                    _ => return Err(bad_outcomes()),
                }
            }
        }
        _ => return Err(bad_outcomes()),
    }
    Ok(ToolDef { name, description, input_schema, result_family, outcomes })
}

/// What an accepted `register_client_tools` does.
#[derive(Debug, Clone, PartialEq)]
pub enum Merged {
    /// The same list as in force (`unchanged`).
    Unchanged,
    /// A first registration (new id) or a superset (same id): `applied`.
    Applied(Registration),
}

/// Add-only against the registration in force: the full list every time;
/// a changed field (or `require_memory_only`) is `def_changed`, a missing
/// tool `omitted`. Equality is JSON value equality (key order ignored).
pub fn merge(current: Option<&Registration>, parsed: Parsed) -> Result<Merged, Refused> {
    let Some(current) = current else {
        return Ok(Merged::Applied(Registration {
            registration_id: mint_registration_id(),
            extension_id: parsed.extension_id,
            require_memory_only: parsed.require_memory_only,
            tools: parsed.tools,
        }));
    };
    if parsed.extension_id != current.extension_id {
        return Err(refused(
            Refusal::ExtensionMismatch,
            format!("this registration's extension_id is {}", current.extension_id),
        ));
    }
    if parsed.require_memory_only != current.require_memory_only {
        return Err(refused(
            Refusal::DefChanged,
            "require_memory_only cannot change while registered".to_string(),
        ));
    }
    for held in &current.tools {
        match parsed.tools.iter().find(|t| t.name == held.name) {
            None => {
                return Err(refused(Refusal::Omitted, format!("tool {} is registered; send the full list", held.name)))
            }
            Some(sent) if sent != held => {
                return Err(refused(
                    Refusal::DefChanged,
                    format!("tool {} is registered with another definition", held.name),
                ))
            }
            Some(_) => {}
        }
    }
    if parsed.tools.len() == current.tools.len() {
        return Ok(Merged::Unchanged);
    }
    Ok(Merged::Applied(Registration {
        registration_id: current.registration_id.clone(),
        extension_id: parsed.extension_id,
        require_memory_only: parsed.require_memory_only,
        tools: parsed.tools,
    }))
}

/// A shape-checked `tool_return`.
pub struct Return {
    pub invocation_id: String,
    pub outcome: String,
    pub content: String,
    /// `_meta` serialized (0 when absent).
    pub meta_bytes: usize,
    /// `_meta["synaps/memory_only"] == true`.
    pub memory_only: bool,
}

impl fmt::Debug for Return {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Return")
            .field("invocation_id", &self.invocation_id)
            .field("outcome", &self.outcome)
            .field("memory_only", &self.memory_only)
            .finish_non_exhaustive()
    }
}

/// `ci_` + 32 lowercase hex, as the daemon mints them.
fn invocation_id_ok(s: &str) -> bool {
    s.strip_prefix("ci_").is_some_and(|hex| {
        hex.len() == 32 && hex.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}

/// The shape of a `tool_return` (`None` = `invalid`): `invocation_id` and
/// `outcome` follow their patterns (a reserved `synaps_` code is merely
/// unknown), `content` is a string, `_meta` an object or null. `is_error`
/// and unknown keys are ignored; so are `_meta` keys but
/// `synaps/memory_only`.
pub fn parse_return(fields: &Map<String, Value>) -> Option<Return> {
    let text = |key: &str| fields.get(key).and_then(Value::as_str).map(str::to_string);
    let (meta_bytes, memory_only) = match fields.get("_meta") {
        None | Some(Value::Null) => (0, false),
        Some(meta @ Value::Object(o)) => {
            (serialized_len(meta), o.get(crate::runtime::memory_only::META_KEY) == Some(&Value::Bool(true)))
        }
        Some(_) => return None,
    };
    Some(Return {
        invocation_id: text("invocation_id").filter(|id| invocation_id_ok(id))?,
        outcome: text("outcome").filter(|code| ident(code, 48, |c| c == b'_'))?,
        content: text("content")?,
        meta_bytes,
        memory_only,
    })
}

/// The `limits` every `client_tools` reply carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub max_tools: usize,
    pub max_pending: usize,
    pub timeout_s: u64,
    pub max_content_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_tools: MAX_TOOLS,
            max_pending: MAX_PENDING,
            timeout_s: TIMEOUT.as_secs(),
            max_content_bytes: MAX_CONTENT_BYTES,
        }
    }
}

/// One tool of the registration in force, as the reply echoes it.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct AckTool {
    pub name: String,
    pub runtime_name: String,
    pub wire_name: String,
    pub description: String,
    pub input_schema: Value,
    pub result_family: String,
    pub outcomes: Vec<String>,
    pub memory_only_marked: bool,
}

impl fmt::Debug for AckTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AckTool")
            .field("wire_name", &self.wire_name)
            .field("result_family", &self.result_family)
            .field("outcomes", &self.outcomes)
            .field("memory_only_marked", &self.memory_only_marked)
            .finish_non_exhaustive()
    }
}

/// `client_tools`: the reply to `register_client_tools` (`id` echoed) or
/// an unsolicited notice (`id: null`), addressed to one client. Always
/// echoes the registration in force (none: `registration_id: null`,
/// `tools: []`).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientToolsAck {
    pub id: Option<u64>,
    /// `applied|unchanged|refused|dropped`.
    pub outcome: String,
    pub code: Option<String>,
    pub message: Option<String>,
    pub contract: u64,
    pub registration_id: Option<String>,
    pub client: Option<ClientId>,
    pub daemon_pid: u32,
    pub daemon_generation: u64,
    pub conversation_id: Option<String>,
    pub forked_from: Option<agent_core::session::ForkedFrom>,
    pub extension_id: Option<String>,
    pub require_memory_only: bool,
    pub tools: Vec<AckTool>,
    pub pending: usize,
    pub limits: Limits,
}

impl fmt::Debug for ClientToolsAck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientToolsAck")
            .field("id", &self.id)
            .field("outcome", &self.outcome)
            .field("code", &self.code)
            .field("registration_id", &self.registration_id)
            .field("client", &self.client)
            .field("tools", &self.tools)
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}

/// The session facts every `client_tools` reply echoes.
#[derive(Debug, Clone, Default)]
pub struct Evidence {
    pub client: Option<ClientId>,
    pub conversation_id: Option<String>,
    pub forked_from: Option<agent_core::session::ForkedFrom>,
    pub pending: usize,
}

impl ClientToolsAck {
    /// A reply echoing `registration` (`None`: nothing in force). `marked`
    /// says whether the session's memory-only policy marks a tool now.
    pub fn echo(
        id: Option<u64>,
        outcome: &str,
        refusal: Option<(&str, String)>,
        registration: Option<&Registration>,
        evidence: Evidence,
        marked: impl Fn(&str, &str, &str) -> bool,
    ) -> Self {
        use crate::runtime::client_tools::{runtime_name, wire_name};
        let (code, message) = refusal.map_or((None, None), |(c, m)| (Some(c.to_string()), Some(m)));
        let tools = registration.map_or_else(Vec::new, |r| {
            r.tools
                .iter()
                .map(|t| {
                    let runtime = runtime_name(&r.extension_id, &t.name);
                    let wire = wire_name(&r.extension_id, &t.name);
                    AckTool {
                        memory_only_marked: marked(&r.extension_id, &runtime, &wire),
                        name: t.name.clone(),
                        runtime_name: runtime,
                        wire_name: wire,
                        description: t.description.clone(),
                        input_schema: t.input_schema.clone(),
                        result_family: t.result_family.clone(),
                        outcomes: t.outcomes.clone(),
                    }
                })
                .collect()
        });
        Self {
            id,
            outcome: outcome.to_string(),
            code,
            message,
            contract: CONTRACT,
            registration_id: registration.map(|r| r.registration_id.clone()),
            client: evidence.client,
            daemon_pid: std::process::id(),
            daemon_generation: daemon_generation(),
            conversation_id: evidence.conversation_id,
            forked_from: evidence.forked_from,
            extension_id: registration.map(|r| r.extension_id.clone()),
            require_memory_only: registration.is_some_and(|r| r.require_memory_only),
            tools,
            pending: evidence.pending,
            limits: Limits::default(),
        }
    }
}

/// `tool_invoke`: one call, for the installer only. Never replayed.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolInvokeFrame {
    pub invocation_id: String,
    pub registration_id: String,
    pub daemon_pid: u32,
    pub daemon_generation: u64,
    pub conversation_id: String,
    /// The calling turn; never empty (a call without one is refused before
    /// any frame).
    pub turn_correlation_id: String,
    pub tool_use_id: String,
    pub tool: String,
    pub wire_name: String,
    pub args: Value,
    pub deadline_ms: u64,
}

impl fmt::Debug for ToolInvokeFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolInvokeFrame")
            .field("invocation_id", &self.invocation_id)
            .field("registration_id", &self.registration_id)
            .field("tool_use_id", &self.tool_use_id)
            .field("wire_name", &self.wire_name)
            .finish_non_exhaustive()
    }
}

impl From<crate::runtime::client_tools::Invoke> for ToolInvokeFrame {
    fn from(invoke: crate::runtime::client_tools::Invoke) -> Self {
        Self {
            invocation_id: invoke.invocation_id,
            registration_id: invoke.registration_id,
            daemon_pid: std::process::id(),
            daemon_generation: daemon_generation(),
            conversation_id: invoke.conversation_id,
            turn_correlation_id: invoke.turn_correlation_id,
            tool_use_id: invoke.tool_use_id,
            tool: invoke.tool,
            wire_name: invoke.wire_name,
            args: invoke.args,
            deadline_ms: invoke.deadline_ms,
        }
    }
}

/// `tool_cancel`: a pending call ended without its return.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCancelFrame {
    pub invocation_id: String,
    pub registration_id: String,
    /// `turn_cancel|timeout|reload|owner_change|new_conversation`.
    pub reason: String,
}

impl From<crate::runtime::client_tools::Cancel> for ToolCancelFrame {
    fn from(cancel: crate::runtime::client_tools::Cancel) -> Self {
        Self {
            invocation_id: cancel.invocation_id,
            registration_id: cancel.registration_id,
            reason: cancel.reason.as_str().to_string(),
        }
    }
}

/// `tool_return_ack`: the reply to `tool_return`, to its sender.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolReturnAckFrame {
    pub id: u64,
    pub invocation_id: Option<String>,
    /// `accepted|refused`.
    pub outcome: String,
    /// `invalid|not_installer|not_pending|unknown_outcome|too_large`.
    pub code: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    fn tool(name: &str) -> Value {
        json!({"name": name, "description": "Fetch one item.\n\tTabbed.",
               "input_schema": {"type": "object", "properties": {"item": {"type": "string"}}},
               "result_family": "pria.vault.v1", "outcomes": ["ok", "not_found"]})
    }

    fn request(tools: Vec<Value>) -> Map<String, Value> {
        fields(json!({"contract": 1, "daemon_generation": 1, "extension_id": "pria-desktop",
                      "require_memory_only": true, "tools": tools}))
    }

    fn code(r: Result<Parsed, Refused>) -> &'static str {
        r.err().map_or("ok", |e| e.refusal.code())
    }

    #[test]
    fn identifiers_follow_the_contract_patterns() {
        assert!(extension_id_ok("pria-desktop") && extension_id_ok(&"a".repeat(32)));
        assert!(!extension_id_ok("Pria") && !extension_id_ok("1x") && !extension_id_ok(&"a".repeat(33)));
        assert!(!extension_id_ok("pria_desktop") && !extension_id_ok(""));
        assert!(tool_name_ok("pria_vault_get") && tool_name_ok(&"a".repeat(64)) && !tool_name_ok(&"a".repeat(65)));
        assert!(!tool_name_ok("pria-vault") && !tool_name_ok("_x"));
        assert!(family_ok("pria.vault.v1") && !family_ok("pria-vault"));
        assert!(outcome_ok("refused_no_grant") && !outcome_ok("synaps_timeout") && !outcome_ok("ok.x"));
    }

    #[test]
    fn register_fields_are_checked_in_shape_and_size() {
        let ok = parse_register(&request(vec![tool("pria_vault_get")])).unwrap();
        assert_eq!(ok.extension_id, "pria-desktop");
        assert!(ok.require_memory_only);
        assert_eq!(ok.tools[0].outcomes, vec!["ok", "not_found"]);
        assert_eq!(code(parse_register(&request(vec![]))), "limit");
        assert_eq!(code(parse_register(&request((0..33).map(|i| tool(&format!("t{i}"))).collect()))), "limit");
        assert_eq!(code(parse_register(&request(vec![tool("a"), tool("a")]))), "invalid");
        let mut big = tool("a");
        big["input_schema"]["description"] = json!("x".repeat(MAX_SCHEMA_BYTES));
        assert_eq!(code(parse_register(&request(vec![big]))), "limit");
        for (key, bad) in [
            ("description", json!("")),
            ("description", json!("bell\u{7}")),
            ("description", json!("x".repeat(MAX_DESCRIPTION_BYTES + 1))),
            ("input_schema", json!({"type": "string"})),
            ("result_family", json!("Pria")),
            ("outcomes", json!([])),
            ("outcomes", json!(["ok", "ok"])),
            ("outcomes", json!(["synaps_busy"])),
        ] {
            let mut t = tool("a");
            t[key] = bad;
            assert_eq!(code(parse_register(&request(vec![t]))), "invalid", "{key}");
        }
        let mut req = request(vec![tool("a")]);
        req.insert("require_memory_only".into(), json!("yes"));
        assert_eq!(code(parse_register(&req)), "invalid");
        req.insert("require_memory_only".into(), Value::Null);
        req.insert("later_key".into(), json!(1));
        assert!(!parse_register(&req).unwrap().require_memory_only);
        assert_eq!(check_contract(&fields(json!({"contract": 2}))).unwrap_err().refusal.code(), "contract_unsupported");
        assert!(check_contract(&fields(json!({"contract": 1}))).is_ok());
        assert_eq!(check_generation(&fields(json!({}))).unwrap_err().refusal.code(), "wrong_generation");
    }

    /// Only the session's own root conversation registers: a worker (a
    /// delegation parent) is refused like a fork child.
    #[test]
    fn only_a_root_session_registers() {
        let code = |r: Result<(), Refused>| r.err().map_or("ok", |e| e.refusal.code());
        assert_eq!(code(check_root(Some("conv"), false, None)), "ok");
        assert_eq!(code(check_root(None, false, None)), "no_conversation");
        assert_eq!(code(check_root(Some("conv"), true, None)), "not_root_session");
        assert_eq!(code(check_root(Some("conv"), false, Some("sa_1"))), "not_root_session");
    }

    #[test]
    fn a_registration_is_add_only() {
        let first = parse_register(&request(vec![tool("a")])).unwrap();
        let Ok(Merged::Applied(reg)) = merge(None, first.clone()) else { panic!("applied") };
        assert!(reg.registration_id.starts_with("cr_"));
        assert_eq!(merge(Some(&reg), first.clone()), Ok(Merged::Unchanged));
        let wider = parse_register(&request(vec![tool("b"), tool("a")])).unwrap();
        let Ok(Merged::Applied(wide)) = merge(Some(&reg), wider) else { panic!("superset") };
        assert_eq!(wide.registration_id, reg.registration_id);
        assert_eq!(wide.tools.len(), 2);
        let code = |r: Result<Merged, Refused>| r.unwrap_err().refusal.code();
        assert_eq!(code(merge(Some(&wide), first.clone())), "omitted");
        let mut changed = tool("a");
        changed["outcomes"] = json!(["ok"]);
        let changed = parse_register(&request(vec![changed, tool("b")])).unwrap();
        assert_eq!(code(merge(Some(&wide), changed)), "def_changed");
        let mut other = first.clone();
        other.extension_id = "other".into();
        assert_eq!(code(merge(Some(&reg), other)), "extension_mismatch");
        let mut lax = first;
        lax.require_memory_only = false;
        assert_eq!(code(merge(Some(&reg), lax)), "def_changed");
    }

    #[test]
    fn a_return_is_shape_checked_and_its_meta_measured() {
        const CI: &str = "ci_0123456789abcdef0123456789abcdef";
        for bad in ["ci_1", "ci_0123456789ABCDEF0123456789abcdef", "cr_0123456789abcdef0123456789abcdef"] {
            assert!(parse_return(&fields(json!({"invocation_id": bad, "outcome": "ok", "content": ""}))).is_none());
        }
        for bad in ["", "Ok", "1ok", "ok-1"] {
            assert!(parse_return(&fields(json!({"invocation_id": CI, "outcome": bad, "content": ""}))).is_none());
        }
        let reserved = parse_return(&fields(json!({"invocation_id": CI, "outcome": "synaps_timeout", "content": ""})));
        assert!(reserved.is_some(), "well formed; unknown_outcome decides it");
        let ret = parse_return(&fields(json!({"invocation_id": CI, "outcome": "ok", "content": "",
            "is_error": true, "_meta": {"synaps/memory_only": true, "later": 1}})))
        .unwrap();
        assert!(ret.memory_only);
        assert_eq!(ret.meta_bytes, r#"{"later":1,"synaps/memory_only":true}"#.len());
        assert!(!format!("{ret:?}").contains("content:"));
        let plain = parse_return(&fields(json!({"invocation_id": CI, "outcome": "ok", "content": "x",
            "_meta": null}))).unwrap();
        assert_eq!((plain.meta_bytes, plain.memory_only), (0, false));
        assert!(parse_return(&fields(json!({"invocation_id": CI, "outcome": "ok"}))).is_none());
        assert!(parse_return(&fields(json!({"invocation_id": CI, "outcome": "ok", "content": "x",
            "_meta": "x"}))).is_none());
        assert!(parse_return(&fields(json!({"invocation_id": 1, "outcome": "ok", "content": "x"}))).is_none());
    }
}
