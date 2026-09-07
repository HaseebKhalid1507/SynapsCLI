//! Generic, explicitly user-authorized foreground session driving.
//!
//! This module grants no authority by itself: the local frontend must obtain a
//! permission-gated eager handler, bind it to an explicit user action/session,
//! and recheck its identity before polling or committing a prepared proposal.
//! Timers, terminal boundaries, cancellation and user-input arbitration belong
//! to that frontend. Loop policy and prompts belong exclusively to the plugin.

use std::collections::HashSet;
use std::io::{self, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_core::reasoning::ReasoningLevel;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::commands::CommandOutputEvent;
use super::invoke_output::{invoke_event_channel, InvokeOutputBudget};
use super::runtime::{ExtensionHandler, InvokeCommandEvent};
use crate::{Runtime, SharedMessage};

const MAX_REPLY_BYTES: usize = 64 * 1024;
const MAX_PROMPT_BYTES: usize = 16 * 1024;
const MAX_NOTICE_BYTES: usize = 2 * 1024;
const MAX_ID_BYTES: usize = 128;
const MAX_MODEL_BYTES: usize = 512;
const MAX_EFFORT_BYTES: usize = 16;
const MAX_MODELS: usize = 16;
const MIN_DELAY_MS: u64 = 1_000;
const MAX_DELAY_MS: u64 = 300_000;
const MAX_DURATION_MS: u64 = 365 * 24 * 60 * 60 * 1_000;
const POLL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    pub model: String,
    pub effort: String,
}

/// Optional session-local context policy requested by an explicit start only.
/// Absent on older drivers means leave the current session preference unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextMode {
    Auto,
    Off,
}
impl ContextMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Off => "off",
        }
    }
}

// Missing is compatible; an explicitly null/unknown/mistyped mode is not.
fn deserialize_context_mode<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<ContextMode>, D::Error> {
    ContextMode::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Start {
        run_id: String,
        models: Vec<Selection>,
        prompt: String,
        delay_ms: u64,
        #[serde(default)]
        max_duration_ms: Option<u64>,
        /// Opt into bounded, content-free completed-turn feedback.
        #[serde(default)]
        feedback_version: Option<u8>,
        /// Explicit support for typed wall-clock continuation checkpoints.
        #[serde(default)]
        time_checkpoint_version: Option<u8>,
        #[serde(
            default,
            deserialize_with = "deserialize_context_mode",
            skip_serializing_if = "Option::is_none"
        )]
        context_mode: Option<ContextMode>,
        #[serde(default)]
        notice: String,
    },
    Next {
        run_id: String,
        selection: Selection,
        prompt: String,
        delay_ms: u64,
        #[serde(default)]
        notice: String,
    },
    Stop {
        #[serde(default)]
        notice: String,
    },
    Status {
        #[serde(default)]
        notice: String,
    },
}

