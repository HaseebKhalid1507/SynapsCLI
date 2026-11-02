//! Metadata-only context-window telemetry, under the `synaps::context`
//! tracing target. Four events, all plain `key=value` fields so a log
//! reader can parse them:
//!
//! - `context rollover`: one per committed rollover.
//! - `context window settled`: the first provider-reported request of a
//!   window after a rollover, once per window.
//! - `context rollover deferred`: a rollover the host wanted but did not
//!   perform, once per reason per window plus at each further 50k tokens.
//! - `context rollover refused: …`: a rollover the host needed and could not
//!   perform, ending the turn (`admission`, `archive_failure`); the
//!   deferral's fields and rate limit.
//!
//! Never message content, titles, prompts, file paths or tool arguments: only
//! a session label (see [`SessionLabel`]), window numbers, the archive id,
//! counts, token numbers and fixed reason codes.

pub(crate) const TARGET: &str = "synaps::context";

/// A deferral reason is logged again in the same window only after the
/// footprint crosses a further multiple of this.
pub(crate) const DEFER_STEP_TOKENS: u64 = 50_000;

/// Why a wanted rollover did not happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeferReason {
    /// Workers are running or unreconciled.
    WorkersPending,
    /// The successor would not shrink by more than the minimum.
    UnproductiveNoShrink,
    /// The successor does not fit the safe request budget.
    Admission,
    /// The archive could not be written.
    ArchiveFailure,
    /// History retrieval tools are not admitted.
    RetrievalUnavailable,
    /// The frontend did not acknowledge the successor head.
    HeadNotAcknowledged,
}

impl DeferReason {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::WorkersPending => "workers_pending",
            Self::UnproductiveNoShrink => "unproductive_no_shrink",
            Self::Admission => "admission",
            Self::ArchiveFailure => "archive_failure",
            Self::RetrievalUnavailable => "retrieval_unavailable",
            Self::HeadNotAcknowledged => "head_not_acknowledged",
        }
    }
}

/// How a session appears in a log line. A logical session id is caller
/// supplied (it can be anything a frontend binds), so only generated-shaped
/// ids (ASCII, in a shape the runtime itself generates) are logged as-is;
/// any other id becomes an opaque reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SessionLabel {
    /// A generated-shaped id (a shape the runtime generates; provenance is
    /// not checked): `session=<id>`.
    Generated(String),
    /// `session_ref=<12 hex>`: 48 bits of a keyed hash of the id under a
    /// random per-process key. Process-local pseudonymous correlation:
    /// stable within one process, not across processes; not anonymity.
    Ref(String),
}

impl SessionLabel {
    pub(crate) fn of(id: &str) -> Self {
        if is_generated_session_id(id) {
            Self::Generated(id.to_owned())
        } else {
            Self::Ref(session_ref(id))
        }
    }

    fn generated(&self) -> Option<&str> {
        match self {
            Self::Generated(id) => Some(id),
            Self::Ref(_) => None,
        }
    }

    fn reference(&self) -> Option<&str> {
        match self {
            Self::Generated(_) => None,
            Self::Ref(r) => Some(r),
        }
    }
}

fn is_lower_hex(b: u8) -> bool {
    matches!(b, b'0'..=b'9' | b'a'..=b'f')
}

/// Canonical lowercase hyphenated UUID (`8-4-4-4-12`).
fn is_canonical_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => is_lower_hex(b),
        })
}

/// Shapes the runtime generates: a session (`%Y%m%d-%H%M%S-xxxx`, four hex
/// digits of a UUID), the unbound default (`ephemeral-<uuid>`), or a bare
/// canonical UUID.
pub(crate) fn is_generated_session_id(id: &str) -> bool {
    // Bytes only: a multi-byte character must never meet a fixed offset.
    let b = id.as_bytes();
    let session = b.len() == 20
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 15 => c == b'-',
            0..=14 => c.is_ascii_digit(),
            _ => is_lower_hex(c),
        });
    session || is_canonical_uuid(id) || id.strip_prefix("ephemeral-").is_some_and(is_canonical_uuid)
}

