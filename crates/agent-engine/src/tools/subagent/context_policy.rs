//! The parent's context settings a worker inherits
//! (`docs/specs/subagent-context-rollover.md` §2, §7.2; slice 7, dark).
//!
//! A [`WorkerContextPolicy`] is a host-built snapshot of the parent
//! runtime's *live* context settings, taken when a worker is started or
//! resumed (or a oneshot runs). It reaches the worker tools through
//! [`ToolCapabilities`](crate::tools::ToolCapabilities); it is never a
//! model-supplied parameter and authorizes nothing by itself.
//!
//! It carries settings *as configured*, never what the parent resolved from
//! them: the summary-model setting (`auto` or one model), not the parent's
//! summarizer; the compaction model as set (`None` when unset), which the
//! worker resolves exactly as the parent would. Nothing reads a snapshot
//! until the worker policy is applied (only to a v2 run: a run with a
//! rollover plan whose launch made it one).

/// The parent's live context settings at a worker's spawn or resume.
/// The default is a parent with context management off and nothing set.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkerContextPolicy {
    /// The parent's continuation settings: mode, thresholds, reserve,
    /// finish rounds, summary size and timeout. The thresholds are
    /// re-validated for the worker's window before they apply.
    pub context_management: agent_core::config::ContextManagementConfig,
    /// `context_management.summary_model` as configured: `None` is `auto`,
    /// `Some` permits only that model. Never the parent's resolved
    /// summarizer.
    pub summary_model: Option<String>,
    /// The parent's summary-model setting was malformed: no summary, never
    /// a silent `auto`.
    pub summary_model_invalid: bool,
    /// The parent's `compaction_model` as configured (`None` when unset).
    pub compaction_model: Option<String>,
    /// Where summarization may run (remote, or local only).
    pub compaction_mode: agent_core::compaction::CompactionMode,
    /// Content classes excluded from remote summarization disclosure.
    pub compaction_exclusions: Vec<agent_core::compaction::ContentClass>,
    /// The parent's context-window override, with the model it was set
    /// for. A worker inherits it only for the same model identity.
    pub window_override: Option<InheritedWindow>,
    /// The operator's opt-out (`subagent.context_rollover`, §2): `Off`
    /// gives no worker of this parent rollover, whatever the caller asks.
    pub operator_rollover: agent_core::config::ContextRollover,
    /// The resume refusal line (`subagent.resume_refuse_tokens`; `None`:
    /// off): `subagent_resume` refuses a worker whose measured request is
    /// above it. A worker's rollover trigger is clamped below it
    /// ([`clamp_below_resume_refusal`]), so a run that ends near its
    /// trigger can still be resumed.
    pub resume_refuse_tokens: Option<u64>,
}

/// A context-window override and the model identity it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InheritedWindow {
    /// The override, in tokens.
    pub tokens: u64,
    /// The parent's model when the snapshot was taken.
    pub model: String,
}

/// What a worker runs with once its run is a v2 run: the parent's
/// settings resolved for the worker's model and window
/// ([`WorkerContextPolicy::resolve`]), applied to its runtime by
/// [`Runtime::apply_worker_context`](crate::Runtime::apply_worker_context).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WorkerContext {
    /// The continuation settings, valid for the worker's window.
    pub(crate) context_management: agent_core::config::ContextManagementConfig,
    pub(crate) summary_model: Option<String>,
    pub(crate) summary_model_invalid: bool,
    pub(crate) compaction_model: Option<String>,
    pub(crate) compaction_mode: agent_core::compaction::CompactionMode,
    pub(crate) compaction_exclusions: Vec<agent_core::compaction::ContentClass>,
    /// The parent's window override, when it was set for the same model
    /// identity as the worker's.
    pub(crate) context_window: Option<u64>,
    /// The caller-visible notice when the thresholds were reset.
    pub(crate) notice: Option<String>,
}