/// Count serialized bytes without allocating an unbounded serialization buffer.
struct SizeLimit(usize);
impl Write for SizeLimit {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.checked_sub(bytes.len()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "session driver reply too large")
        })?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn bounded_serialization(value: &impl Serialize) -> Result<(), String> {
    serde_json::to_writer(SizeLimit(MAX_REPLY_BYTES), value)
        .map_err(|_| "session driver reply exceeds 64 KiB".into())
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_BYTES
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn exact_effort(effort: &str) -> Result<ReasoningLevel, String> {
    ReasoningLevel::parse(effort)
        .filter(|level| effort.len() <= MAX_EFFORT_BYTES && level.as_str() == effort)
        .ok_or_else(|| "session driver effort must be an exact canonical reasoning name".into())
}

fn validate_selection(selection: &Selection) -> Result<ReasoningLevel, String> {
    if selection.model.len() > MAX_MODEL_BYTES
        || agent_core::prompt::QualifiedModelId::parse(selection.model.clone()).is_err()
    {
        return Err("session driver model must be a bounded exact provider/model identity".into());
    }
    exact_effort(&selection.effort)
}

fn validate_text(prompt: &str, notice: &str, delay_ms: u64) -> Result<(), String> {
    if prompt.trim().is_empty() || prompt.len() > MAX_PROMPT_BYTES {
        return Err("session driver prompt must be nonempty and at most 16 KiB".into());
    }
    validate_notice(notice)?;
    if !(MIN_DELAY_MS..=MAX_DELAY_MS).contains(&delay_ms) {
        return Err("session driver delay must be 1000..300000 ms".into());
    }
    Ok(())
}

fn validate_notice(notice: &str) -> Result<(), String> {
    if notice.len() > MAX_NOTICE_BYTES {
        return Err("session driver notice exceeds 2 KiB".into());
    }
    Ok(())
}

fn validate_reply(reply: &Reply) -> Result<(), String> {
    // Public enum constructors and serde consumers cannot bypass grant bounds.
    bounded_serialization(reply)?;
    match reply {
        Reply::Start {
            run_id,
            models,
            prompt,
            delay_ms,
            max_duration_ms,
            feedback_version,
            time_checkpoint_version,
            context_mode: _,
            notice,
        } => {
            if !valid_id(run_id) {
                return Err("session driver run_id must be 1..128 ASCII letters/digits/-/_".into());
            }
            if models.is_empty() || models.len() > MAX_MODELS {
                return Err("session driver requires 1..16 model selections".into());
            }
            let mut unique = HashSet::new();
            for selection in models {
                validate_selection(selection)?;
                if !unique.insert(&selection.model) {
                    return Err("session driver model identities must be unique".into());
                }
            }
            if max_duration_ms.is_some_and(|duration| duration == 0 || duration > MAX_DURATION_MS) {
                return Err("session driver duration must be positive and at most 365 days".into());
            }
            if feedback_version.is_some_and(|v| v != 1) {
                return Err("unsupported session driver feedback version".into());
            }
            if time_checkpoint_version.is_some_and(|v| v != 1) {
                return Err("unsupported session driver time checkpoint version".into());
            }
            validate_text(prompt, notice, *delay_ms)
        }
        Reply::Next {
            run_id,
            selection,
            prompt,
            delay_ms,
            notice,
        } => {
            if !valid_id(run_id) {
                return Err("session driver run_id must be 1..128 ASCII letters/digits/-/_".into());
            }
            validate_selection(selection)?;
            validate_text(prompt, notice, *delay_ms)
        }
        Reply::Stop { notice } | Reply::Status { notice } => validate_notice(notice),
    }
}

/// Parse only the structured top-level response, never printed output.
/// Unknown fields inside the driver object/selections fail closed. Unrelated
/// command response fields are allowed, but count toward the 64 KiB bound.
/// Optional start `selection` is redundant and must exactly equal models[0].
/// It is removed here so the coordinated public Start enum has no extra field.
pub fn parse_reply(value: &Value) -> Result<Option<Reply>, String> {
    let Some(driver) = value.get("session_driver") else {
        return Ok(None);
    };
    bounded_serialization(value)?;
    let mut driver = driver.clone();
    if driver.get("action").and_then(Value::as_str) == Some("start") {
        if let Some(selection) = driver
            .as_object_mut()
            .and_then(|map| map.remove("selection"))
        {
            let selection: Selection = serde_json::from_value(selection)
                .map_err(|_| "invalid session driver start selection".to_string())?;
            let first: Selection = serde_json::from_value(
                driver
                    .get("models")
                    .and_then(Value::as_array)
                    .and_then(|m| m.first())
                    .cloned()
                    .ok_or_else(|| "session driver start has no initial model".to_string())?,
            )
            .map_err(|_| "invalid session driver initial selection".to_string())?;
            if selection != first {
                return Err("session driver start selection must equal models[0]".into());
            }
        }
    }
    let reply: Reply = serde_json::from_value(driver).map_err(|_| {
        "malformed session driver response (unknown field, action or type)".to_string()
    })?;
    validate_reply(&reply)?;
    Ok(Some(reply))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    pub selection: Selection,
    pub prompt: String,
    pub delay: Duration,
    pub notice: String,
}

/// An ephemeral allowlist and deadline, never persisted. Public identity fields
/// are host bookkeeping, not values the plugin may update via subsequent replies.
#[derive(Debug)]
pub struct Grant {
    pub plugin_id: String,
    pub session_id: String,
    pub run_id: String,
    models: Vec<Selection>,
    // Retain the authoritative ID privately: changing a public bookkeeping
    // field must not silently authorize a different poll run.
    pinned_run_id: String,
    feedback_version: Option<u8>,
    time_checkpoint_version: Option<u8>,
    context_mode: Option<ContextMode>,
    deadline: Option<Instant>,
}

impl Grant {
    /// The caller MUST establish explicit user intent and permission first.
    pub fn from_start(
        plugin_id: &str,
        session_id: &str,
        reply: Reply,
    ) -> Result<(Self, Proposal), String> {
        validate_reply(&reply)?;
        if plugin_id.is_empty() || session_id.is_empty() {
            return Err("session driver requires an owning plugin and session".into());
        }
        let Reply::Start {
            run_id,
            models,
            prompt,
            delay_ms,
            max_duration_ms,
            feedback_version,
            time_checkpoint_version,
            context_mode,
            notice,
        } = reply
        else {
            return Err("session driver can only be armed by start".into());
        };
        let deadline = max_duration_ms
            .map(|ms| {
                Instant::now()
                    .checked_add(Duration::from_millis(ms))
                    .ok_or_else(|| "session driver deadline is not representable".to_string())
            })
            .transpose()?;
        let proposal = Proposal {
            selection: models[0].clone(),
            prompt,
            delay: Duration::from_millis(delay_ms),
            notice,
        };
        let grant = Self {
            plugin_id: plugin_id.into(),
            session_id: session_id.into(),
            pinned_run_id: run_id.clone(),
            feedback_version,
            time_checkpoint_version,
            context_mode,
            run_id,
            models,
            deadline,
        };
        grant.check_delay(proposal.delay)?;
        Ok((grant, proposal))
    }

    /// Apply only after the frontend accepted an explicit start and all grant
    /// checks passed. Uses exactly the ordinary /context command's validation,
    /// not a raw config mutation or permission/recall bypass.
    pub fn apply_context_mode(&self, runtime: &Runtime) -> Result<Option<String>, String> {
        self.context_mode
            .map(|mode| runtime.context_management_command(mode.as_str()))
            .transpose()
    }

    pub fn feedback_enabled(&self) -> bool {
        self.feedback_version == Some(1)
    }

    pub fn time_checkpoints_enabled(&self) -> bool {
        self.time_checkpoint_version == Some(1)
    }

    pub fn expired(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    fn check_delay(&self, delay: Duration) -> Result<(), String> {
        if self.deadline.is_some_and(|deadline| {
            Instant::now()
                .checked_add(delay)
                .map_or(true, |at| at >= deadline)
        }) {
            return Err("session driver deadline expires before the proposed turn".into());
        }
        Ok(())
    }

    pub fn accept(&self, reply: Reply) -> Result<Option<Proposal>, String> {
        validate_reply(&reply)?;
        if self.expired() {
            return Err("session driver deadline expired".into());
        }
        match reply {
            Reply::Stop { .. } => Ok(None),
            Reply::Next {
                run_id,
                selection,
                prompt,
                delay_ms,
                notice,
            } => {
                if run_id != self.pinned_run_id {
                    return Err("session driver response has a stale or incorrect run_id".into());
                }
                if !self.models.contains(&selection) {
                    return Err(
                        "session driver selection is outside the original exact allowlist".into(),
                    );
                }
                let delay = Duration::from_millis(delay_ms);
                self.check_delay(delay)?;
                Ok(Some(Proposal {
                    selection,
                    prompt,
                    delay,
                    notice,
                }))
            }
            Reply::Start { .. } | Reply::Status { .. } => {
                Err("session driver polls accept only next or stop".into())
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Success,
    ProviderError,
    SelectionRejected,
    TimeCheckpoint,
    Blocked,
}

/// Coarse host metadata only. `decision_id` is stable across transport retries;
/// the independent command request_id is always fresh. Neither contains secrets.
#[derive(Debug, Clone, Serialize)]
pub struct PollRequest {
    pub run_id: String,
    pub decision_id: String,
    pub outcome: Outcome,
    pub error_kind: String,
    pub model: String,
    pub effort: String,
    /// Only emitted for a grant explicitly opting into feedback v1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback: Option<String>,
}

fn blocked() -> (Outcome, String) {
    (Outcome::Blocked, "unknown".into())
}
fn provider_error(kind: &str) -> (Outcome, String) {
    (Outcome::ProviderError, kind.into())
}

/// Typed provenance is mandatory at the frontend boundary. Even `ProviderFailed`
/// includes local config/session/tool failures: these are NOT failover signals.
/// Cancellation is Blocked here; the frontend must revoke without polling.
pub fn classify_turn_error(error: &agent_core::TurnError) -> (Outcome, String) {
    if matches!(
        error.outcome,
        agent_core::TurnOutcome::BudgetExceeded {
            dimension: agent_core::BudgetDimension::WallClock
        }
    ) {
        return (Outcome::TimeCheckpoint, "wall_clock".into());
    }
    let agent_core::TurnOutcome::ProviderFailed { code, .. } = &error.outcome else {
        return blocked();
    };
    match code.as_str() {
        // Auth also wraps broker machine authorization/storage/transport errors.
        // Only exact host missing-account templates establish provider provenance.
        "auth_error" if missing_anthropic_account(&error.message) => provider_error("auth"),
        "network_error" | "timeout" => provider_error("transient"),
        "api_status" => classify_error(&error.message),
        // Narrow exception for the broker's exact NotConfigured template. No
        // generic Config error, policy denial or freeform credential text is
        // promoted. Provider keys are verified against host routing metadata.
        "config_error" if missing_account(&error.message) => provider_error("auth"),
        _ => blocked(),
    }
}

fn missing_anthropic_account(message: &str) -> bool {
    let message = message.strip_prefix("Auth error: ").unwrap_or(message);
    matches!(message,
        "No Anthropic credentials. Run `synaps login`, or switch to a provider model with `/model groq/llama-3.3-70b-versatile`."
        | "No API key or OAuth token found. Run `synaps login` to authenticate."
    ) || message == "Token refresh failed: no credential configured for 'anthropic'. Run `synaps login` to add one.. Run `synaps login` to re-authenticate, or check auth.remote_endpoint / broker reachability."
}

fn known_provider(provider: &str) -> bool {
    matches!(
        provider,
        "anthropic"
            | "openai-codex"
            | "xai-auth"
            | "kimi-code"
            | "github-copilot"
            | "google-gemini"
            | "local"
    ) || crate::runtime::openai::registry::providers()
        .iter()
        .any(|p| p.key == provider)
}

fn missing_account(message: &str) -> bool {
    let message = message.strip_prefix("Config error: ").unwrap_or(message);
    let Some(message) = message.strip_prefix("openai provider: ") else {
        return false;
    };
    let message = message
        .strip_prefix("openai request failed: ")
        .or_else(|| message.strip_prefix("codex request failed: "))
        .unwrap_or(message);
    missing_account_template(message)
}

fn missing_account_template(message: &str) -> bool {
    let Some(provider) = message
        .strip_prefix("no credential configured for '")
        .and_then(|rest| rest.strip_suffix("'. Run `synaps login` to add one."))
    else {
        return false;
    };
    known_provider(provider)
}

fn status_kind(status: u16) -> (Outcome, String) {
    match status {
        401 => provider_error("auth"),
        402 => provider_error("quota"),
        429 => provider_error("rate_limit"),
        408 | 500 | 502 | 503 | 504 | 529 => provider_error("transient"),
        // Generic 403 can be a model/permission/policy restriction. No failover.
        _ => blocked(),
    }
}

/// Recognize only anchored host-generated API status forms. Never search
/// arbitrary error/transcript text for "auth", "429", "quota" etc. Raw strings
/// are not copied into the return value. Prefer `classify_turn_error` externally.
pub fn classify_error(error: &str) -> (Outcome, String) {
    if error.strip_prefix("API error: ").unwrap_or(error)
        == crate::runtime::openai::stream::STREAM_INTERRUPTED
    {
        return provider_error("transient");
    }

    let error = error.strip_prefix("API error: ").unwrap_or(error);
    // Some provider routes wrap broker NotConfigured in ApiStatus instead.
    if ["openai request failed: ", "codex request failed: "]
        .iter()
        .any(|prefix| {
            error
                .strip_prefix(prefix)
                .is_some_and(missing_account_template)
        })
    {
        return provider_error("auth");
    }
    for prefix in [
        "codex request failed: ",
        "openai request failed: ",
        "provider request failed: ",
    ] {
        if let Some(rest) = error.strip_prefix(prefix) {
            // Broker errors have a fixed nesting prefix; no substring search.
            let rest = rest
                .strip_prefix("broker transport error: ")
                .unwrap_or(rest);
            let rest = rest
                .strip_prefix("provider request failed: ")
                .unwrap_or(rest);
            let bytes = rest.as_bytes();
            if bytes.len() >= 3
                && bytes[..3].iter().all(u8::is_ascii_digit)
                && (bytes.len() == 3 || matches!(bytes[3], b' ' | b':' | b'('))
            {
                return status_kind(rest[..3].parse().unwrap_or(0));
            }
            return blocked();
        }
    }
    // Exact host-authored Anthropic SSE templates survive as api_status after
    // provider retries. Never match provider free-text or policy refusals.
    if let Some(kind) = error.strip_prefix("API stream error (").and_then(|s| {
        s.strip_suffix("). Provider error details withheld — they can echo request content.")
    }) {
        return match kind {
            "authentication_error" => provider_error("auth"),
            "billing_error" => provider_error("quota"),
            "rate_limit_error" => provider_error("rate_limit"),
            "overloaded_error" | "api_error" | "timeout_error" => provider_error("transient"),
            _ => blocked(),
        };
    }
    if matches!(error,
        "Request to api.anthropic.com timed out. Check your connection and try again."
        | "Could not reach api.anthropic.com (connection failed). Check your network, DNS, or proxy settings."
        | "Connection lost mid-response. Partial reply kept — send again to continue."
    ) { return provider_error("transient"); }
    // These messages are authored by core/error.rs, not remote response bodies.
    if error == "Authentication rejected. Run `synaps login` to re-authenticate." {
        return provider_error("auth");
    }
    if status_template(error, "Rate limited by Anthropic (HTTP 429")
        || error.starts_with(
            "Rate limit exhausted — retries used up while waiting for reset (next window in ",
        )
    {
        return provider_error("rate_limit");
    }
    if error
        == "Anthropic is overloaded right now. Retries exhausted — wait a minute and try again."
    {
        return provider_error("transient");
    }
    for status in [500, 502, 503] {
        if status_template(error, &format!("Anthropic server error (HTTP {status}")) {
            return provider_error("transient");
        }
    }
    for label in ["Codex", "OpenAI", "xAI"] {
        use crate::runtime::openai::stream::{
            RESPONSES_AUTH_SUFFIX, RESPONSES_CAPACITY_SUFFIX, RESPONSES_EMPTY_SUFFIX,
            RESPONSES_MISSING_TERMINAL_SUFFIX, RESPONSES_QUOTA_SUFFIX,
        };
        for (suffix, kind) in [
            (RESPONSES_AUTH_SUFFIX, "auth"),
            (RESPONSES_QUOTA_SUFFIX, "quota"),
            (RESPONSES_CAPACITY_SUFFIX, "transient"),
            (RESPONSES_EMPTY_SUFFIX, "transient"),
            (RESPONSES_MISSING_TERMINAL_SUFFIX, "transient"),
        ] {
            if error == format!("{label}{suffix}") {
                return provider_error(kind);
            }
        }
    }
    // Host-authored network templates have a single bounded hostname slot.
    // Do not inspect the bracketed underlying error chain (it may be secret).
    for (prefix, suffix) in [
        (
            "Request to ",
            " timed out — usually transient; check your connection and try again. [",
        ),
        (
            "Could not reach ",
            " (connection failed). Check your network, DNS, or proxy settings. [",
        ),
        (
            "Connection to ",
            " lost mid-response — usually transient; try again. [",
        ),
    ] {
        if let Some(rest) = error.strip_prefix(prefix) {
            if let Some((host, _)) = rest.split_once(suffix) {
                if valid_network_host(host) && error.ends_with(']') {
                    return provider_error("transient");
                }
            }
        }
    }
    if let Some((provider, rest)) = error.split_once(": ") {
        if known_provider(provider) {
            if rest.starts_with("usage quota exhausted (HTTP 403). ")
                || rest.starts_with("usage quota exhausted (HTTP 429). ")
            {
                return provider_error("quota");
            }
            if rest.starts_with("authentication rejected (HTTP 401). ") {
                return provider_error("auth");
            }
            if rest.starts_with("rate limited (HTTP 429). ") {
                return provider_error("rate_limit");
            }
        }
    }
    blocked()
}

fn status_template(error: &str, prefix: &str) -> bool {
    error
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.starts_with("). ") || rest.starts_with(" ["))
}

fn valid_network_host(host: &str) -> bool {
    host == "the provider endpoint"
        || (!host.is_empty()
            && host.len() <= 253
            && host.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']')
            }))
}

/// Bounded asynchronous poll using the existing command.invoke frame contract.
/// No spawned task or manager lock is retained here: dropping this future drops
/// both invocation and collector. The frontend owns cancellation/generation.
pub async fn poll(
    handler: Arc<dyn ExtensionHandler>,
    request: PollRequest,
) -> Result<Reply, String> {
    if !valid_id(&request.run_id) || !valid_id(&request.decision_id) {
        return Err("session driver poll identifiers are invalid".into());
    }
    validate_selection(&Selection {
        model: request.model.clone(),
        effort: request.effort.clone(),
    })?;
    if !matches!(
        request.error_kind.as_str(),
        "none" | "auth" | "quota" | "rate_limit" | "transient" | "wall_clock" | "unknown"
    ) {
        return Err("session driver poll requires a coarse error kind".into());
    }
    if (request.outcome == Outcome::TimeCheckpoint) != (request.error_kind == "wall_clock") {
        return Err("inconsistent time checkpoint metadata".into());
    }
    if request.feedback.as_deref().is_some_and(|f| {
        !matches!(f, "unknown" | "changed" | "repeated" | "empty")
            || (request.outcome != Outcome::Success && f != "unknown")
    }) {
        return Err("invalid session driver feedback".into());
    }
    let args = vec![serde_json::to_string(&request)
        .map_err(|_| "invalid session driver poll request".to_string())?];
    let request_id = uuid::Uuid::new_v4().to_string();
    let result = tokio::time::timeout(POLL_TIMEOUT, async {
        let (sink, collector) = invoke_event_channel(InvokeOutputBudget::default());
        tokio::join!(
            handler.invoke_command("__session_driver__", args, &request_id, sink),
            collector.collect()
        )
    })
    .await
    .map_err(|_| "session driver poll timed out".to_string())?;
    let (value, report) = result;
    // Display output cannot drive a session. Errors and overflow stop visibly,
    // even if the final structured response otherwise proposes a valid next.
    if report.is_limited() {
        return Err("session driver poll output exceeded its bounded frame budget".into());
    }
    if report.events.iter().any(|event| {
        matches!(
            event,
            InvokeCommandEvent::Output(CommandOutputEvent::Error { .. })
        )
    }) {
        return Err("session driver plugin reported an error".into());
    }
    let value = value.map_err(|_| "session driver plugin invocation failed".to_string())?;
    let reply = parse_reply(&value)?
        .ok_or_else(|| "session driver poll omitted its structured response".to_string())?;
    match reply {
        Reply::Next { .. } | Reply::Stop { .. } => Ok(reply),
        _ => Err("session driver polls accept only next or stop".into()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareError {
    Selection(String),
    Blocked(String),
}
impl std::fmt::Display for PrepareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Selection(error) | Self::Blocked(error) => f.write_str(error),
        }
    }
}
impl std::error::Error for PrepareError {}

/// Atomic session-only model/effort preparation. `history` is the EXISTING
/// history: this function adds the proposed user message for full preflight.
/// The caller commits returned Runtime plus its own session mirrors only on
/// success, after rechecking cancellation/deadline/handler/session identity.
/// No config writes, credential refresh, provider calls, or history mutation.
pub async fn prepare(
    runtime: &Runtime,
    proposal: &Proposal,
    history: &[SharedMessage],
) -> Result<Runtime, PrepareError> {
    use agent_core::orchestration::CompletionGate;
    if let Some(policy) = runtime.orchestration() {
        if matches!(policy.completion_gate(), CompletionGate::Blocked { .. }) {
            return Err(PrepareError::Blocked(
                "session driver blocked: workers require collection/reconciliation".into(),
            ));
        }
        if proposal.selection.model != runtime.model()
            && !policy.unreconciled_runtime_handles().is_empty()
        {
            return Err(PrepareError::Blocked(
                "session driver cannot change model while workers are outstanding".into(),
            ));
        }
    }
    if let Some(prompt) = runtime.effective_prompt() {
        if prompt.provenance("").foreground_model != proposal.selection.model {
            return Err(PrepareError::Blocked(
                "session driver cannot change a typed prompt's pinned foreground model".into(),
            ));
        }
    }
    if proposal.prompt.trim().is_empty() || proposal.prompt.len() > MAX_PROMPT_BYTES {
        return Err(PrepareError::Blocked(
            "invalid session driver proposed prompt".into(),
        ));
    }
    let level = validate_selection(&proposal.selection).map_err(PrepareError::Selection)?;
    let model = &proposal.selection.model;
    let (provider, model_id) = model.split_once('/').expect("validated qualified model");
    let route = crate::runtime::openai::resolve_route(model).ok_or_else(|| {
        PrepareError::Selection("session driver model has no exact supported provider route".into())
    })?;
    if route.provider != provider || route.model != model_id {
        return Err(PrepareError::Selection(
            "session driver refuses provider or model alias substitution".into(),
        ));
    }
    // A media/history incompatibility is a hard stop, even when the same
    // proposal also requested an unsupported effort. Never fail over retained
    // multimodal work as though it were an account or selection failure.
    let mut proposed_history = history.to_vec();
    proposed_history.push(Arc::new(json!({"role":"user", "content":proposal.prompt})));
    crate::runtime::attachments::validate_messages(model, &proposed_history)
        .map_err(PrepareError::Blocked)?;
    validate_model_capability(model, level).map_err(PrepareError::Selection)?;
    let mut candidate = runtime.clone();
    // Same-model changes MUST preserve the existing policy Arc, including
    // session-only worker grants. try_set_model replaces manifestless policy.
    if candidate.model() != model {
        candidate.try_set_model(model.clone()).map_err(|_| {
            PrepareError::Blocked("session driver model change blocked by host policy".into())
        })?;
    }
    // Exact model/effort validity already passed above. Remaining checked-set
    // failures are host prerequisites (e.g. ultracode lifecycle tools), not
    // permission to fail over to another model and evade that prerequisite.
    candidate.set_reasoning_level_checked(level).map_err(|_| {
        PrepareError::Blocked(
            "session driver reasoning change blocked by host prerequisites".into(),
        )
    })?;
    if candidate.model() != model || candidate.reasoning_level() != level {
        return Err(PrepareError::Selection(
            "session driver requires the exact model and effort without downgrades".into(),
        ));
    }
    candidate
        .validate_session_driver_preflight()
        .await
        .map_err(|_| {
            PrepareError::Blocked(
                "session driver blocked by request policy or execution prerequisites".into(),
            )
        })?;
    Ok(candidate)
}

/// Recheck dynamic catalog/media evidence immediately before frontend commit.
/// Async credential preflight may have yielded while a capability was revoked.
pub fn validate_prepared(
    runtime: &Runtime,
    proposal: &Proposal,
    history: &[SharedMessage],
) -> Result<(), String> {
    let level = validate_selection(&proposal.selection)?;
    if runtime.model() != proposal.selection.model || runtime.reasoning_level() != level {
        return Err("prepared runtime no longer matches the exact proposal".into());
    }
    validate_model_capability(&proposal.selection.model, level)?;
    let mut messages = history.to_vec();
    messages.push(Arc::new(json!({"role":"user", "content":proposal.prompt})));
    crate::runtime::attachments::validate_messages(&proposal.selection.model, &messages)
}

/// Mutation validation is deliberately permissive for arbitrary OpenAI-compatible
/// model IDs. A driver is stricter: require an exact known catalog row and named
/// effort evidence; unknown generic capability only allows provider-default
/// `adaptive`, never a silently omitted named effort or off toggle.
fn validate_model_capability(model: &str, level: ReasoningLevel) -> Result<(), String> {
    use crate::runtime::openai::{catalog, registry};
    use catalog::ReasoningSupport;
    let (provider, id) = model.split_once('/').ok_or("invalid qualified model")?;
    let cached = catalog::capability_cache::get(model);
    let known = cached
        .as_ref()
        .is_some_and(|entry| entry.runtime_id() == model)
        || match provider {
            "anthropic" => agent_core::models::KNOWN_MODELS
                .iter()
                .any(|(known, _)| *known == id),
            "openai-codex" => catalog::codex_static_capability(id).is_some(),
            "xai-auth" => catalog::xai_model(id).is_some(),
            "kimi-code" => catalog::kimi_code_model(id).is_some(),
            "github-copilot" => catalog::github_copilot_runtime_model(id).is_some(),
            "google-gemini" => catalog::google_gemini_model(id).is_some(),
            _ => registry::providers()
                .iter()
                .find(|p| p.key == provider)
                .is_some_and(|p| p.models.iter().any(|(known, _, _)| *known == id)),
        };
    if !known {
        return Err("session driver model has no exact known capability descriptor".into());
    }
    if !matches!(
        provider,
        "anthropic" | "openai-codex" | "xai-auth" | "kimi" | "kimi-code"
    ) && level != ReasoningLevel::Adaptive
    {
        let exact_effort = matches!(
            cached.as_ref().map(|entry| &entry.reasoning),
            Some(ReasoningSupport::OpenRouter { effort: true, .. })
        ) && matches!(
            level,
            ReasoningLevel::Low | ReasoningLevel::Medium | ReasoningLevel::High
        );
        if !exact_effort {
            return Err("session driver effort has no exact supported capability evidence".into());
        }
    }
    catalog::validation::validate_reasoning_mutation(model, level)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn selection() -> Selection {
        Selection {
            model: "anthropic/claude-fable-5".into(),
            effort: "high".into(),
        }
    }
    fn start() -> Value {
        json!({"session_driver": {"action":"start", "run_id":"run-1", "models":[selection()], "prompt":"Inspect retained work", "delay_ms":1000}})
    }
    fn next() -> Reply {
        Reply::Next {
            run_id: "run-1".into(),
            selection: selection(),
            prompt: "Continue retained work".into(),
            delay_ms: 1000,
            notice: String::new(),
        }
    }
    fn grant() -> (Grant, Proposal) {
        Grant::from_start("driver", "session", parse_reply(&start()).unwrap().unwrap()).unwrap()
    }
    fn request() -> PollRequest {
        PollRequest {
            run_id: "run-1".into(),
            decision_id: "decision-1".into(),
            outcome: Outcome::Success,
            error_kind: "none".into(),
            model: selection().model,
            effort: selection().effort,
            feedback: None,
        }
    }

    #[test]
    fn parse_only_structured_driver_and_optional_start_selection() {
        assert_eq!(
            parse_reply(&json!({"text":"session_driver start"})).unwrap(),
            None
        );
        let mut value = start();
        value["session_driver"]["selection"] = json!(selection());
        assert!(
            matches!(parse_reply(&value).unwrap(), Some(Reply::Start { notice, max_duration_ms: None, .. }) if notice.is_empty())
        );
        value["session_driver"]["selection"]["effort"] = json!("low");
        assert!(parse_reply(&value).is_err());
        assert!(parse_reply(&json!({"session_driver":null})).is_err());
    }

    #[test]
    fn parser_rejects_unknown_fields_types_and_noncanonical_efforts() {
        for effort in [
            "HIGH", " high", "med", "none", "x-high", "x_high", "maximum", "1000",
        ] {
            let mut value = start();
            value["session_driver"]["models"][0]["effort"] = json!(effort);
            assert!(parse_reply(&value).is_err(), "{effort}");
        }
        for effort in [
            "off",
            "adaptive",
            "low",
            "medium",
            "high",
            "xhigh",
            "max",
            "ultra",
            "ultracode",
        ] {
            assert!(exact_effort(effort).is_ok());
        }
        for (key, bad) in [
            ("unexpected", json!(true)),
            ("delay_ms", json!("1000")),
            ("max_duration_ms", json!(-1)),
            ("notice", json!(false)),
            ("run_id", json!("bad/id")),
        ] {
            let mut value = start();
            value["session_driver"][key] = bad;
            assert!(parse_reply(&value).is_err(), "{key}");
        }
        let mut value = start();
        value["session_driver"]["models"][0]["secret"] = json!("must not echo");
        let err = parse_reply(&value).unwrap_err();
        assert!(!err.contains("must not echo"));
        for model in [
            "claude-fable-5-1",
            "anthropic/",
            "anthropic//model",
            " anthropic/model",
            "anthropic/a\nb",
        ] {
            value = start();
            value["session_driver"]["models"][0]["model"] = json!(model);
            assert!(parse_reply(&value).is_err());
        }
    }

    #[test]
    fn parser_enforces_all_byte_count_and_time_bounds() {
        for (key, good, bad) in [
            (
                "prompt",
                json!("p".repeat(MAX_PROMPT_BYTES)),
                json!("p".repeat(MAX_PROMPT_BYTES + 1)),
            ),
            (
                "notice",
                json!("n".repeat(MAX_NOTICE_BYTES)),
                json!("n".repeat(MAX_NOTICE_BYTES + 1)),
            ),
            (
                "run_id",
                json!("r".repeat(MAX_ID_BYTES)),
                json!("r".repeat(MAX_ID_BYTES + 1)),
            ),
            ("delay_ms", json!(MAX_DELAY_MS), json!(MAX_DELAY_MS + 1)),
            (
                "max_duration_ms",
                json!(MAX_DURATION_MS),
                json!(MAX_DURATION_MS + 1),
            ),
        ] {
            let mut value = start();
            value["session_driver"][key] = good;
            assert!(parse_reply(&value).is_ok(), "{key}");
            value["session_driver"][key] = bad;
            assert!(parse_reply(&value).is_err(), "{key}");
        }
        for (key, bad) in [
            ("delay_ms", json!(999)),
            ("max_duration_ms", json!(0)),
            ("prompt", json!(" \n\t")),
            ("run_id", json!("")),
        ] {
            let mut value = start();
            value["session_driver"][key] = bad;
            assert!(parse_reply(&value).is_err());
        }
        let mut value = start();
        value["session_driver"]["prompt"] = json!("é".repeat(MAX_PROMPT_BYTES / 2 + 1));
        assert!(parse_reply(&value).is_err());
        value = start();
        value["unrelated"] = json!("x".repeat(MAX_REPLY_BYTES));
        assert!(parse_reply(&value).is_err());
        value = start();
        value["session_driver"]["models"][0]["model"] =
            json!(format!("p/{}", "m".repeat(MAX_MODEL_BYTES - 2)));
        assert!(parse_reply(&value).is_ok());
        value["session_driver"]["models"][0]["model"] =
            json!(format!("p/{}", "m".repeat(MAX_MODEL_BYTES - 1)));
        assert!(parse_reply(&value).is_err());
        value = start();
        let models: Vec<_> = (0..MAX_MODELS)
            .map(|i| json!({"model":format!("p/m{i}"),"effort":"high"}))
            .collect();
        value["session_driver"]["models"] = json!(models);
        assert!(parse_reply(&value).is_ok());
        value["session_driver"]["models"]
            .as_array_mut()
            .unwrap()
            .push(json!({"model":"p/extra","effort":"high"}));
        assert!(parse_reply(&value).is_err());
        value["session_driver"]["models"] = json!([]);
        assert!(parse_reply(&value).is_err());
        value["session_driver"]["models"] = json!([
            selection(),
            Selection {
                effort: "low".into(),
                ..selection()
            }
        ]);
        assert!(parse_reply(&value).is_err());
    }

    #[test]
    fn grant_pins_exact_run_model_effort_and_rejects_rearming() {
        let (mut grant, first) = grant();
        assert_eq!(first.selection, selection());
        assert_eq!(grant.plugin_id, "driver");
        assert_eq!(grant.session_id, "session");
        assert!(grant.deadline().is_none());
        assert!(!grant.expired());
        assert!(grant.accept(next()).unwrap().is_some());
        let Reply::Next {
            selection: first_selection,
            prompt,
            delay_ms,
            notice,
            ..
        } = next()
        else {
            unreachable!()
        };
        assert!(grant
            .accept(Reply::Next {
                run_id: "different-run".into(),
                selection: first_selection,
                prompt,
                delay_ms,
                notice
            })
            .is_err());
        let Reply::Next {
            run_id,
            prompt,
            delay_ms,
            notice,
            ..
        } = next()
        else {
            unreachable!()
        };
        for selected in [
            Selection {
                effort: "low".into(),
                ..selection()
            },
            Selection {
                model: "openai-codex/gpt-6-astra".into(),
                effort: "ultra".into(),
            },
        ] {
            assert!(grant
                .accept(Reply::Next {
                    run_id: run_id.clone(),
                    selection: selected,
                    prompt: prompt.clone(),
                    delay_ms,
                    notice: notice.clone()
                })
                .is_err());
        }
        assert!(grant
            .accept(parse_reply(&start()).unwrap().unwrap())
            .is_err());
        assert!(grant
            .accept(Reply::Status {
                notice: String::new()
            })
            .is_err());
        assert!(grant
            .accept(Reply::Stop {
                notice: String::new()
            })
            .unwrap()
            .is_none());
        grant.run_id = "different-run".into();
        assert!(
            grant.accept(next()).is_ok(),
            "public bookkeeping must not change pinned authority"
        );
    }

    #[test]
    fn grant_revalidates_direct_constructors_and_deadline_scheduling() {
        let (mut grant, _) = grant();
        grant.deadline = Some(Instant::now() + Duration::from_millis(500));
        assert!(!grant.expired());
        assert!(
            grant.accept(next()).is_err(),
            "do not schedule beyond deadline"
        );
        grant.deadline = Some(Instant::now() - Duration::from_millis(1));
        assert!(grant.expired());
        assert!(grant.accept(next()).is_err());
        let mut value = start();
        value["session_driver"]["max_duration_ms"] = json!(500);
        assert!(Grant::from_start("p", "s", parse_reply(&value).unwrap().unwrap()).is_err());
        value["session_driver"]["max_duration_ms"] = json!(10_000);
        assert!(
            Grant::from_start("p", "s", parse_reply(&value).unwrap().unwrap())
                .unwrap()
                .0
                .deadline()
                .is_some()
        );
        let bad = Reply::Start {
            run_id: "x".into(),
            models: vec![],
            prompt: "hello".into(),
            delay_ms: 1000,
            max_duration_ms: None,
            feedback_version: None,
            time_checkpoint_version: None,
            context_mode: None,
            notice: String::new(),
        };
        assert!(Grant::from_start("p", "s", bad).is_err());
    }

    #[test]
    fn time_checkpoint_support_is_versioned_and_not_provider_failure() {
        let legacy = parse_reply(&start()).unwrap().unwrap();
        assert!(!Grant::from_start("p", "s", legacy)
            .unwrap()
            .0
            .time_checkpoints_enabled());
        let mut value = start();
        value["session_driver"]["time_checkpoint_version"] = json!(1);
        let grant = Grant::from_start("p", "s", parse_reply(&value).unwrap().unwrap())
            .unwrap()
            .0;
        assert!(grant.time_checkpoints_enabled());
        for bad in [json!(0), json!(2), json!(true), json!("1")] {
            value["session_driver"]["time_checkpoint_version"] = bad;
            assert!(parse_reply(&value).is_err());
        }
        assert_eq!(
            classify_turn_error(&agent_core::TurnError::budget(
                agent_core::BudgetDimension::WallClock
            )),
            (Outcome::TimeCheckpoint, "wall_clock".into())
        );
        for dimension in [
            agent_core::BudgetDimension::CostUsd,
            agent_core::BudgetDimension::ToolCalls,
            agent_core::BudgetDimension::ProviderRounds,
        ] {
            assert_eq!(
                classify_turn_error(&agent_core::TurnError::budget(dimension)),
                blocked()
            );
        }
    }

    #[test]
    fn context_mode_is_optional_strict_and_start_only() {
        let legacy = start();
        assert!(matches!(
            parse_reply(&legacy).unwrap(),
            Some(Reply::Start {
                context_mode: None,
                ..
            })
        ));
        for (name, expected) in [("auto", ContextMode::Auto), ("off", ContextMode::Off)] {
            let mut value = start();
            value["session_driver"]["context_mode"] = json!(name);
            let reply = parse_reply(&value).unwrap().unwrap();
            assert!(
                matches!(&reply, Reply::Start { context_mode:Some(mode), .. } if *mode == expected)
            );
            let encoded = serde_json::to_value(&reply).unwrap();
            assert_eq!(encoded["context_mode"], name);
            let (grant, _) = Grant::from_start("fixture", "session", reply).unwrap();
            assert_eq!(grant.context_mode, Some(expected));
        }
        for bad in [
            json!(null),
            json!(true),
            json!(1),
            json!("AUTO"),
            json!("inherit"),
            json!({}),
            json!([]),
        ] {
            let mut value = start();
            value["session_driver"]["context_mode"] = bad;
            assert!(parse_reply(&value).is_err());
        }
        for action in ["next", "stop", "status"] {
            let value = json!({"session_driver":{
                "action":action, "context_mode":"auto"
            }});
            assert!(parse_reply(&value).is_err());
        }
    }

    #[test]
    #[serial_test::serial(synaps_base_dir)]
    fn context_mode_uses_session_command_without_recall_or_global_config_changes() {
        let env = crate::test_env::BaseDirGuard::new();
        let runtime = Runtime::new_headless();
        let (legacy, _) = Grant::from_start(
            "fixture",
            "session",
            parse_reply(&start()).unwrap().unwrap(),
        )
        .unwrap();
        assert!(legacy.apply_context_mode(&runtime).unwrap().is_none());
        assert!(!runtime.context_management_enabled());
        let mut value = start();
        value["session_driver"]["context_mode"] = json!("auto");
        let (auto, _) =
            Grant::from_start("fixture", "session", parse_reply(&value).unwrap().unwrap()).unwrap();
        let notice = auto.apply_context_mode(&runtime).unwrap().unwrap();
        assert!(runtime.context_management_enabled());
        assert!(notice.contains("Session-only"));
        assert!(notice.contains("excludes private reasoning"));
        assert!(legacy.apply_context_mode(&runtime).unwrap().is_none());
        assert!(
            runtime.context_management_enabled(),
            "legacy mode cannot undo an explicit choice"
        );
        value["session_driver"]["context_mode"] = json!("off");
        let (off, _) =
            Grant::from_start("fixture", "session", parse_reply(&value).unwrap().unwrap()).unwrap();
        off.apply_context_mode(&runtime).unwrap();
        assert!(!runtime.context_management_enabled());
        assert!(!env.path().join("config").exists());
        assert!(!env.path().join("context-archives").exists());
    }

    #[test]
    #[serial_test::serial(synaps_base_dir)]
    fn context_auto_backend_failure_preserves_previous_mode() {
        let _env = crate::test_env::BaseDirGuard::new();
        let mut runtime = Runtime::new_headless();
        runtime.apply_memory_backend_config(&agent_core::config::MemoryBackendConfig {
            kind: agent_core::config::MemoryBackendKind::Unavailable,
            ..Default::default()
        });
        let mut value = start();
        value["session_driver"]["context_mode"] = json!("auto");
        let (grant, _) =
            Grant::from_start("fixture", "session", parse_reply(&value).unwrap().unwrap()).unwrap();
        assert!(grant
            .apply_context_mode(&runtime)
            .unwrap_err()
            .contains("unavailable"));
        assert!(!runtime.context_management_enabled());
    }

    #[test]
    fn known_stream_failures_reach_fallback_without_promoting_policy_or_context() {
        for (kind, expected) in [
            ("authentication_error", "auth"),
            ("billing_error", "quota"),
            ("rate_limit_error", "rate_limit"),
            ("api_error", "transient"),
            ("overloaded_error", "transient"),
            ("timeout_error", "transient"),
        ] {
            let msg = format!("API stream error ({kind}). Provider error details withheld — they can echo request content.");
            assert_eq!(
                classify_turn_error(&agent_core::TurnError::provider(&msg, "api_status", "t")),
                provider_error(expected)
            );
            assert_eq!(
                classify_turn_error(&agent_core::TurnError::provider(&msg, "tool_error", "t")),
                blocked()
            );
            assert_eq!(classify_error(&format!("arbitrary text {msg}")), blocked());
        }
        for kind in [
            "permission_error",
            "invalid_request_error",
            "request_too_large",
            "unrecognized_error",
        ] {
            assert_eq!(classify_error(&format!("API stream error ({kind}). Provider error details withheld — they can echo request content.")), blocked());
        }
        use crate::runtime::openai::stream::*;
        for label in ["Codex", "OpenAI", "xAI"] {
            for (suffix, expected) in [
                (RESPONSES_AUTH_SUFFIX, "auth"),
                (RESPONSES_QUOTA_SUFFIX, "quota"),
                (RESPONSES_EMPTY_SUFFIX, "transient"),
                (RESPONSES_MISSING_TERMINAL_SUFFIX, "transient"),
            ] {
                assert_eq!(
                    classify_error(&format!("{label}{suffix}")),
                    provider_error(expected)
                );
            }
            for suffix in [
                RESPONSES_CONTEXT_SUFFIX,
                RESPONSES_INCOMPLETE_SUFFIX,
                RESPONSES_FAILED_SUFFIX,
            ] {
                assert_eq!(classify_error(&format!("{label}{suffix}")), blocked());
            }
        }
        for msg in ["Request to api.anthropic.com timed out. Check your connection and try again.",
            "Could not reach api.anthropic.com (connection failed). Check your network, DNS, or proxy settings.",
            "Connection lost mid-response. Partial reply kept — send again to continue."] {
            assert_eq!(classify_error(msg), provider_error("transient"));
        }
    }

    #[test]
    fn feedback_version_is_explicit_pinned_and_bounded() {
        let (grant, _) =
            Grant::from_start("p", "s", parse_reply(&start()).unwrap().unwrap()).unwrap();
        assert!(!grant.feedback_enabled());
        let legacy = serde_json::to_value(request()).unwrap();
        assert!(legacy.get("feedback").is_none());
        let mut value = start();
        value["session_driver"]["feedback_version"] = json!(1);
        let (grant, _) =
            Grant::from_start("p", "s", parse_reply(&value).unwrap().unwrap()).unwrap();
        assert!(grant.feedback_enabled());
        for bad in [
            json!(0),
            json!(2),
            json!(true),
            json!("1"),
            json!(256),
            json!(-1),
        ] {
            value["session_driver"]["feedback_version"] = bad;
            assert!(parse_reply(&value).is_err());
        }
    }

    #[test]
    fn typed_error_classification_uses_host_templates_not_arbitrary_substrings() {
        for (message, kind) in [
            ("API error: Authentication rejected. Run `synaps login` to re-authenticate.", "auth"),
            ("API error: codex request failed: 401: SECRET", "auth"),
            ("API error: openai request failed: 503: SECRET", "transient"),
            ("openai request failed: provider request failed: 429 Too Many Requests", "rate_limit"),
            ("Rate limited by Anthropic (HTTP 429 [rate_limit_error]). Wait for reset", "rate_limit"),
            ("Rate limit exhausted — retries used up while waiting for reset (next window in 5s). Try again shortly", "rate_limit"),
            ("Anthropic is overloaded right now. Retries exhausted — wait a minute and try again.", "transient"),
            ("Anthropic server error (HTTP 503 [api_error]). Retries exhausted", "transient"),
            ("kimi-code: usage quota exhausted (HTTP 403). Your plan's usage window is used up", "quota"),
            ("xAI reports the model is at capacity (retries exhausted). Try again in a few minutes or switch models with /model.", "transient"),
            ("Request to chatgpt.com timed out — usually transient; check your connection and try again. [SECRET]", "transient"),
            ("Could not reach api.x.ai (connection failed). Check your network, DNS, or proxy settings. [SECRET]", "transient"),
        ] {
            let error = agent_core::TurnError::provider(message, "api_status", "not-forwarded");
            assert_eq!(classify_turn_error(&error), provider_error(kind), "{message}");
        }
        for code in ["config_error", "session_error", "tool_error", "unknown"] {
            let error = agent_core::TurnError::provider(
                "API error: codex request failed: 401: SECRET",
                code,
                "private",
            );
            assert_eq!(classify_turn_error(&error), blocked());
        }
        for message in [
            "transcript says auth 401 quota",
            "Tool execution failed: openai request failed: 429",
            "codex request failed: 4290: fake",
            "Access denied (HTTP 403). Your account may not have access to this model.",
            "Context window exceeded (HTTP 400)",
            "Request too large",
            "invalid media auth error",
        ] {
            assert_eq!(
                classify_turn_error(&agent_core::TurnError::provider(
                    message,
                    "api_status",
                    "private"
                )),
                blocked(),
                "{message}"
            );
        }
        for code in ["network_error", "timeout"] {
            assert_eq!(
                classify_turn_error(&agent_core::TurnError::provider("SECRET", code, "private")),
                provider_error("transient")
            );
        }
        for message in [
            "SECRET",
            "Auth error: Token refresh failed: broker rejected machine auth",
            "Auth error: Token refresh failed: broker denied request: policy",
            "Auth error: Token refresh failed: broker transport error: broker HTTP 403",
            "Auth error: Token refresh failed: credential error: storage unavailable",
        ] {
            assert_eq!(
                classify_turn_error(&agent_core::TurnError::provider(
                    message,
                    "auth_error",
                    "private"
                )),
                blocked()
            );
        }
        assert_eq!(
            classify_turn_error(&agent_core::TurnError::provider(
                "Auth error: No API key or OAuth token found. Run `synaps login` to authenticate.",
                "auth_error",
                "t"
            )),
            provider_error("auth")
        );
        for outcome in [
            agent_core::TurnOutcome::Canceled,
            agent_core::TurnOutcome::Completed,
            agent_core::TurnOutcome::InterruptedAfterSideEffect {
                call_id: "private".into(),
            },
        ] {
            assert_eq!(
                classify_turn_error(&agent_core::TurnError {
                    message: "openai request failed: 429".into(),
                    outcome
                }),
                blocked()
            );
        }
    }

    #[test]
    fn missing_accounts_are_exact_exceptions_not_generic_config_failover() {
        for (code, message) in [
            ("config_error", "Config error: openai provider: no credential configured for 'openai-codex'. Run `synaps login` to add one."),
            ("config_error", "Config error: openai provider: openai request failed: no credential configured for 'kimi-code'. Run `synaps login` to add one."),
            ("api_status", "API error: openai request failed: no credential configured for 'kimi-code'. Run `synaps login` to add one."),
        ] {
            assert_eq!(classify_turn_error(&agent_core::TurnError::provider(message, code, "private")), provider_error("auth"));
        }
        for message in [
            "Config error: policy auth denied",
            "Config error: openai provider: no credential configured for 'invented'. Run `synaps login` to add one.",
            "Config error: openai provider: no credential configured for 'kimi-code'. Run `synaps login` to add one. SECRET",
            "Config error: openai provider: broker denied request: no credential configured for 'kimi-code'. Run `synaps login` to add one.",
        ] {
            assert_eq!(classify_turn_error(&agent_core::TurnError::provider(message, "config_error", "private")), blocked());
        }
        assert_eq!(classify_error("API error: openai request failed: broker transport error: provider request failed: 503 Service Unavailable [server_error]"), provider_error("transient"));
        let message =
            crate::RuntimeError::ApiStatus(crate::core::error::humanize_api_error(429, ""))
                .to_string();
        assert_eq!(
            classify_turn_error(&agent_core::TurnError::provider(
                message,
                "api_status",
                "private"
            )),
            provider_error("rate_limit")
        );
    }

    struct FakeHandler {
        reply: Result<Value, String>,
        calls: AtomicUsize,
        flood: usize,
        delay: Duration,
        dropped: Arc<AtomicBool>,
    }
    impl FakeHandler {
        fn new(reply: Value) -> Self {
            Self {
                reply: Ok(reply),
                calls: AtomicUsize::new(0),
                flood: 0,
                delay: Duration::ZERO,
                dropped: Arc::new(AtomicBool::new(false)),
            }
        }
    }
    struct DropMarker(Arc<AtomicBool>);
    impl Drop for DropMarker {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    #[async_trait::async_trait]
    impl ExtensionHandler for FakeHandler {
        fn id(&self) -> &str {
            "driver"
        }
        async fn handle(
            &self,
            _: &crate::extensions::hooks::events::HookEvent,
        ) -> crate::extensions::hooks::events::HookResult {
            Default::default()
        }
        async fn shutdown(&self) {}
        async fn invoke_command(
            &self,
            command: &str,
            args: Vec<String>,
            request_id: &str,
            sink: super::super::invoke_output::InvokeEventSink,
        ) -> Result<Value, String> {
            let _drop = DropMarker(self.dropped.clone());
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(command, "__session_driver__");
            assert!(uuid::Uuid::parse_str(request_id).is_ok());
            assert_eq!(args.len(), 1);
            let request: Value = serde_json::from_str(&args[0]).unwrap();
            assert_eq!(request["decision_id"], "decision-1");
            assert!(request.get("messages").is_none());
            for _ in 0..self.flood {
                let _ = sink
                    .send(InvokeCommandEvent::Output(CommandOutputEvent::Text {
                        content: "f".repeat(4096),
                    }))
                    .await;
            }
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            self.reply.clone()
        }
    }

    #[tokio::test]
    async fn poll_collector_is_concurrent_bounded_and_ignores_display_authority() {
        let handler = Arc::new(FakeHandler::new(json!({"session_driver":next()})));
        assert!(matches!(
            poll(handler.clone(), request()).await.unwrap(),
            Reply::Next { .. }
        ));
        assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
        let mut flooding = FakeHandler::new(json!({"session_driver":next()}));
        flooding.flood = 256;
        let result = poll(Arc::new(flooding), request()).await;
        assert!(result.unwrap_err().contains("budget"));
        let display_only = Arc::new(FakeHandler::new(
            json!({"text":"{\"session_driver\":{\"action\":\"next\"}}"}),
        ));
        assert!(poll(display_only, request()).await.is_err());
        let mut secret_error = FakeHandler::new(json!({}));
        secret_error.reply = Err("SECRET transport error".into());
        assert!(!poll(Arc::new(secret_error), request())
            .await
            .unwrap_err()
            .contains("SECRET"));
        let mut invalid = request();
        invalid.error_kind = "raw secret error".into();
        assert!(poll(handler.clone(), invalid).await.is_err());
        assert_eq!(
            handler.calls.load(Ordering::SeqCst),
            1,
            "validate before RPC"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn poll_timeout_and_cancellation_drop_invocation_without_detached_work() {
        let mut handler = FakeHandler::new(json!({"session_driver":next()}));
        handler.delay = Duration::from_secs(30);
        let marker = handler.dropped.clone();
        assert!(poll(Arc::new(handler), request())
            .await
            .unwrap_err()
            .contains("timed out"));
        assert!(marker.load(Ordering::SeqCst));
        let mut handler = FakeHandler::new(json!({"session_driver":next()}));
        handler.delay = Duration::from_secs(30);
        let handler = Arc::new(handler);
        let marker = handler.dropped.clone();
        let task = tokio::spawn(poll(handler.clone(), request()));
        tokio::task::yield_now().await;
        assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(marker.load(Ordering::SeqCst));
    }

    fn runtime_with_policy() -> Runtime {
        let mut runtime = Runtime::new_headless();
        runtime.try_set_model(selection().model).unwrap();
        runtime
            .set_reasoning_level_checked(ReasoningLevel::High)
            .unwrap();
        let model = agent_core::prompt::QualifiedModelId::parse(runtime.model()).unwrap();
        let policy = crate::orchestration::OrchestrationRuntime::baseline(model, 4, 16).unwrap();
        runtime.install_orchestration(Arc::new(policy));
        runtime
    }
    #[tokio::test]
    async fn prepared_commit_rechecks_exact_selection_and_history() {
        let runtime = runtime_with_policy();
        let (_, mut proposal) = grant();
        let candidate = prepare(&runtime, &proposal, &[]).await.unwrap();
        assert!(validate_prepared(&candidate, &proposal, &[]).is_ok());
        proposal.selection.effort = "low".into();
        assert!(validate_prepared(&candidate, &proposal, &[]).is_err());
        proposal.selection = selection();
        let history = vec![Arc::new(
            json!({"role":"user", "content":[{"type":"image", "source":{"type":"base64", "media_type":"image/png", "data":"corrupt"}}]}),
        )];
        assert!(validate_prepared(&candidate, &proposal, &history).is_err());
    }

    #[tokio::test]
    async fn prepare_preserves_same_model_policy_and_original_state() {
        let runtime = runtime_with_policy();
        let policy = runtime.orchestration().unwrap();
        policy
            .grant_worker_model("openai-codex/gpt-6-astra")
            .unwrap();
        let choices = policy.effective_choices();
        let (_, mut proposal) = grant();
        proposal.selection.effort = "low".into();
        let result = prepare(&runtime, &proposal, &[]).await.unwrap();
        assert!(Arc::ptr_eq(policy, result.orchestration().unwrap()));
        assert_eq!(choices, result.orchestration().unwrap().effective_choices());
        assert_eq!(runtime.reasoning_level(), ReasoningLevel::High);
        assert_eq!(result.reasoning_level(), ReasoningLevel::Low);
        assert_eq!(
            runtime.host_tool_session_id(),
            result.host_tool_session_id()
        );
        proposal.selection.effort = "ultra".into();
        assert!(matches!(
            prepare(&runtime, &proposal, &[]).await,
            Err(PrepareError::Selection(_))
        ));
        assert_eq!(runtime.reasoning_level(), ReasoningLevel::High);
        assert_eq!(runtime.model(), selection().model);
    }

    #[tokio::test]
    async fn prepare_blocks_workers_before_selection_and_pinned_prompt_changes() {
        let mut runtime = runtime_with_policy();
        let policy = runtime.orchestration().unwrap().clone();
        policy.authorize("sa_driver_test", runtime.model()).unwrap();
        policy
            .terminal_and_collect(
                "sa_driver_test",
                agent_core::orchestration::WorkerTerminal::Completed,
            )
            .unwrap();
        let (_, mut proposal) = grant();
        proposal.selection.model = "invalid".into();
        assert!(matches!(
            prepare(&runtime, &proposal, &[]).await,
            Err(PrepareError::Blocked(_))
        ));
        proposal.selection = selection();
        assert!(matches!(
            prepare(&runtime, &proposal, &[]).await,
            Err(PrepareError::Blocked(_))
        ));
        policy.reconcile("sa_driver_test").unwrap();
        let context = agent_core::prompt::SelectionContext::new(
            agent_core::prompt::QualifiedModelId::parse(runtime.model()).unwrap(),
            None,
        )
        .unwrap();
        runtime
            .apply_prompt_stack(agent_core::prompt::PromptStack::new(vec![], context).unwrap())
            .unwrap();
        proposal.selection = Selection {
            model: "openai-codex/gpt-6-astra".into(),
            effort: "ultra".into(),
        };
        assert!(matches!(
            prepare(&runtime, &proposal, &[]).await,
            Err(PrepareError::Blocked(_))
        ));
    }

    #[tokio::test]
    async fn prepare_validates_complete_history_and_never_aliases_models() {
        let runtime = runtime_with_policy();
        let (_, mut proposal) = grant();
        for model in [
            "x-ai/grok-4.6",
            "anthropic/claude-invented",
            "codex/gpt-6-astra",
            "groq/invented",
        ] {
            proposal.selection.model = model.into();
            assert!(
                matches!(
                    prepare(&runtime, &proposal, &[]).await,
                    Err(PrepareError::Selection(_))
                ),
                "{model}"
            );
        }
        proposal.selection = selection();
        let history = vec![
            Arc::new(
                json!({"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"invalid base64!"}}]}),
            ),
            Arc::new(json!({"role":"assistant","content":"retained"})),
        ];
        let snapshot = history.clone();
        assert!(matches!(
            prepare(&runtime, &proposal, &history).await,
            Err(PrepareError::Blocked(_))
        ));
        assert_eq!(history, snapshot);
        // Unsupported model history must not masquerade as unsupported effort.
        proposal.selection.effort = "ultra".into();
        assert!(matches!(
            prepare(&runtime, &proposal, &history).await,
            Err(PrepareError::Blocked(_))
        ));
        assert_eq!(runtime.reasoning_level(), ReasoningLevel::High);
    }

    #[tokio::test]
    async fn manager_rejects_unknown_and_deferred_without_rpc() {
        let mut manager = super::super::manager::ExtensionManager::new(Arc::new(
            super::super::hooks::HookBus::new(),
        ));
        assert!(manager.session_driver_handler("unknown").is_err());
        manager.set_progressive_deferral(true);
        let manifest = serde_json::from_value(json!({"runtime":"process", "command":"/must/not/spawn", "permissions":["session.drive"], "deferred":{"lifecycle":"user"}})).unwrap();
        manager.load("driver", &manifest).await.unwrap();
        assert!(manager
            .session_driver_handler("driver")
            .err()
            .unwrap()
            .contains("deferred"));
        manager.unload("driver").await.unwrap();
        assert!(manager.session_driver_handler("driver").is_err());
    }

    #[tokio::test]
    async fn eager_permission_lifecycle_revokes_and_failed_reload_cannot_reuse_authority() {
        // Self-contained offline process: no filesystem fixture, user config,
        // provider access, optional capabilities or sidecar activation.
        const PROCESS: &str = r#"
import sys, json
while True:
    n = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        if line in (b'\r\n', b'\n'): break
        if line.lower().startswith(b'content-length:'): n = int(line.split(b':', 1)[1])
    if n is None: sys.exit(1)
    request = json.loads(sys.stdin.buffer.read(n))
    if 'id' not in request:
        if request.get('method') == 'shutdown': sys.exit(0)
        continue
    result = {'protocol_version': 1, 'capabilities': {}} if request['method'] == 'initialize' else {}
    body = json.dumps({'jsonrpc':'2.0', 'id':request['id'], 'result':result}).encode()
    sys.stdout.buffer.write(b'Content-Length: ' + str(len(body)).encode() + b'\r\n\r\n' + body)
    sys.stdout.buffer.flush()
    if request['method'] == 'shutdown': sys.exit(0)
"#;
        let mut manager = super::super::manager::ExtensionManager::new(Arc::new(
            super::super::hooks::HookBus::new(),
        ));
        let mut manifest: super::super::manifest::ExtensionManifest = serde_json::from_value(json!({
            "runtime":"process", "command":"python3", "args":["-u", "-c", PROCESS], "permissions":["session.drive"]
        })).unwrap();
        manager.load("driver", &manifest).await.unwrap();
        let original = manager.session_driver_handler("driver").unwrap();
        assert!(Arc::ptr_eq(
            &original,
            &manager.session_driver_handler("driver").unwrap()
        ));
        manager.reload("driver", &manifest, None).await.unwrap();
        let replacement = manager.session_driver_handler("driver").unwrap();
        assert!(!Arc::ptr_eq(&original, &replacement));
        manifest.permissions = vec!["tools.register".into()];
        manager.reload("driver", &manifest, None).await.unwrap();
        assert!(manager.session_driver_handler("driver").is_err());
        manifest.permissions.push("session.drive".into());
        manager.reload("driver", &manifest, None).await.unwrap();
        manifest.command = "/must/not/spawn".into();
        assert!(manager.reload("driver", &manifest, None).await.is_err());
        assert!(manager.session_driver_handler("driver").is_err());
        manifest.permissions.push("not.a.permission".into());
        assert!(manager
            .load("driver", &manifest)
            .await
            .unwrap_err()
            .contains("permission"));
        assert!(manager.session_driver_handler("driver").is_err());
    }
}