/// 12 hex digits of HMAC-SHA256(per-process random key, id).
fn session_ref(id: &str) -> String {
    use hmac::Mac as _;
    static KEY: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    let key = KEY.get_or_init(rand::random);
    let mut mac =
        hmac::Hmac::<sha2::Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(id.as_bytes());
    mac.finalize().into_bytes()[..6]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// What the first provider-reported request after a rollover is compared
/// with. Bound to the session binding (epoch) and model of the rollover.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SettleProbe {
    pub window: u64,
    pub successor_tokens: u64,
    pub target_tokens: u64,
    /// Provider calls in the window so far that returned or reported usage.
    pub rounds: u64,
    epoch: uuid::Uuid,
    model: String,
}

/// Per-session rate-limit and settle bookkeeping. Lives in the continuation
/// state, so a session reset clears it.
#[derive(Clone, Debug, Default)]
pub(crate) struct WindowTelemetry {
    /// Window the deferral marks belong to.
    window: u64,
    /// (reason, highest footprint step logged) in `window`.
    deferred: Vec<(DeferReason, u64)>,
    settle: Option<SettleProbe>,
}

impl WindowTelemetry {
    /// Whether a deferral should be logged now: the first time per reason per
    /// window, then each time the footprint reaches a further absolute
    /// multiple of 50k.
    pub(crate) fn should_log_deferral(
        &mut self,
        window: u64,
        reason: DeferReason,
        footprint_tokens: u64,
    ) -> bool {
        if self.window != window {
            self.window = window;
            self.deferred.clear();
        }
        let step = footprint_tokens / DEFER_STEP_TOKENS;
        match self.deferred.iter_mut().find(|(r, _)| *r == reason) {
            Some((_, logged)) if step <= *logged => false,
            Some((_, logged)) => {
                *logged = step;
                true
            }
            None => {
                self.deferred.push((reason, step));
                true
            }
        }
    }

    /// A rollover into `window` committed under binding `epoch` while
    /// `model` was selected; arm its settle probe. Called from the
    /// epoch-checked commit, under the same lock.
    pub(crate) fn rolled_over(
        &mut self,
        window: u64,
        epoch: uuid::Uuid,
        model: &str,
        successor_tokens: u64,
        target_tokens: u64,
    ) {
        self.window = window;
        self.deferred.clear();
        self.settle = Some(SettleProbe {
            window,
            successor_tokens,
            target_tokens,
            rounds: 0,
            epoch,
            model: model.to_owned(),
        });
    }

    /// Before a provider call: whether a probe for this window, binding and
    /// model is waiting. A probe made under another model is dropped (its
    /// tokenizer and estimate do not describe this request).
    pub(crate) fn watch(&mut self, window: u64, epoch: uuid::Uuid, model: &str) -> bool {
        match &self.settle {
            Some(p) if p.window == window && p.epoch == epoch && p.model == model => true,
            Some(_) => {
                self.settle = None;
                false
            }
            None => false,
        }
    }

    /// After a watched provider call, keyed by the window and binding it
    /// was watched under. `reported` is the context footprint of
    /// the first usage the provider reported during it (any attempt, even
    /// one that then failed); `returned` whether the call returned a
    /// response. A call that neither returned nor reported (canceled before
    /// sending, failed without usage) does not count. Returns the probe and
    /// the footprint once, on the first reported usage.
    pub(crate) fn provider_round(
        &mut self,
        window: u64,
        epoch: uuid::Uuid,
        model: &str,
        reported: Option<u64>,
        returned: bool,
    ) -> Option<(SettleProbe, u64)> {
        // A completion watched under another window or binding is stale:
        // whatever probe is stored now is newer, and is left alone.
        let probe = self
            .settle
            .as_mut()
            .filter(|p| p.window == window && p.epoch == epoch)?;
        if probe.model != model {
            // The current binding's model changed: its probe is void.
            self.settle = None;
            return None;
        }
        if reported.is_none() && !returned {
            return None;
        }
        probe.rounds = probe.rounds.saturating_add(1);
        let tokens = reported?;
        self.settle.take().map(|probe| (probe, tokens))
    }
}

