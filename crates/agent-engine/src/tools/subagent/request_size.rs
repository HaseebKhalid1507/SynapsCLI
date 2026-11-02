//! Context admission for subagent start, resume and oneshot.
//!
//! Before a worker starts, resumes or runs as a oneshot, [`admit_resume`] or
//! [`admit_fresh`] measures the first request it will send: the system
//! prompt, the exposed tool schemas and the history, built the way the spawn
//! thread builds them, and charged by the engine's request-aware estimator
//! ([`crate::runtime::context::assess`], the estimator behind compaction and
//! rollover). No provider call is made; the cost is one serialization pass
//! over the history, with images charged flat.
//!
//! The figure the limits compare against is in provider tokens:
//! - `calibrated`: the estimate times provider/estimated from the calling
//!   session's own calibration sample, when that sample is plausible and was
//!   measured on the same model identity (same resolved route);
//! - `estimate`: otherwise the estimate divided by 1.2, just under the lowest
//!   estimate/provider ratio seen on Claude sessions, so it still reads at or
//!   above the provider count for most requests. It may read high, and the
//!   caller is told so. The tracing line (target [`LOG_TARGET`]) logs
//!   the raw estimate next to the compared figure, so the factor can be
//!   refitted from real runs.
//!
//! Limits, read from the config on every call (`off` disables a line; the
//! operator changes them in the config, a caller cannot override them):
//! - `subagent.resume_refuse_tokens` (default 250,000): a resume above it is
//!   refused before any id, reservation, model grant, handle or event
//!   exists. The refusal tells the caller to start a fresh worker from a
//!   short brief and quotes the end of the worker's last report.
//! - `subagent.context_warn_tokens` (default 150,000): any kind at or above
//!   it is admitted with a `context_warning`.
//! - Start and oneshot are never refused.
//! - A warn line above the refuse line is lowered to it by the config
//!   parser; the first admission that sees a given pair logs one WARN
//!   (target [`LOG_TARGET`], the two numbers only).
//!
//! These are context limits, not spend caps.
//!
//! **The one resume check.** [`admit_resume`] is the admission check for
//! every path that resumes a worker. The `subagent_resume` tool is its first
//! caller, and today its only one: nothing else replays a worker archive.
//! Any other resume path (for example a daemon-protocol Resume from an app)
//! must call it where it has built the resumed run's history and before it
//! allocates an id or a handle, exactly as the tool does, and so gets the
//! same limits. There is no way round them.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

use super::super::ToolContext;
use crate::runtime::context;
use crate::runtime::continuation::{ContinuationState, TokenCalibration};
use crate::{Result, RuntimeError, SharedMessage, ToolRegistry};

/// Stable start of every refusal, so an app or harness can match it.
pub(crate) const REFUSAL_PREFIX: &str = "subagent_resume refused (context limit):";

/// Tracing target of every admission line. The daemon's log filter keeps
/// lines by target PREFIX (`agent_engine=debug` among the crate directives;
/// everything else at ERROR unless `RUST_LOG` names it), so the target starts
/// with the crate name. It was `subagent_admission` before, and synaps.log
/// dropped those lines; a `RUST_LOG` that named the old target must name
/// this one.
pub(crate) const LOG_TARGET: &str = "agent_engine::subagent_admission";

/// How much of the worker's last report a refusal quotes.
pub(crate) const REPORT_EXCERPT_CHARS: usize = 2_000;

/// Uncalibrated sizes are the estimate divided by 6/5 = 1.2.
const UNCALIBRATED_RATIO: (u64, u64) = (6, 5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Start,
    Resume,
    Oneshot,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Start => "start",
            Kind::Resume => "resume",
            Kind::Oneshot => "oneshot",
        }
    }
}

/// The kinds that are measured but never refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fresh {
    Start,
    Oneshot,
}

impl From<Fresh> for Kind {
    fn from(kind: Fresh) -> Self {
        match kind {
            Fresh::Start => Kind::Start,
            Fresh::Oneshot => Kind::Oneshot,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Basis {
    Calibrated,
    Estimate,
}

impl Basis {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Basis::Calibrated => "calibrated",
            Basis::Estimate => "estimate",
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Basis::Calibrated => "calibrated estimate",
            Basis::Estimate => "estimate, may read high",
        }
    }
}

/// Size of a worker's first request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RequestSize {
    /// Provider tokens; the figure the limits compare against.
    pub context_tokens: u64,
    /// The conservative estimator's raw figure.
    pub estimate_tokens: u64,
    pub basis: Basis,
}

/// The two lines, in provider tokens; `None` = off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    pub warn_tokens: Option<u64>,
    pub refuse_tokens: Option<u64>,
}