impl WorkerContextPolicy {
    /// The settings a worker on `model` runs with, or why it runs with
    /// rollover off (§2, §7.2).
    ///
    /// The window is the parent's override, clamped to the worker model's
    /// ceiling ([`effective_context_window`]), when it was set for the same
    /// model identity ([`same_model_identity`]), else the worker model's
    /// own. Thresholds that fit that window are kept. Thresholds that don't
    /// are reset to the window's defaults (pressure, rollover, target and
    /// summary size; the reserve, finish rounds and summary timeout stay),
    /// with a notice that says why. When even the defaults don't fit, the
    /// worker runs with rollover off. Then, with a resume refusal line, the
    /// rollover trigger is clamped below it ([`clamp_below_resume_refusal`];
    /// no notice). A worker's thresholds therefore
    /// always resolve for its window: never the unvalidated settings that
    /// the stream would treat as unbounded with no summary. The summary,
    /// compaction and disclosure settings are copied as configured; the
    /// worker resolves them itself ([`Runtime::apply_worker_context`]).
    ///
    /// [`Runtime::apply_worker_context`]: crate::Runtime::apply_worker_context
    /// [`effective_context_window`]: crate::models::effective_context_window
    pub(crate) fn resolve(&self, model: &str) -> Result<WorkerContext, String> {
        use agent_core::config::ContextManagementMode;
        if self.context_management.mode != ContextManagementMode::Auto {
            return Err("the parent's context management is not auto".to_owned());
        }
        let context_window = self
            .window_override
            .as_ref()
            .filter(|inherited| same_model_identity(&inherited.model, model))
            .map(|inherited| inherited.tokens);
        // The window the worker's runtime budgets with
        // (`Runtime::context_window`): never above what its model accepts.
        let window = crate::models::effective_context_window(model, context_window);
        let mut context_management = self.context_management;
        // The validator's reasons are fixed short sentences, so the notice
        // is one bounded plain line.
        let notice = match context_management.validate_for_window(window) {
            Ok(()) => None,
            Err(why) => {
                context_management.pressure_tokens = None;
                context_management.rollover_tokens = None;
                context_management.target_tokens = None;
                context_management.summary_tokens = None;
                context_management
                    .validate_for_window(window)
                    .map_err(|why| format!("{why} (window {window} tokens)"))?;
                Some(format!(
                    "rollover thresholds reset to this worker's defaults: {why} (window {window} tokens)"
                ))
            }
        };
        if let Some(refuse) = self.resume_refuse_tokens {
            clamp_below_resume_refusal(&mut context_management, window, refuse)?;
        }
        Ok(WorkerContext {
            context_management,
            summary_model: self.summary_model.clone(),
            summary_model_invalid: self.summary_model_invalid,
            compaction_model: self.compaction_model.clone(),
            compaction_mode: self.compaction_mode,
            compaction_exclusions: self.compaction_exclusions.clone(),
            context_window,
            notice,
        })
    }
}

/// Clamp `config`'s rollover trigger for `window` below the resume refusal
/// line `refuse` (§7.2, §12 item 9), so a worker that ends a run near its
/// trigger can still be resumed: the trigger is at most
/// `refuse - (1 + finish_rounds) * reserve_tokens`. The headroom holds the
/// round that crosses the trigger and the finish rounds allowed after it,
/// each bounded by the reserve. A trigger at or below the cap is kept.
/// Otherwise the trigger becomes the cap; a pressure threshold that is not
/// below it becomes 7/9 of it (the 140k:180k ratio of the defaults at and
/// below a 200k window, so the worker keeps an advisory band), an explicit
/// one below it stays; a target or summary size that no longer fits goes
/// back to its default. The result is validated again. When that fails, or
/// the headroom leaves no cap of at least two tokens, the worker runs with
/// rollover off and says why. A clamp gives no notice (a default, like the
/// window's own).
fn clamp_below_resume_refusal(
    config: &mut agent_core::config::ContextManagementConfig,
    window: u64,
    refuse: u64,
) -> Result<(), String> {
    use agent_core::core::context_policy::ContextThresholds;
    let headroom = u64::from(config.finish_rounds)
        .saturating_add(1)
        .saturating_mul(config.reserve_tokens);
    let cap = refuse.saturating_sub(headroom);
    if cap < 2 {
        return Err(format!(
            "the resume refusal line ({refuse} tokens) leaves no room for a rollover trigger \
             below its {headroom} tokens of headroom (window {window} tokens)"
        ));
    }
    let resolved = ContextThresholds::resolve(config, window)
        .map_err(|why| format!("{why} (window {window} tokens)"))?;
    if resolved.rollover_tokens <= cap {
        return Ok(());
    }
    config.rollover_tokens = Some(cap);
    let pressure = if resolved.pressure_tokens < cap {
        resolved.pressure_tokens
    } else {
        // 7/9 of the cap, rounded down, without overflow; at least 1.
        let pressure = cap / 9 * 7 + cap % 9 * 7 / 9;
        config.pressure_tokens = Some(pressure);
        pressure
    };
    if config.target_tokens.is_some_and(|target| target >= pressure) {
        config.target_tokens = None;
    }
    let target = ContextThresholds {
        pressure_tokens: pressure,
        rollover_tokens: cap,
    }
    .target_tokens(config);
    if config
        .summary_tokens
        .is_some_and(|summary| summary > 0 && summary >= target)
    {
        config.summary_tokens = None;
    }
    config
        .validate_for_window(window)
        .map_err(|why| format!("{why} (window {window} tokens)"))
}

/// Whether two model ids name the same model on the same provider and
/// endpoint: an extension-hosted id only itself, a routed id by its
/// resolved provider, endpoint and wire model (so `anthropic/claude-x` is
/// `claude-x`), an id no route resolves only itself.
pub(crate) fn same_model_identity(a: &str, b: &str) -> bool {
    use crate::extensions::providers::ProviderRegistry;
    let (a, b) = (a.trim(), b.trim());
    if a == b {
        return true;
    }
    if ProviderRegistry::parse_model_id(a).is_some() || ProviderRegistry::parse_model_id(b).is_some()
    {
        return false;
    }
    match (
        crate::runtime::openai::resolve_route(a),
        crate::runtime::openai::resolve_route(b),
    ) {
        (Some(a), Some(b)) => {
            a.provider == b.provider && a.endpoint == b.endpoint && a.model == b.model
        }
        _ => false,
    }
}

#[cfg(test)]
#[path = "context_policy_tests.rs"]
mod tests;