/// After a provider call that was watched under `watched` (window, epoch):
/// count the round and, on the first reported usage, take the probe and the
/// session label for the settle event.
pub(crate) fn settle_after_call(
    state: &super::continuation::SharedContinuation,
    watched: (u64, uuid::Uuid),
    model: &str,
    reported: Option<u64>,
    returned: bool,
) -> Option<((SettleProbe, u64), SessionLabel)> {
    let (window, epoch) = watched;
    let mut s = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Checked before any telemetry mutation: a completion from an older
    // binding or window must not touch a newer probe.
    if s.epoch() != epoch || s.window != window {
        return None;
    }
    s.telemetry
        .provider_round(window, epoch, model, reported, returned)
        .map(|settled| (settled, SessionLabel::of(&s.logical_id)))
}

/// Fields of one `context rollover` event.
pub(crate) struct RolloverEvent<'a> {
    pub session: &'a SessionLabel,
    pub window_from: u64,
    pub window_to: u64,
    pub archive: &'a str,
    pub trigger: &'static str,
    pub footprint_tokens: u64,
    pub provider_context_tokens: Option<u64>,
    pub calibration_ratio: Option<f64>,
    pub target_tokens: u64,
    pub target_history_tokens: u64,
    pub history_budget_tokens: u64,
    /// `target_history_tokens` clamped below `history_budget_tokens`: what
    /// the successor was sized to.
    pub history_target_tokens: u64,
    pub overhead_tokens: u64,
    /// `None` (with the next two) when the successor was not measured.
    pub successor_tokens: Option<u64>,
    pub retained_messages: u64,
    pub dropped_messages: u64,
    pub must_keep_tokens: u64,
    /// Pending workers listed in the successor's manifest (0 when none).
    pub workers_carried: u64,
    pub continuation_tokens: Option<u64>,
    pub summary: &'static str,
    pub summary_reason: &'static str,
    pub summary_tokens: Option<u64>,
    /// What the summary attempt did; `None` when none was made.
    pub summary_attempt: Option<&'a super::rolling_summary::SummaryReport>,
}

/// Micro-dollars, rounded up (a bound is never understated).
fn microusd(usd: f64) -> Option<u64> {
    (usd.is_finite() && usd >= 0.0).then(|| (usd * 1_000_000.0).ceil() as u64)
}

impl RolloverEvent<'_> {
    pub(crate) fn emit(&self) {
        // `measured=false` only when the sizes are missing, so a measured
        // line keeps its stable shape.
        let unmeasured = self.successor_tokens.is_none().then_some(false);
        // Attempt fields exist only for what was attempted and measured:
        // never zero-filled.
        let attempt = self.summary_attempt;
        let path = attempt.map_or("none", |a| match a.path {
            super::rolling_summary::SummaryPath::None => "none",
            super::rolling_summary::SummaryPath::Warm => "warm",
            super::rolling_summary::SummaryPath::Cold => "cold",
        });
        let usage = attempt.and_then(|a| a.usage).unwrap_or_default();
        let summary_id = attempt
            .and_then(|a| a.id)
            .map(super::telemetry::summary_id);
        tracing::info!(
            target: TARGET,
            session = self.session.generated().map(tracing::field::display),
            session_ref = self.session.reference().map(tracing::field::display),
            window_from = self.window_from,
            window_to = self.window_to,
            archive = %self.archive,
            trigger = %self.trigger,
            footprint_tokens = self.footprint_tokens,
            provider_context_tokens = self.provider_context_tokens,
            calibration_ratio = self.calibration_ratio,
            target_tokens = self.target_tokens,
            target_history_tokens = self.target_history_tokens,
            history_budget_tokens = self.history_budget_tokens,
            history_target_tokens = self.history_target_tokens,
            overhead_tokens = self.overhead_tokens,
            successor_tokens = self.successor_tokens,
            retained_messages = self.retained_messages,
            dropped_messages = self.dropped_messages,
            must_keep_tokens = self.must_keep_tokens,
            workers_carried = self.workers_carried,
            continuation_tokens = self.continuation_tokens,
            summary = %self.summary,
            summary_reason = %self.summary_reason,
            summary_tokens = self.summary_tokens,
            summary_attempted_path = %path,
            summary_cold_reason = attempt
                .and_then(|a| a.cold_reason)
                .map(tracing::field::display),
            summary_id = summary_id.as_deref().map(tracing::field::display),
            summary_warm_miss = attempt
                .and_then(|a| a.warm_miss)
                .map(|miss| tracing::field::display(miss.as_str())),
            summary_terminal = attempt
                .and_then(|a| a.terminal)
                .map(tracing::field::display),
            summary_limit_tokens = attempt.and_then(|a| a.limit_tokens),
            summary_cache_age_s = attempt.and_then(|a| a.cache_age_s),
            summary_reserved_microusd = attempt.and_then(|a| a.reserved_usd).and_then(microusd),
            summary_settled_microusd = attempt.and_then(|a| a.settled_usd).and_then(microusd),
            summary_warm_settled_microusd = attempt
                .and_then(|a| a.warm_settled_usd)
                .and_then(microusd),
            summary_duration_ms = attempt.map(|a| a.duration_ms),
            summary_error_status = attempt
                .and_then(|a| a.error)
                .map(|f| tracing::field::display(f.class.label())),
            provider_summary_input_tokens = usage.input,
            provider_summary_output_tokens = usage.output,
            provider_summary_cache_read_tokens = usage.cache_read,
            provider_summary_cache_write_tokens = usage.cache_write,
            provider_summary_cache_write_5m_tokens = usage.cache_write_5m,
            provider_summary_cache_write_1h_tokens = usage.cache_write_1h,
            summary_cache_read_pct = usage.cache_read_pct(),
            measured = unmeasured,
            "context rollover"
        );
    }
}