impl Limits {
    /// The config parser already clamps the warn line to the refuse line.
    pub(crate) fn from_config(limits: &crate::config::SubagentLimits) -> Self {
        Self {
            warn_tokens: limits.context_warn_tokens,
            refuse_tokens: limits.resume_refuse_tokens,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Admitted {
    pub size: RequestSize,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Admission {
    Admitted(Admitted),
    Refused { size: RequestSize, message: String },
}

impl Admitted {
    /// Adds `context_tokens`, `context_basis` and (when it applies)
    /// `context_warning` to a start or resume reply. New keys: the reply's
    /// `warning` key carries other warnings and is left alone.
    pub(crate) fn add_reply_fields(&self, reply: &mut Value) {
        if let Some(fields) = reply.as_object_mut() {
            fields.insert("context_tokens".into(), json!(self.size.context_tokens));
            fields.insert("context_basis".into(), json!(self.size.basis.as_str()));
            if let Some(warning) = &self.warning {
                fields.insert("context_warning".into(), json!(warning));
            }
        }
    }

    /// A oneshot's result is the worker's answer: the warning is appended
    /// only at or above the warn line, never a size on every result.
    pub(crate) fn with_notice(&self, text: String) -> String {
        match &self.warning {
            Some(warning) => format!("{text}\n\n[subagent context warning] {warning}"),
            None => text,
        }
    }

    /// One line for the oneshot log file.
    pub(crate) fn log_line(&self) -> String {
        format!(
            "Context: about {} tokens ({})",
            group(self.size.context_tokens),
            self.size.basis.as_str()
        )
    }
}

/// The admission figure, carried into the worker's run so that the
/// provider's own count for the worker's FIRST request is logged next to it,
/// once per run (target [`LOG_TARGET`], message "subagent first
/// request"). This is the calibration record for the uncalibrated 1.2
/// divisor. Metadata only: the handle id, the kind, the model the run uses,
/// the raw estimate, the compared figure and its basis, and the provider's
/// input tokens (input + cache read + cache creation of the first Usage). No
/// text. A run that never reaches its first Usage logs nothing.
#[derive(Debug)]
pub(crate) struct FirstRequest {
    handle_id: String,
    kind: Kind,
    model: String,
    size: RequestSize,
    logged: AtomicBool,
}

impl Admitted {
    /// The calibration probe for the run this admission let through: built
    /// once the handle id and the run's model exist.
    pub(crate) fn first_request(&self, kind: Kind, handle_id: &str, model: &str) -> FirstRequest {
        FirstRequest {
            handle_id: handle_id.to_owned(),
            kind,
            model: model.to_owned(),
            size: self.size,
            logged: AtomicBool::new(false),
        }
    }
}

impl FirstRequest {
    /// Call on every Usage of the run; only the first one is logged.
    pub(crate) fn on_usage(&self, input_tokens: u64, cache_read: u64, cache_creation: u64) {
        if self.logged.swap(true, Ordering::Relaxed) {
            return;
        }
        let provider_tokens = input_tokens
            .saturating_add(cache_read)
            .saturating_add(cache_creation);
        tracing::info!(
            target: LOG_TARGET,
            handle_id = self.handle_id.as_str(),
            kind = self.kind.as_str(),
            model = self.model.as_str(),
            estimate_tokens = self.size.estimate_tokens,
            context_tokens = self.size.context_tokens,
            basis = self.size.basis.as_str(),
            provider_tokens,
            "subagent first request"
        );
    }
}

/// THE resume admission check (see the module doc): measures the resumed
/// run's first request and refuses it above `subagent.resume_refuse_tokens`.
/// `subject` names the worker being resumed; `tools` is the registry the run
/// will use, `None` for the standard worker registry.
pub(crate) async fn admit_resume(
    ctx: &ToolContext,
    model: &str,
    agent_prompt: &str,
    tools: Option<&ToolRegistry>,
    messages: &[SharedMessage],
    subject: &str,
) -> Result<Admitted> {
    let config = crate::config::load_config();
    note_limit_clamp(config.subagent_context_clamp);
    let size = measure(ctx, &config, model, agent_prompt, tools, messages).await;
    let admission = decide(
        Kind::Resume,
        size,
        Limits::from_config(&config.subagent),
        subject,
        || last_report_excerpt(messages),
    );
    trace(Kind::Resume, subject, model, &admission);
    match admission {
        Admission::Admitted(admitted) => Ok(admitted),
        Admission::Refused { message, .. } => Err(RuntimeError::Tool(message)),
    }
}

/// Start and oneshot: measured and reported, never refused. Called before
/// the caller allocates an id or reserves anything, so a turn dropped at
/// this await leaks nothing; the model is therefore the one
/// `resolve_and_authorize` will pick, the requested id or the foreground.
pub(crate) async fn admit_fresh(
    kind: Fresh,
    ctx: &ToolContext,
    requested_model: Option<&str>,
    agent_prompt: &str,
    messages: &[SharedMessage],
    subject: &str,
) -> Admitted {
    let model = match requested_model {
        Some(model) => model.to_owned(),
        None => ctx
            .capabilities
            .orchestration
            .as_ref()
            .map(|policy| policy.foreground_model())
            .unwrap_or_default(),
    };
    let model = model.as_str();
    let config = crate::config::load_config();
    note_limit_clamp(config.subagent_context_clamp);
    let size = measure(ctx, &config, model, agent_prompt, None, messages).await;
    let admission = decide(
        kind.into(),
        size,
        Limits::from_config(&config.subagent),
        subject,
        || None,
    );
    trace(kind.into(), subject, model, &admission);
    match admission {
        Admission::Admitted(admitted) => admitted,
        // `decide` never refuses a fresh kind; keep the text if it ever did.
        Admission::Refused { size, message } => Admitted {
            size,
            warning: Some(message),
        },
    }
}

/// How many distinct clamped `(warn, refuse)` pairs one process logs.
const CLAMP_NOTES_MAX: usize = 32;

/// The config parser lowers a warn line above the refuse line to the refuse
/// line. Admission re-reads the config on every call, so a mid-session edit
/// would clamp silently: log it here, once per distinct pair per process,
/// with the two key names and numbers only.
fn note_limit_clamp(clamp: Option<(u64, u64)>) {
    static NOTED: std::sync::Mutex<Vec<(u64, u64)>> = std::sync::Mutex::new(Vec::new());
    let Some((warn, refuse)) = clamp else {
        return;
    };
    {
        let mut noted = NOTED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if noted.contains(&(warn, refuse)) || noted.len() >= CLAMP_NOTES_MAX {
            return;
        }
        noted.push((warn, refuse));
    }
    tracing::warn!(
        target: LOG_TARGET,
        context_warn_tokens = warn,
        resume_refuse_tokens = refuse,
        "subagent.context_warn_tokens is above subagent.resume_refuse_tokens; using the refuse line for both"
    );
}

async fn measure(
    ctx: &ToolContext,
    config: &crate::config::SynapsConfig,
    model: &str,
    agent_prompt: &str,
    tools: Option<&ToolRegistry>,
    messages: &[SharedMessage],
) -> RequestSize {
    let request = worker_request(ctx, config, agent_prompt, tools).await;
    size_from(
        estimate_tokens(model, &request, messages),
        calibration(ctx, model),
    )
}

/// Pure decision. Warn at `>= warn`; refuse a resume at `> refuse`. The
/// report excerpt is only built for a refusal.
pub(crate) fn decide(
    kind: Kind,
    size: RequestSize,
    limits: Limits,
    subject: &str,
    last_report: impl FnOnce() -> Option<String>,
) -> Admission {
    let tokens = size.context_tokens;
    if kind == Kind::Resume {
        if let Some(refuse) = limits.refuse_tokens.filter(|refuse| tokens > *refuse) {
            let report = match last_report() {
                Some(text) => format!(
                    "Its last report (at most the last {} characters):\n{text}",
                    group(REPORT_EXCERPT_CHARS as u64)
                ),
                None => "It has no report text to quote; build the brief from its task.".into(),
            };
            let message = format!(
                "{REFUSAL_PREFIX} resuming {subject} would send about {} tokens on every \
                 request ({}; limit {}, config subagent.resume_refuse_tokens). Start a fresh \
                 worker with subagent_start and a short brief built from its last report \
                 instead. {report}",
                group(tokens),
                size.basis.describe(),
                group(refuse),
            );
            return Admission::Refused { size, message };
        }
    }
    let warning = limits
        .warn_tokens
        .filter(|warn| tokens >= *warn)
        .map(|warn| {
            let measured = format!(
                "about {} tokens ({}; warn line {}, config subagent.context_warn_tokens)",
                group(tokens),
                size.basis.describe(),
                group(warn),
            );
            match kind {
                Kind::Resume => format!(
                    "this resume re-sends {measured} on every request. Resume only if the job \
                     is nearly done; otherwise start a fresh worker from a short brief built \
                     from its last report."
                ),
                Kind::Start | Kind::Oneshot => {
                    let mut text = format!(
                        "every request of this worker re-sends at least {measured}. A shorter \
                         task or agent prompt keeps it smaller."
                    );
                    if let Some(refuse) = limits.refuse_tokens.filter(|refuse| tokens > *refuse) {
                        text.push_str(&format!(
                            " It is already above subagent.resume_refuse_tokens ({}), so it \
                             cannot be resumed; collect its report and start fresh if more \
                             work is needed.",
                            group(refuse)
                        ));
                    }
                    text
                }
            }
        });
    Admission::Admitted(Admitted { size, warning })
}

/// The worker's system prompt and exposed tool schemas, as the spawn
/// thread builds them (`spawn_runtime` + `apply_subagent_runtime_policy` +
/// `compose_system_prompt`).
pub(crate) struct WorkerRequest {
    pub system_prompt: String,
    pub tools_schema: Arc<Vec<Value>>,
}

pub(crate) async fn worker_request(
    ctx: &ToolContext,
    config: &crate::config::SynapsConfig,
    agent_prompt: &str,
    tools: Option<&ToolRegistry>,
) -> WorkerRequest {
    // The worker inherits the caller's binding, else the configured one
    // (`apply_subagent_runtime_policy`); forum guidance and forum tools
    // follow `is_axel` (`compose_system_prompt`, `exposed_tools_schema`).
    let forum = match &ctx.capabilities.memory_backend {
        Some(binding) => binding.is_axel(),
        None => crate::memory_backend::MemoryBinding::from_config(&config.memory_backend).is_axel(),
    };
    let system_prompt = super::compose_system_prompt(agent_prompt.to_owned(), forum);
    let tools_schema = match tools {
        Some(registry) => exposed_schema(registry, forum),
        None => exposed_schema(&worker_registry().await, forum),
    };
    WorkerRequest {
        system_prompt,
        tools_schema,
    }
}

/// The registry `spawn_runtime` gives a worker.
async fn worker_registry() -> ToolRegistry {
    if !super::legacy_fresh_runtime() {
        if let Some(host) = crate::EngineHost::current() {
            return host.worker_registry().await;
        }
    }
    super::subagent_tools(&crate::config::load_config().disabled_tools).await
}

/// `Runtime::exposed_tools_schema` for a worker: no progressive disclosure,
/// a fresh continuation (context management as its default), forum tools
/// withheld unless the binding is Axel.
fn exposed_schema(registry: &ToolRegistry, forum: bool) -> Arc<Vec<Value>> {
    let session = crate::tools::activation::SessionId::parse("subagent-admission")
        .expect("a constant, valid session id");
    let set = crate::runtime::continuation::context_tool_set(
        session,
        registry.catalog(),
        false,
        ContinuationState::default().enabled(),
        forum,
    );
    registry
        .schema_withholding(&set)
        .unwrap_or_else(|| registry.tools_schema())
}

/// The conservative estimate of the whole request (no reserves).
pub(crate) fn estimate_tokens(
    model: &str,
    request: &WorkerRequest,
    messages: &[SharedMessage],
) -> u64 {
    context::assess(&context::ContextBudgetInputs {
        model,
        provider_window: 0,
        system_prompt: Some(&request.system_prompt),
        tools_schema: &request.tools_schema,
        messages,
        skill_contents: &[],
        memory_contents: &[],
        thinking_budget_tokens: 0,
        next_tool_result_bytes: 0,
        max_output_tokens: 0,
    })
    .used_tokens()
}

/// The calling session's calibration sample, if it is plausible and was
/// measured on the worker's model identity.
pub(crate) fn calibration(ctx: &ToolContext, model: &str) -> Option<TokenCalibration> {
    let state = ctx
        .capabilities
        .continuation
        .as_ref()?
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let sample = state.token_calibration?;
    let measured_on = state.calibration_model.as_deref()?;
    (sample.plausible() && same_model_identity(measured_on, model)).then_some(sample)
}

/// Same provider, endpoint, wire and wire model: a bare and a qualified id
/// of one model match; a different model or provider does not.
fn same_model_identity(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    use crate::runtime::openai::resolve_route;
    matches!((resolve_route(a), resolve_route(b)), (Some(x), Some(y)) if x == y)
}

pub(crate) fn size_from(estimate: u64, calibration: Option<TokenCalibration>) -> RequestSize {
    match calibration {
        Some(sample) => RequestSize {
            context_tokens: mul_div_ceil(estimate, sample.provider_tokens, sample.estimated_tokens),
            estimate_tokens: estimate,
            basis: Basis::Calibrated,
        },
        None => RequestSize {
            context_tokens: mul_div_ceil(estimate, UNCALIBRATED_RATIO.1, UNCALIBRATED_RATIO.0),
            estimate_tokens: estimate,
            basis: Basis::Estimate,
        },
    }
}

fn mul_div_ceil(value: u64, numerator: u64, denominator: u64) -> u64 {
    let scaled =
        (u128::from(value) * u128::from(numerator)).div_ceil(u128::from(denominator.max(1)));
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

/// The end of the worker's last report: the last text block of the last
/// assistant message that has text (a trailing tool_use turn is skipped),
/// control characters other than newline and tab removed, at most
/// [`REPORT_EXCERPT_CHARS`] characters, cut on a character boundary.
pub(crate) fn last_report_excerpt(messages: &[SharedMessage]) -> Option<String> {
    messages
        .iter()
        .rev()
        .filter(|message| message["role"] == "assistant")
        .find_map(|message| {
            let text = last_text_block(message)?;
            let clean: String = text
                .chars()
                .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
                .collect();
            let clean = clean.trim();
            (!clean.is_empty()).then(|| tail_chars(clean, REPORT_EXCERPT_CHARS))
        })
}

fn last_text_block(message: &Value) -> Option<&str> {
    match &message["content"] {
        Value::String(text) => Some(text),
        Value::Array(blocks) => blocks
            .iter()
            .rev()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .find(|text| !text.trim().is_empty()),
        _ => None,
    }
}

fn tail_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(max)).collect()
}

fn trace(kind: Kind, subject: &str, model: &str, admission: &Admission) {
    let (size, decision) = match admission {
        Admission::Admitted(admitted) if admitted.warning.is_some() => (admitted.size, "warn"),
        Admission::Admitted(admitted) => (admitted.size, "admit"),
        Admission::Refused { size, .. } => (*size, "refuse"),
    };
    tracing::info!(
        target: LOG_TARGET,
        kind = kind.as_str(),
        subject,
        model,
        context_tokens = size.context_tokens,
        estimate_tokens = size.estimate_tokens,
        basis = size.basis.as_str(),
        decision,
        "subagent context admission"
    );
}

/// `312000` -> `312,000`.
fn group(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_backend::MemoryBinding;
    use crate::runtime::subagent::{
        SubagentArchiveMeta, SubagentHandle, SubagentRegistry, SubagentState, SubagentStatus,
    };
    use crate::test_env::BaseDirGuard;
    use crate::tools::subagent::{SubagentResumeTool, SubagentStartTool, SubagentTool};
    use crate::tools::test_helpers::create_tool_context;
    use crate::tools::Tool;
    use agent_core::config::{MemoryBackendConfig, MemoryBackendKind};
    use agent_core::memory::store::ProjectScope;
    use serial_test::serial;
    use std::sync::{Mutex, RwLock};

    const MODEL: &str = "anthropic/claude-sonnet-4-6";
    const LIMITS: Limits = Limits {
        warn_tokens: Some(150_000),
        refuse_tokens: Some(250_000),
    };

    fn size(tokens: u64) -> RequestSize {
        RequestSize {
            context_tokens: tokens,
            estimate_tokens: tokens,
            basis: Basis::Calibrated,
        }
    }

    fn decide_at(kind: Kind, tokens: u64, limits: Limits) -> Admission {
        decide(kind, size(tokens), limits, "sa_7", || Some("REPORT".into()))
    }

    fn user(text: &str) -> SharedMessage {
        Arc::new(json!({"role": "user", "content": text}))
    }

    fn assistant(text: &str) -> SharedMessage {
        Arc::new(json!({"role": "assistant", "content": [{"type": "text", "text": text}]}))
    }

    fn request(system: &str, tools: Vec<Value>) -> WorkerRequest {
        WorkerRequest {
            system_prompt: system.into(),
            tools_schema: Arc::new(tools),
        }
    }

    fn axel(base: &BaseDirGuard) -> MemoryBinding {
        MemoryBinding::from_config(&MemoryBackendConfig {
            kind: MemoryBackendKind::Axel,
            executable: Some(base.path().join("must-not-launch")),
            brain: Some(base.path().join("must-not-create.r8")),
            user_scope: false,
        })
        .with_scope(ProjectScope::from_key(base.path(), "p1111111111111111").unwrap())
    }

    fn finished_handle(id: &str, archive: std::path::PathBuf) -> SubagentHandle {
        let state = Arc::new(RwLock::new(SubagentState::new()));
        {
            let mut s = state.write().unwrap();
            s.status = SubagentStatus::Completed;
            s.finished_at = Some(std::time::Instant::now());
            s.archive_path = Some(archive);
        }
        let (steer_tx, _steer_rx) = tokio::sync::mpsc::unbounded_channel();
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
        let (_result_tx, result_rx) = tokio::sync::oneshot::channel();
        SubagentHandle::new(
            id.into(),
            1,
            "worker".into(),
            "task".into(),
            MODEL.into(),
            "You are a worker.".into(),
            60,
            state,
            Some(steer_tx),
            Some(shutdown_tx),
            Some(result_rx),
        )
    }

    fn archive(dir: &std::path::Path, id: &str, history: &[SharedMessage]) -> std::path::PathBuf {
        let meta = SubagentArchiveMeta {
            agent_name: "worker".into(),
            model: MODEL.into(),
            system_prompt: "You are a worker.".into(),
            timeout_secs: 1,
            cwd: None,
            thinking: None,
            watch_usage: None,
        };
        super::super::archive::write_archive(dir, id, &meta, "completed", None, "report", history)
            .unwrap()
    }

    // ── T1: decide, boundaries ───────────────────────────────────────────

    #[test]
    fn t1_resume_is_refused_only_above_the_refuse_line_and_warned_from_the_warn_line() {
        for (tokens, refused, warned) in [
            (149_999, false, false),
            (150_000, false, true),
            (250_000, false, true),
            (250_001, true, false),
        ] {
            match decide_at(Kind::Resume, tokens, LIMITS) {
                Admission::Refused { size, message } => {
                    assert!(refused, "{tokens} must be admitted: {message}");
                    assert!(message.starts_with(REFUSAL_PREFIX), "{message}");
                    assert_eq!(size.context_tokens, tokens);
                }
                Admission::Admitted(admitted) => {
                    assert!(!refused, "{tokens} must be refused");
                    assert_eq!(admitted.warning.is_some(), warned, "{tokens}");
                    if let Some(warning) = admitted.warning {
                        assert!(warning.starts_with("this resume re-sends"), "{warning}");
                    }
                }
            }
        }
    }

    #[test]
    fn t1_start_and_oneshot_are_never_refused() {
        for kind in [Kind::Start, Kind::Oneshot] {
            for (tokens, warned) in [
                (149_999, false),
                (150_000, true),
                (250_000, true),
                (250_001, true),
                (5_000_000, true),
            ] {
                let Admission::Admitted(admitted) = decide_at(kind, tokens, LIMITS) else {
                    panic!("{kind:?} at {tokens} was refused");
                };
                assert_eq!(admitted.warning.is_some(), warned, "{kind:?} {tokens}");
                let not_resumable = admitted
                    .warning
                    .as_deref()
                    .is_some_and(|w| w.contains("cannot be resumed"));
                assert_eq!(not_resumable, tokens > 250_000, "{kind:?} {tokens}");
            }
        }
    }

    #[test]
    fn t1_off_disables_each_line() {
        let off = Limits {
            warn_tokens: None,
            refuse_tokens: None,
        };
        for kind in [Kind::Start, Kind::Resume, Kind::Oneshot] {
            assert_eq!(
                decide_at(kind, 10_000_000, off),
                Admission::Admitted(Admitted {
                    size: size(10_000_000),
                    warning: None
                })
            );
        }
        let refuse_only = Limits {
            warn_tokens: None,
            refuse_tokens: Some(250_000),
        };
        assert!(matches!(
            decide_at(Kind::Resume, 250_001, refuse_only),
            Admission::Refused { .. }
        ));
        assert!(matches!(
            decide_at(Kind::Resume, 249_000, refuse_only),
            Admission::Admitted(Admitted { warning: None, .. })
        ));
        let warn_only = Limits {
            warn_tokens: Some(150_000),
            refuse_tokens: None,
        };
        let Admission::Admitted(admitted) = decide_at(Kind::Resume, 9_000_000, warn_only) else {
            panic!("refused with the refuse line off");
        };
        assert!(admitted.warning.is_some());
    }

    #[test]
    fn t1_the_refusal_names_the_worker_the_limit_the_key_and_quotes_the_report() {
        let Admission::Refused { message, .. } =
            decide(Kind::Resume, size(312_000), LIMITS, "sa_12", || {
                Some("all done: see the branch".into())
            })
        else {
            panic!("312,000 must be refused");
        };
        assert!(
            message.starts_with(
                "subagent_resume refused (context limit): resuming sa_12 would send about \
                 312,000 tokens on every request"
            ),
            "{message}"
        );
        for part in [
            "calibrated estimate",
            "limit 250,000",
            "config subagent.resume_refuse_tokens",
            "subagent_start",
            "short brief built from its last report",
            "last 2,000 characters",
            "all done: see the branch",
        ] {
            assert!(message.contains(part), "missing {part:?}: {message}");
        }
        let mut built = false;
        let _ = decide(Kind::Resume, size(200_000), LIMITS, "sa_12", || {
            built = true;
            None
        });
        assert!(!built, "the excerpt is only built for a refusal");
        let Admission::Refused { message, .. } =
            decide(Kind::Resume, size(300_000), LIMITS, "sa_12", || None)
        else {
            panic!("300,000 must be refused");
        };
        assert!(message.contains("no report text"), "{message}");
    }

    // ── T2: calibration ──────────────────────────────────────────────────

    fn ctx_with_sample(measured_on: Option<&str>, provider: u64, estimated: u64) -> ToolContext {
        let mut ctx = create_tool_context();
        let mut state = ContinuationState::default();
        state.token_calibration = Some(TokenCalibration {
            provider_tokens: provider,
            estimated_tokens: estimated,
        });
        state.calibration_model = measured_on.map(str::to_owned);
        ctx.capabilities.continuation = Some(Arc::new(Mutex::new(state)));
        ctx
    }

    #[test]
    fn t2_a_plausible_sample_on_the_same_model_calibrates() {
        let ctx = ctx_with_sample(Some(MODEL), 100_000, 128_000);
        let sample = calibration(&ctx, MODEL).expect("same model, ratio 1.28");
        assert_eq!(
            size_from(320_000, Some(sample)),
            RequestSize {
                context_tokens: 250_000,
                estimate_tokens: 320_000,
                basis: Basis::Calibrated,
            }
        );
        // A bare and a qualified id of one model are one identity.
        let ctx = ctx_with_sample(Some("claude-sonnet-4-6"), 100_000, 128_000);
        assert!(calibration(&ctx, MODEL).is_some());
    }

    #[test]
    fn t2_other_implausible_or_missing_samples_fall_back_to_the_estimate() {
        for (measured_on, provider, estimated, model) in [
            (Some(MODEL), 100_000, 128_000, "anthropic/claude-opus-5-5"),
            (Some(MODEL), 100_000, 128_000, "openai-codex/gpt-5.6-sol"),
            (Some(MODEL), 100_000, 300_000, MODEL),
            (Some(MODEL), 0, 128_000, MODEL),
            (None, 100_000, 128_000, MODEL),
        ] {
            let ctx = ctx_with_sample(measured_on, provider, estimated);
            assert!(
                calibration(&ctx, model).is_none(),
                "{measured_on:?} {provider}/{estimated} for {model}"
            );
        }
        assert!(calibration(&create_tool_context(), MODEL).is_none());
        // Uncalibrated: the estimate divided by 1.2, rounded up.
        assert_eq!(
            size_from(300_000, None),
            RequestSize {
                context_tokens: 250_000,
                estimate_tokens: 300_000,
                basis: Basis::Estimate,
            }
        );
        assert_eq!(size_from(300_001, None).context_tokens, 250_001);
        let Admission::Refused { message, .. } = decide(
            Kind::Resume,
            size_from(300_002, None),
            LIMITS,
            "sa_1",
            || None,
        ) else {
            panic!("estimate 300,002 must be refused");
        };
        assert!(message.contains("(estimate, may read high;"), "{message}");
        let Admission::Admitted(admitted) = decide(
            Kind::Resume,
            size_from(200_000, None),
            LIMITS,
            "sa_1",
            || None,
        ) else {
            panic!("estimate 200,000 must be admitted");
        };
        assert!(admitted
            .warning
            .unwrap()
            .contains("estimate, may read high"));
    }

    // ── T3: segments ─────────────────────────────────────────────────────

    #[test]
    fn t3_system_prompt_tools_and_history_each_count() {
        let tool = json!({"name": "t", "description": "d".repeat(2_500)});
        assert_eq!(estimate_tokens(MODEL, &request("", vec![]), &[]), 0);
        let system = estimate_tokens(MODEL, &request(&"s".repeat(2_500), vec![]), &[]);
        let tools = estimate_tokens(MODEL, &request("", vec![tool.clone()]), &[]);
        let history = estimate_tokens(MODEL, &request("", vec![]), &[user(&"h".repeat(2_500))]);
        assert_eq!(system, 1_000);
        assert!((1_000..1_100).contains(&tools), "{tools}");
        assert!((1_000..1_100).contains(&history), "{history}");
        let all = estimate_tokens(
            MODEL,
            &request(&"s".repeat(2_500), vec![tool]),
            &[user(&"h".repeat(2_500))],
        );
        assert_eq!(all, system + tools + history);
    }

    #[tokio::test]
    #[serial(synaps_base_dir)]
    async fn t3_forum_guidance_and_tools_count_only_on_axel() {
        let base = BaseDirGuard::new();
        let config = crate::config::load_config();
        let legacy = worker_request(&create_tool_context(), &config, "worker", None).await;
        let mut ctx = create_tool_context();
        ctx.capabilities.memory_backend = Some(axel(&base));
        let forum = worker_request(&ctx, &config, "worker", None).await;
        let has_forum_tool = |request: &WorkerRequest| {
            request
                .tools_schema
                .iter()
                .any(|tool| tool["name"] == "forum_post")
        };
        assert!(!legacy.system_prompt.contains("Project forum"));
        assert!(!has_forum_tool(&legacy));
        assert!(forum.system_prompt.contains("Project forum"));
        assert!(has_forum_tool(&forum));
        assert!(estimate_tokens(MODEL, &forum, &[]) > estimate_tokens(MODEL, &legacy, &[]));
        assert!(
            !legacy.tools_schema.is_empty(),
            "a worker's request carries its tool schemas"
        );
    }

    // ── T4: images ───────────────────────────────────────────────────────

    #[test]
    fn t4_a_3mb_image_costs_the_flat_image_charge_not_its_bytes() {
        let data = "A".repeat(3 * 1024 * 1024);
        let messages = vec![
            user("look at the plate"),
            Arc::new(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "read", "input": {"path": "plate.png"}}
            ]})),
            Arc::new(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": data}}
                ]}
            ]})),
        ];
        let tokens = estimate_tokens(MODEL, &request("", vec![]), &messages);
        assert!(
            (context::IMAGE_TOKEN_ESTIMATE..context::IMAGE_TOKEN_ESTIMATE + 200).contains(&tokens),
            "{tokens}"
        );
    }

    // ── T5: refusal before side effects; the config is read on every call ─

    #[tokio::test]
    #[serial(synaps_base_dir)]
    async fn t5_an_oversized_resume_is_refused_before_any_side_effect() {
        let _base = BaseDirGuard::new();
        // ~820k ASCII chars: ~328k estimated, ~273k after the 1.2 divisor.
        let report = format!("{}\u{7}\r\nFINAL TAIL", "r".repeat(820_000));
        // Under the archive root, in this process's directory and named as
        // the writer names it: resume admission locates a handle's archive
        // by the store's own path forms (the port's strict paths).
        let path = archive(
            &super::super::archive::process_dir(),
            "sa_51",
            &[user("build it"), assistant(&report)],
        );
        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        registry
            .lock()
            .unwrap()
            .register(finished_handle("sa_51", path));
        let mut ctx = create_tool_context();
        let orchestration = ctx.capabilities.orchestration.clone().unwrap();
        let gate_before = orchestration.completion_gate();
        let descendants_before = orchestration.delegation_descendants();
        let active_before = registry.lock().unwrap().list_active().len();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.channels.tx_events = Some(tx);
        ctx.capabilities.subagent_registry = Some(registry.clone());

        let error = SubagentResumeTool
            .execute(
                json!({"handle_id": "sa_51", "instructions": "one more pass"}),
                ctx,
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains(REFUSAL_PREFIX), "{error}");
        for part in [
            "resuming sa_51",
            "(estimate, may read high; limit 250,000",
            "subagent.resume_refuse_tokens",
        ] {
            assert!(error.contains(part), "missing {part:?}");
        }
        let excerpt = error
            .split_once("characters):\n")
            .map(|(_, tail)| tail)
            .expect("an excerpt");
        assert!(excerpt.chars().count() <= REPORT_EXCERPT_CHARS);
        assert!(
            excerpt.ends_with("rrr\nFINAL TAIL"),
            "control characters stripped"
        );
        assert_eq!(orchestration.completion_gate(), gate_before);
        assert_eq!(orchestration.delegation_descendants(), descendants_before);
        assert_eq!(registry.lock().unwrap().list_active().len(), active_before);
        assert!(rx.try_recv().is_err(), "no SubagentStart event");
    }

    #[tokio::test]
    #[serial(synaps_base_dir)]
    async fn t5_the_config_lines_are_read_on_every_call() {
        let base = BaseDirGuard::new();
        let ctx = create_tool_context();
        let messages = vec![user("task"), assistant("a short report"), user("next")];
        let admitted = admit_resume(&ctx, MODEL, "worker", None, &messages, "sa_9")
            .await
            .expect("a small resume is admitted under the defaults");
        assert!(admitted.warning.is_none());
        assert_eq!(admitted.size.basis, Basis::Estimate);

        std::fs::write(
            base.path().join("config"),
            "subagent.resume_refuse_tokens = 100\n",
        )
        .unwrap();
        let error = admit_resume(&ctx, MODEL, "worker", None, &messages, "sa_9")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("limit 100, config"), "{error}");
        assert!(error.ends_with("a short report"), "{error}");

        std::fs::write(
            base.path().join("config"),
            "subagent.resume_refuse_tokens = off\nsubagent.context_warn_tokens = 100\n",
        )
        .unwrap();
        let admitted = admit_resume(&ctx, MODEL, "worker", None, &messages, "sa_9")
            .await
            .expect("the refuse line is off");
        assert!(admitted
            .warning
            .unwrap()
            .starts_with("this resume re-sends about"));
    }

    #[test]
    fn t5_the_excerpt_is_the_last_text_of_the_last_assistant_message_with_text() {
        let messages = vec![
            user("task"),
            Arc::new(json!({"role": "assistant", "content": [
                {"type": "text", "text": "early"},
                {"type": "text", "text": "final report\u{1b}[31m\r\n\tline"}
            ]})),
            Arc::new(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "t", "name": "bash", "input": {}}
            ]})),
            user("instructions"),
        ];
        assert_eq!(
            last_report_excerpt(&messages).as_deref(),
            Some("final report[31m\n\tline")
        );
        let long = format!("{}end", "é".repeat(3_000));
        let excerpt =
            last_report_excerpt(&[Arc::new(json!({"role": "assistant", "content": long}))])
                .unwrap();
        assert_eq!(excerpt.chars().count(), REPORT_EXCERPT_CHARS);
        assert!(excerpt.ends_with("éend"));
        assert!(last_report_excerpt(&[user("only a task")]).is_none());
    }

    // ── T6: parity with the worker runtime ───────────────────────────────

    #[tokio::test]
    #[serial(synaps_base_dir)]
    async fn t6_admission_measures_what_the_worker_runtime_would_send() {
        let base = BaseDirGuard::new();
        std::fs::write(
            base.path().join("subagent-preamble.md"),
            "Preamble for every worker.\n",
        )
        .unwrap();
        let agent_prompt = "You are a careful worker.";
        let history: Vec<SharedMessage> = vec![
            user("render the plate"),
            Arc::new(json!({"role": "assistant", "content": [
                {"type": "text", "text": "Rendering."},
                {"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "render --all"}}
            ]})),
            Arc::new(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "rendered 300 frames"}
            ]})),
            assistant("Done: 300 frames, no errors."),
        ];
        let messages = super::super::archive::resume_messages(
            history.iter().map(|m| m.as_ref().clone()).collect(),
            "check frame 12",
        );
        let config = crate::config::load_config();
        let mut sizes = Vec::new();
        for binding in [None, Some(axel(&base))] {
            let mut ctx = create_tool_context();
            ctx.capabilities.memory_backend = binding.clone();
            let request = worker_request(&ctx, &config, agent_prompt, None).await;
            let admitted = estimate_tokens(MODEL, &request, &messages);

            // The worker runtime, built the way the spawn thread builds it.
            let mut runtime = super::super::spawn_runtime().await.unwrap();
            super::super::apply_subagent_runtime_policy(
                &mut runtime,
                &config,
                binding.as_ref(),
                None,
                None,
                &super::super::WorkerWorkspace::default(),
            );
            runtime.set_system_prompt(super::super::compose_system_prompt(
                agent_prompt.to_owned(),
                runtime.memory_backend_is_axel(),
            ));
            runtime.set_model(MODEL.to_owned());
            let worker = runtime.assess_context(&messages).await.used_tokens();
            assert_eq!(admitted, worker, "forum {}", binding.is_some());
            sizes.push(admitted);
        }
        assert!(
            sizes[1] > sizes[0],
            "forum guidance and tools count on Axel"
        );
    }

    // ── T7: what the caller sees ─────────────────────────────────────────

    #[test]
    fn t7_reply_fields_sit_next_to_the_existing_keys() {
        let mut reply = json!({"handle_id": "sa_3", "warning": null, "status": "running"});
        Admitted {
            size: RequestSize {
                context_tokens: 160_000,
                estimate_tokens: 192_000,
                basis: Basis::Estimate,
            },
            warning: Some("w".into()),
        }
        .add_reply_fields(&mut reply);
        assert_eq!(reply["context_tokens"], 160_000);
        assert_eq!(reply["context_basis"], "estimate");
        assert_eq!(reply["context_warning"], "w");
        assert!(reply["warning"].is_null(), "the old key is untouched");
        assert_eq!(reply["status"], "running");

        let quiet = Admitted {
            size: size(1_000),
            warning: None,
        };
        let mut reply = json!({"handle_id": "sa_4"});
        quiet.add_reply_fields(&mut reply);
        assert_eq!(reply["context_basis"], "calibrated");
        assert!(reply.get("context_warning").is_none());
        assert_eq!(
            quiet.with_notice("[subagent:x] ok".into()),
            "[subagent:x] ok"
        );
        assert_eq!(quiet.log_line(), "Context: about 1,000 tokens (calibrated)");
        let loud = Admitted {
            size: size(1_000),
            warning: Some("big".into()),
        };
        assert_eq!(
            loud.with_notice("[subagent:x] ok".into()),
            "[subagent:x] ok\n\n[subagent context warning] big"
        );
    }

    /// Join the worker thread a test spawned, so the scratch base outlives it:
    /// its log and any credential lookup stay in the scratch dir.
    async fn wait_for_worker(registry: &Arc<Mutex<SubagentRegistry>>, reply: &Value) {
        let handle_id = reply["handle_id"].as_str().expect("a handle id");
        let thread = registry
            .lock()
            .unwrap()
            .get_mut(handle_id)
            .and_then(|handle| handle.thread_handle.take())
            .expect("the spawned worker's thread");
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            tokio::task::spawn_blocking(move || thread.join()),
        )
        .await
        .expect("the worker ends after its 1 s timeout")
        .unwrap()
        .expect("the worker thread does not panic");
    }

    async fn warn_on_everything(base: &BaseDirGuard) {
        std::fs::write(
            base.path().join("config"),
            "subagent.context_warn_tokens = 100\n",
        )
        .unwrap();
    }

    #[tokio::test]
    #[serial(synaps_base_dir)]
    async fn t7_start_reports_the_size_and_the_warning() {
        let base = BaseDirGuard::new();
        warn_on_everything(&base).await;
        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        let mut ctx = create_tool_context();
        ctx.capabilities.subagent_registry = Some(registry.clone());
        let reply = SubagentStartTool
            .execute(
                json!({"system_prompt": "You are a test worker.", "task": "Say ok",
                       "model": MODEL, "timeout": 1}),
                ctx,
            )
            .await
            .expect("dispatch");
        let reply: Value = serde_json::from_str(&reply).unwrap();
        wait_for_worker(&registry, &reply).await;
        assert!(reply["context_tokens"].as_u64().unwrap() > 100, "{reply}");
        assert_eq!(reply["context_basis"], "estimate");
        assert!(reply["context_warning"]
            .as_str()
            .unwrap()
            .starts_with("every request of this worker"));
    }

    #[tokio::test]
    #[serial(synaps_base_dir)]
    async fn t7_an_admitted_resume_reports_the_size_and_the_warning() {
        let base = BaseDirGuard::new();
        warn_on_everything(&base).await;
        // Under the archive root, as in t5 (the port's strict paths).
        let path = archive(
            &super::super::archive::process_dir(),
            "sa_52",
            &[user("task"), assistant("report")],
        );
        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        registry
            .lock()
            .unwrap()
            .register(finished_handle("sa_52", path));
        let mut ctx = create_tool_context();
        ctx.capabilities.subagent_registry = Some(registry.clone());
        let reply = SubagentResumeTool
            .execute(
                json!({"handle_id": "sa_52", "instructions": "again", "timeout": 1}),
                ctx,
            )
            .await
            .expect("admitted");
        let reply: Value = serde_json::from_str(&reply).unwrap();
        wait_for_worker(&registry, &reply).await;
        assert!(reply["context_tokens"].as_u64().unwrap() > 100, "{reply}");
        assert_eq!(reply["context_basis"], "estimate");
        assert!(reply["context_warning"]
            .as_str()
            .unwrap()
            .starts_with("this resume re-sends"));
        assert!(
            reply.get("warning").is_some(),
            "the timeout warning key stays"
        );
    }

    #[tokio::test]
    #[serial(synaps_base_dir)]
    async fn t7_a_oneshot_appends_the_warning_and_logs_the_size() {
        let base = BaseDirGuard::new();
        warn_on_everything(&base).await;
        let text = SubagentTool
            .execute(
                json!({"system_prompt": "You are a test worker.", "task": "Say ok",
                       "model": MODEL, "timeout": 1}),
                create_tool_context(),
            )
            .await
            .expect("a oneshot returns text");
        assert!(
            text.contains("\n\n[subagent context warning] every request of this worker"),
            "{text}"
        );
        let logs = base.path().join("logs").join("subagents");
        let logged = std::fs::read_dir(&logs)
            .unwrap()
            .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
            .any(|log| log.contains("Context: about "));
        assert!(logged, "the oneshot log records the size");
    }

    type Fields = std::collections::BTreeMap<String, String>;

    /// [`LOG_TARGET`] events captured on this thread: (message, fields).
    #[derive(Clone, Default)]
    struct AdmissionEvents(Arc<Mutex<Vec<(String, Fields)>>>);

    struct EventFields<'a>(&'a mut Fields);

    impl tracing::field::Visit for EventFields<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().into(), format!("{value:?}"));
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.insert(field.name().into(), value.into());
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for AdmissionEvents {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().target() != LOG_TARGET {
                return;
            }
            let mut fields = Fields::new();
            event.record(&mut EventFields(&mut fields));
            let message = fields.remove("message").unwrap_or_default();
            self.0.lock().unwrap().push((message, fields));
        }
    }

    impl AdmissionEvents {
        fn install() -> (Self, tracing::subscriber::DefaultGuard) {
            use tracing_subscriber::layer::SubscriberExt;
            let events = Self::default();
            let guard = tracing::subscriber::set_default(
                tracing_subscriber::registry().with(events.clone()),
            );
            (events, guard)
        }

        fn first_requests(&self) -> Vec<Fields> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|(message, _)| message == "subagent first request")
                .map(|(_, fields)| fields.clone())
                .collect()
        }

        fn clamps(&self) -> Vec<Fields> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|(message, _)| message.starts_with("subagent.context_warn_tokens is above"))
                .map(|(_, fields)| fields.clone())
                .collect()
        }
    }

    fn admitted_at(context_tokens: u64, estimate_tokens: u64) -> Admitted {
        Admitted {
            size: RequestSize {
                context_tokens,
                estimate_tokens,
                basis: Basis::Estimate,
            },
            warning: Some("a warning text that must not be logged".into()),
        }
    }

    fn usage(input: u64, cache_read: u64, cache_creation: u64) -> crate::StreamEvent {
        crate::StreamEvent::Session(crate::SessionEvent::Usage {
            input_tokens: input,
            output_tokens: 99,
            cache_read_input_tokens: cache_read,
            cache_creation_input_tokens: cache_creation,
            cache_creation_5m: None,
            cache_creation_1h: None,
            model: None,
        })
    }

    fn worker_drive(first_request: FirstRequest) -> super::super::drive::WorkerDrive {
        super::super::drive::WorkerDrive {
            state: Arc::new(RwLock::new(SubagentState::new())),
            tx_events: None,
            subagent_id: 7,
            label: "inline".into(),
            model: MODEL.into(),
            timeout_secs: 60,
            run_clock: None,
            cancel: crate::CancellationToken::new(),
            task: "SECRET TASK TEXT".into(),
            first_request: Some(first_request),
            lease: None,
            lease_binds_rounds: false,
            shape: super::super::drive::DriveShape::Reactive,
        }
    }

    #[tokio::test]
    async fn t8_a_run_logs_its_first_request_once_with_metadata_only() {
        let (events, _guard) = AdmissionEvents::install();
        let first = admitted_at(5_000, 6_000).first_request(Kind::Start, "sa_7", MODEL);
        let stream = vec![
            crate::StreamEvent::Llm(crate::LlmEvent::ResponseStart),
            crate::StreamEvent::Llm(crate::LlmEvent::Text("SECRET REPLY TEXT".into())),
            usage(1_000, 3_000, 500),
            crate::StreamEvent::Llm(crate::LlmEvent::ResponseStart),
            usage(7, 8, 9),
            usage(10, 11, 12),
            crate::StreamEvent::Session(crate::SessionEvent::Done),
        ];
        worker_drive(first)
            .run(Box::pin(futures::stream::iter(stream)))
            .await
            .unwrap();

        let lines = events.first_requests();
        assert_eq!(lines.len(), 1, "one line per run: {lines:?}");
        let line = &lines[0];
        let keys: Vec<&str> = line.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "basis",
                "context_tokens",
                "estimate_tokens",
                "handle_id",
                "kind",
                "model",
                "provider_tokens"
            ],
            "metadata only"
        );
        assert_eq!(line["handle_id"], "sa_7");
        assert_eq!(line["kind"], "start");
        assert_eq!(line["model"], MODEL);
        assert_eq!(line["estimate_tokens"], "6000");
        assert_eq!(line["context_tokens"], "5000");
        assert_eq!(line["basis"], "estimate");
        assert_eq!(
            line["provider_tokens"], "4500",
            "input + cache read + cache creation"
        );
        for value in line.values() {
            assert!(
                !value.contains("SECRET") && !value.contains("warning text"),
                "{value}"
            );
        }
    }

    #[tokio::test]
    async fn t8_a_run_that_never_reaches_its_first_request_logs_nothing() {
        let (events, _guard) = AdmissionEvents::install();
        // The provider refuses the first request: an error, no Usage.
        let failed = vec![crate::StreamEvent::Session(crate::SessionEvent::Error(
            agent_core::TurnError::interrupted_after_side_effect("c1"),
        ))];
        let first = admitted_at(5_000, 6_000).first_request(Kind::Resume, "sa_8", MODEL);
        assert!(worker_drive(first)
            .run(Box::pin(futures::stream::iter(failed)))
            .await
            .is_err());
        // The stream ends before any request.
        let first = admitted_at(5_000, 6_000).first_request(Kind::Start, "sa_9", MODEL);
        worker_drive(first)
            .run(Box::pin(futures::stream::iter(Vec::new())))
            .await
            .unwrap();
        assert!(
            events.first_requests().is_empty(),
            "{:?}",
            events.first_requests()
        );
    }

    #[test]
    fn t8_the_probe_logs_only_its_first_usage() {
        let (events, _guard) = AdmissionEvents::install();
        let first = admitted_at(5_000, 6_000).first_request(Kind::Oneshot, "sa_10", MODEL);
        first.on_usage(100, 0, 0);
        first.on_usage(200, 0, 0);
        let lines = events.first_requests();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["kind"], "oneshot");
        assert_eq!(lines[0]["provider_tokens"], "100");
    }

    /// Every admission line reaches synaps.log. The subscriber filters with
    /// the daemon's own filter, `default_env_filter(None)` (RUST_LOG unset),
    /// and each line passes at its level. The controls are dropped: the old
    /// bare `subagent_admission` target and an unrelated one.
    #[test]
    fn admission_lines_pass_the_daemon_log_filter() {
        use tracing_subscriber::layer::SubscriberExt;

        /// (target, level, message) of every event the filter let through.
        #[derive(Clone, Default)]
        struct Passed(Arc<Mutex<Vec<(String, tracing::Level, String)>>>);

        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Passed {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut fields = Fields::new();
                event.record(&mut EventFields(&mut fields));
                self.0.lock().unwrap().push((
                    event.metadata().target().to_owned(),
                    *event.metadata().level(),
                    fields.remove("message").unwrap_or_default(),
                ));
            }
        }

        let passed = Passed::default();
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::registry()
                .with(crate::logging::default_env_filter(None))
                .with(passed.clone()),
        );
        admitted_at(5_000, 6_000)
            .first_request(Kind::Start, "sa_31", MODEL)
            .on_usage(100, 0, 0);
        note_limit_clamp(Some((910_007, 910_003)));
        trace(
            Kind::Resume,
            "sa_31",
            MODEL,
            &Admission::Admitted(admitted_at(5_000, 6_000)),
        );
        tracing::warn!(target: "subagent_admission", "control: the old bare target");
        tracing::warn!(target: "some_dependency", "control: an unrelated target");

        let passed = passed.0.lock().unwrap().clone();
        let has = |level: tracing::Level, message: &str| {
            passed
                .iter()
                .any(|(t, l, m)| t == LOG_TARGET && *l == level && m.starts_with(message))
        };
        assert!(
            has(tracing::Level::INFO, "subagent first request"),
            "{passed:?}"
        );
        assert!(
            has(
                tracing::Level::WARN,
                "subagent.context_warn_tokens is above"
            ),
            "{passed:?}"
        );
        assert!(
            has(tracing::Level::INFO, "subagent context admission"),
            "{passed:?}"
        );
        assert!(
            passed.iter().all(|(t, _, _)| t == LOG_TARGET),
            "the controls are dropped: {passed:?}"
        );
    }

    /// A config edit that puts the warn line above the refuse line is
    /// clamped by the parser and logged at the admission seam once per
    /// distinct pair: the two key names and numbers, nothing else.
    #[tokio::test]
    #[serial(synaps_base_dir)]
    async fn t9_a_clamped_warn_line_is_logged_once_per_pair() {
        let base = BaseDirGuard::new();
        let ctx = create_tool_context();
        let (events, _guard) = AdmissionEvents::install();
        let messages = vec![user("task"), assistant("a short report"), user("next")];
        let pair = |warn: &str, refuse: &str| {
            Fields::from([
                ("context_warn_tokens".to_string(), warn.to_string()),
                ("resume_refuse_tokens".to_string(), refuse.to_string()),
            ])
        };
        admit_resume(&ctx, MODEL, "worker", None, &messages, "sa_9")
            .await
            .unwrap();
        assert!(events.clamps().is_empty(), "the defaults are not clamped");

        std::fs::write(
            base.path().join("config"),
            "subagent.context_warn_tokens = 900007\nsubagent.resume_refuse_tokens = 900003\n",
        )
        .unwrap();
        for _ in 0..2 {
            admit_resume(&ctx, MODEL, "worker", None, &messages, "sa_9")
                .await
                .unwrap();
        }
        let _ = admit_fresh(Fresh::Start, &ctx, Some(MODEL), "worker", &messages, "start").await;
        assert_eq!(events.clamps(), vec![pair("900007", "900003")]);

        // Another pair is logged once more.
        std::fs::write(
            base.path().join("config"),
            "subagent.context_warn_tokens = 900011\nsubagent.resume_refuse_tokens = 900003\n",
        )
        .unwrap();
        admit_resume(&ctx, MODEL, "worker", None, &messages, "sa_9")
            .await
            .unwrap();
        assert_eq!(
            events.clamps(),
            vec![pair("900007", "900003"), pair("900011", "900003")]
        );

        // A start or oneshot goes through the same seam.
        std::fs::write(
            base.path().join("config"),
            "subagent.context_warn_tokens = 900013\nsubagent.resume_refuse_tokens = 900003\n",
        )
        .unwrap();
        let _ = admit_fresh(Fresh::Oneshot, &ctx, Some(MODEL), "worker", &messages, "oneshot").await;
        assert_eq!(events.clamps().last(), Some(&pair("900013", "900003")));
        assert_eq!(events.clamps().len(), 3);
    }
}