pub(crate) fn emit_settled(
    session: &SessionLabel,
    probe: &SettleProbe,
    provider_context_tokens: u64,
    request_tokens: u64,
) {
    tracing::info!(
        target: TARGET,
        session = session.generated().map(tracing::field::display),
        session_ref = session.reference().map(tracing::field::display),
        window = probe.window,
        provider_context_tokens,
        request_tokens,
        successor_tokens = probe.successor_tokens,
        target_tokens = probe.target_tokens,
        rounds_since_rollover = probe.rounds,
        "context window settled"
    );
}

pub(crate) fn emit_deferred(
    session: &SessionLabel,
    window: u64,
    reason: DeferReason,
    workers: u64,
    footprint_tokens: u64,
    provider_context_tokens: Option<u64>,
) {
    tracing::info!(
        target: TARGET,
        session = session.generated().map(tracing::field::display),
        session_ref = session.reference().map(tracing::field::display),
        window,
        reason = %reason.code(),
        workers,
        footprint_tokens,
        provider_context_tokens,
        "context rollover deferred"
    );
}

/// A rollover the host needed and refused: unlike a deferral, the turn ends
/// with the error (history retained). Same fields as a deferral.
pub(crate) fn emit_refused(
    session: &SessionLabel,
    window: u64,
    reason: DeferReason,
    workers: u64,
    footprint_tokens: u64,
    provider_context_tokens: Option<u64>,
) {
    let what = match reason {
        DeferReason::Admission => "successor over the hard budget",
        DeferReason::ArchiveFailure => "archive commit failed",
        other => other.code(),
    };
    tracing::info!(
        target: TARGET,
        session = session.generated().map(tracing::field::display),
        session_ref = session.reference().map(tracing::field::display),
        window,
        reason = %reason.code(),
        workers,
        footprint_tokens,
        provider_context_tokens,
        "context rollover refused: {what}"
    );
}

/// The session label and window to log `reason` under, if the deferral
/// rate limit allows.
fn rate_limited(
    state: &super::continuation::SharedContinuation,
    reason: DeferReason,
    footprint_tokens: u64,
) -> Option<(SessionLabel, u64)> {
    let mut s = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let window = s.window;
    s.telemetry
        .should_log_deferral(window, reason, footprint_tokens)
        .then(|| (SessionLabel::of(&s.logical_id), window))
}

/// Log a deferral if its rate limit allows.
pub(crate) fn log_deferral(
    state: &super::continuation::SharedContinuation,
    reason: DeferReason,
    workers: u64,
    footprint_tokens: u64,
    provider_context_tokens: Option<u64>,
) {
    let Some((session, window)) = rate_limited(state, reason, footprint_tokens) else {
        return;
    };
    emit_deferred(
        &session,
        window,
        reason,
        workers,
        footprint_tokens,
        provider_context_tokens,
    );
}

/// Log a refusal under the same rate limit as a deferral.
pub(crate) fn log_refusal(
    state: &super::continuation::SharedContinuation,
    reason: DeferReason,
    workers: u64,
    footprint_tokens: u64,
    provider_context_tokens: Option<u64>,
) {
    let Some((session, window)) = rate_limited(state, reason, footprint_tokens) else {
        return;
    };
    emit_refused(
        &session,
        window,
        reason,
        workers,
        footprint_tokens,
        provider_context_tokens,
    );
}

/// The rollover trigger of the 1M entitlement fallback's rollover.
pub(crate) const NATIVE_WINDOW_TRIGGER: &str = "native_window";

/// The session fell back from the 1M window to its model's native window
/// after a request that carried the 1M beta was refused (see
/// `runtime::native_window`). Metadata only: the HTTP status, the vetted
/// class code and the two window sizes, never the provider's body.
pub(crate) fn emit_native_window_fallback(
    session: &SessionLabel,
    status: u16,
    class: &'static str,
    window_from_tokens: u64,
    window_to_tokens: u64,
) {
    tracing::info!(
        target: TARGET,
        session = session.generated().map(tracing::field::display),
        session_ref = session.reference().map(tracing::field::display),
        status,
        class,
        window_from_tokens,
        window_to_tokens,
        "context window fallback to native"
    );
}

/// [`emit_native_window_fallback`] under the session's label.
pub(crate) fn log_native_window_fallback(
    state: &super::continuation::SharedContinuation,
    status: u16,
    class: &'static str,
    window_from_tokens: u64,
    window_to_tokens: u64,
) {
    let session = SessionLabel::of(
        &state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .logical_id,
    );
    emit_native_window_fallback(
        &session,
        status,
        class,
        window_from_tokens,
        window_to_tokens,
    );
}

/// What made the host roll over: `hard_capacity`, `wall_clock`,
/// `phase_boundary`, `rollover_band`, `rollover_pending` or `other`.
pub(crate) fn rollover_trigger(
    time_checkpoint: bool,
    decision: &agent_core::core::context_policy::ContextAssessment,
) -> &'static str {
    use agent_core::core::context_policy::{ContextAction, ContextReason};
    if decision.action == ContextAction::HardStop {
        return "hard_capacity";
    }
    if time_checkpoint {
        return "wall_clock";
    }
    match decision.reason {
        ContextReason::TaskBoundary => "phase_boundary",
        ContextReason::RolloverThreshold | ContextReason::FinishAllowanceExhausted => {
            "rollover_band"
        }
        ContextReason::RolloverPending => "rollover_pending",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferral_is_once_per_reason_per_window_and_per_further_50k() {
        let mut t = WindowTelemetry::default();
        let w = DeferReason::WorkersPending;
        let u = DeferReason::UnproductiveNoShrink;
        assert!(t.should_log_deferral(1, w, 410_000));
        assert!(!t.should_log_deferral(1, w, 420_000));
        assert!(!t.should_log_deferral(1, w, 449_999));
        assert!(t.should_log_deferral(1, u, 420_000), "other reason");
        assert!(t.should_log_deferral(1, w, 450_000), "crossed a 50k step");
        assert!(!t.should_log_deferral(1, w, 460_000));
        assert!(!t.should_log_deferral(1, w, 300_000), "shrinking is quiet");
        assert!(t.should_log_deferral(1, w, 520_000), "two steps up");
        assert!(t.should_log_deferral(2, w, 300_000), "new window");
        assert!(!t.should_log_deferral(2, w, 310_000));
    }

    const M: &str = "claude-sonnet-4-6";

    fn armed(window: u64) -> (WindowTelemetry, uuid::Uuid) {
        let mut t = WindowTelemetry::default();
        let epoch = uuid::Uuid::new_v4();
        t.rolled_over(window, epoch, M, 90_000, 100_000);
        (t, epoch)
    }

    #[test]
    fn settle_fires_once_on_the_first_reported_usage() {
        let mut t = WindowTelemetry::default();
        let e = uuid::Uuid::new_v4();
        assert_eq!(
            t.provider_round(1, e, M, Some(5), true),
            None,
            "no rollover yet"
        );
        let (mut t, e) = armed(2);
        assert!(t.watch(2, e, M));
        assert_eq!(
            t.provider_round(2, e, M, None, true),
            None,
            "no usage reported"
        );
        let (probe, tokens) = t
            .provider_round(2, e, M, Some(97_000), false)
            .expect("settled");
        assert_eq!(
            (probe.rounds, tokens),
            (2, 97_000),
            "a failed call with usage counts"
        );
        assert_eq!(probe.successor_tokens, 90_000);
        assert!(!t.watch(2, e, M), "once per window");
        assert_eq!(t.provider_round(2, e, M, Some(1), true), None);
    }

    #[test]
    fn a_call_that_never_reached_the_provider_keeps_the_probe() {
        let (mut t, e) = armed(2);
        // Canceled before sending, or failed without any usage.
        assert_eq!(t.provider_round(2, e, M, None, false), None);
        let (probe, _) = t.provider_round(2, e, M, Some(1), true).unwrap();
        assert_eq!(probe.rounds, 1, "the uncounted call is not a round");
    }

    #[test]
    fn a_model_switch_or_rebind_drops_the_probe() {
        let (mut t, e) = armed(2);
        assert!(!t.watch(2, e, "claude-opus-4-7"), "another model");
        assert!(!t.watch(2, e, M), "dropped, not suspended");
        let (mut t, _) = armed(2);
        let other = uuid::Uuid::new_v4();
        assert_eq!(
            t.provider_round(2, other, M, Some(1), true),
            None,
            "another binding"
        );
        assert!(!t.watch(2, other, M));
    }

    #[test]
    fn generated_session_ids_are_logged_as_is() {
        for id in [
            "20260916-101112-a3f9",
            "6f1c2d3e-4b5a-4c6d-8e7f-90a1b2c3d4e5",
            "ephemeral-6f1c2d3e-4b5a-4c6d-8e7f-90a1b2c3d4e5",
        ] {
            assert_eq!(SessionLabel::of(id), SessionLabel::Generated(id.into()));
        }
        // A real generated id, as the session type makes it.
        let session = agent_core::core::session::Session::new(M, "low", None);
        assert!(is_generated_session_id(&session.id), "{}", session.id);
    }

    #[test]
    fn other_session_ids_become_an_opaque_keyed_reference() {
        for id in [
            "",
            "telemetry-session",
            "2026091-101112-a3f9x",
            "20260916-101112-A3F9",
            "20260916 101112-a3f9",
            "20260916-101112-a3f9\n",
            "6F1C2D3E-4B5A-4C6D-8E7F-90A1B2C3D4E5",
            "ephemeral-x",
            "/home/alice/.ssh/id_rsa",
            "sk-ant-api03-SENTINEL\u{1b}[31m",
        ] {
            let SessionLabel::Ref(r) = SessionLabel::of(id) else {
                panic!("{id:?} logged as-is")
            };
            assert_eq!(r.len(), 12);
            assert!(r.bytes().all(is_lower_hex), "{r}");
            assert_eq!(SessionLabel::of(id), SessionLabel::Ref(r.clone()), "stable");
            if !id.is_empty() {
                assert!(!r.contains(id) && !id.contains(&r));
            }
        }
        assert_ne!(SessionLabel::of("a"), SessionLabel::of("b"));
    }

    /// 20-byte ids (the session shape's length) with a multi-byte character
    /// at every byte offset, so one straddles each fixed slicing point of
    /// the shape check (8, 9, 15, 16); likewise for the UUID lengths.
    #[test]
    fn unicode_ids_straddling_every_shape_boundary_are_opaque() {
        for ch in ["é", "€", "😀"] {
            for len in [20, 36, 46] {
                for at in 0..=len - ch.len() {
                    let id = format!("{}{ch}{}", "1".repeat(at), "1".repeat(len - at - ch.len()));
                    assert_eq!(id.len(), len);
                    let SessionLabel::Ref(r) = SessionLabel::of(&id) else {
                        panic!("{id:?} logged as-is")
                    };
                    assert!(r.len() == 12 && r.bytes().all(is_lower_hex), "{r}");
                }
            }
        }
        // Valid shapes with one byte swapped for a multi-byte character.
        for id in [
            "2026091é-101112-a3f9",
            "20260916é101112-a3f",
            "20260916-10111é-a3f",
            "20260916-101112é3f9",
            "20260916-101112-é3f",
            "ephemeral-6f1c2d3e-4b5a-4c6d-8e7f-90a1b2c3d4é",
        ] {
            assert!(!is_generated_session_id(id), "{id:?}");
        }
    }

    /// A completion keyed by an older binding (or window) is stale: it must
    /// not touch a probe a newer binding has armed since.
    #[test]
    fn a_stale_completion_never_touches_a_newer_probe() {
        let (mut t, old) = armed(2);
        assert!(t.watch(2, old, M), "the old call watched");
        // Reset and a new rollover before the old call completes.
        let new = uuid::Uuid::new_v4();
        t.rolled_over(2, new, M, 50_000, 60_000);
        assert_eq!(t.provider_round(2, old, M, Some(1), true), None);
        assert_eq!(
            t.provider_round(1, new, M, Some(1), true),
            None,
            "old window"
        );
        assert!(t.watch(2, new, M), "the newer probe survives");
        let (probe, tokens) = t.provider_round(2, new, M, Some(55_000), true).unwrap();
        assert_eq!(
            (probe.successor_tokens, probe.rounds, tokens),
            (50_000, 1, 55_000)
        );
        assert_eq!(
            t.provider_round(2, new, M, Some(1), true),
            None,
            "exactly once"
        );

        // A model change under the current binding still invalidates it.
        let (mut t, e) = armed(3);
        assert_eq!(
            t.provider_round(3, e, "claude-opus-4-7", Some(1), true),
            None
        );
        assert!(!t.watch(3, e, M), "dropped");
    }

    /// What a log reader sees: one line of plain `key=value` pairs, no
    /// quoting or nesting, and no field at all for an unknown value.
    #[test]
    fn fmt_lines_are_flat_key_value_pairs() {
        #[derive(Clone, Default)]
        struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let sink = Sink::default();
        let writer = sink.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .without_time()
            .finish();
        let id = "20260916-101112-a3f9";
        let generated = SessionLabel::of(id);
        let opaque = SessionLabel::of("caller supplied");
        let SessionLabel::Ref(r) = &opaque else {
            unreachable!()
        };
        let (_, e) = armed(4);
        let probe = SettleProbe {
            window: 4,
            successor_tokens: 92_000,
            target_tokens: 100_000,
            rounds: 1,
            epoch: e,
            model: M.into(),
        };
        tracing::subscriber::with_default(subscriber, || {
            emit_deferred(&generated, 3, DeferReason::WorkersPending, 2, 410_000, None);
            emit_settled(&generated, &probe, 97_500, 95_000);
            emit_deferred(&opaque, 3, DeferReason::Admission, 0, 1, Some(2));
            emit_refused(&opaque, 3, DeferReason::Admission, 0, 1, Some(2));
        });
        let out = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 4, "{out}");
        assert!(
            lines[0].ends_with(&format!("synaps::context: context rollover deferred session={id} window=3 reason=workers_pending workers=2 footprint_tokens=410000")),
            "{out}"
        );
        assert!(
            lines[1].ends_with(&format!("synaps::context: context window settled session={id} window=4 provider_context_tokens=97500 request_tokens=95000 successor_tokens=92000 target_tokens=100000 rounds_since_rollover=1")),
            "{out}"
        );
        assert!(
            lines[2].ends_with(&format!("synaps::context: context rollover deferred session_ref={r} window=3 reason=admission workers=0 footprint_tokens=1 provider_context_tokens=2")),
            "{out}"
        );
        // A refusal ends the turn: never logged as a deferral.
        assert!(
            lines[3].ends_with(&format!("synaps::context: context rollover refused: successor over the hard budget session_ref={r} window=3 reason=admission workers=0 footprint_tokens=1 provider_context_tokens=2")),
            "{out}"
        );
        assert!(!out.contains(M), "the model is never logged");
    }

    #[test]
    fn a_rollover_resets_deferral_marks() {
        let mut t = WindowTelemetry::default();
        assert!(t.should_log_deferral(1, DeferReason::WorkersPending, 1));
        t.rolled_over(2, uuid::Uuid::new_v4(), M, 1, 1);
        assert!(t.should_log_deferral(2, DeferReason::WorkersPending, 1));
    }
}
