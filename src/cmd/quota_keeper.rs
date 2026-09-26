//! `synaps quota-keeper` — poll per-account quota (read-only by default) and,
//! only for explicitly opted-in Codex accounts, run one minimal, tool-free
//! activation request per verified weekly reset.
//!
//! The scheduler semantics live in `synaps_cli::auth::quota_keeper` (pure,
//! synthetic-clock tested). This file is the runner: it enumerates accounts
//! through the broker, fetches usage snapshots behind the broker boundary,
//! maps them to keeper observations, persists private state, holds the
//! singleton and per-account locks, and — when authorized — performs the
//! bounded activation POST.
//!
//! Safety properties enforced here (the rest are in the pure module):
//! * No inference unless `--activate <openai-codex@label>` names the account
//!   AND `--model` is an explicit, catalog-known Codex model.
//! * The activation request uses the SAME `CredentialRef` the keeper tracks
//!   (`access_token_for`), derives the account header from THAT token, hits
//!   the pinned Codex responses URL, sends no tools/MCP/context, reads a
//!   bounded body under a timeout, and never retries by itself.
//! * Seat identity is established from the credential itself, not from
//!   metadata: every `--activate` account vends its pinned token at startup
//!   and the FULL provider account id is digested into a strong seat
//!   fingerprint (the state key; canonical across aliases). Immediately
//!   before the POST the activator re-vends the token and re-verifies the
//!   seat; a mismatch (re-login under the same label mid-run) is a typed
//!   `NotSent` and disables activation for that account until restart, so
//!   usage evidence from seat A can never authorize inference on seat B.
//!   An 8-character metadata prefix is only a read-only discriminator.
//! * The wire body mirrors the production Codex builder (which deliberately
//!   omits `max_output_tokens` — the ChatGPT backend may reject it), with the
//!   lowest reasoning effort the model supports, validated through
//!   `plan_codex_execution`. There is therefore NO guaranteed output-token
//!   ceiling; cost is bounded by minimal effort, a one-word prompt, low
//!   verbosity, no tools and a hard request timeout.
//! * State is written (0600, atomic) BEFORE the request leaves the process.
//! * The canonical keeper dir derives from the credential SOURCE (the
//!   resolved `auth.json`, or the remote endpoint), not the active profile,
//!   so profiles sharing one `auth.json` share locks and ledgers.
//! * Per-account locks live in the canonical keeper dir regardless of
//!   `--state-dir`, so two keepers on this host cannot act on one account;
//!   and `--state-dir` is refused together with `--activate`, so the attempt
//!   ledger can never be sidestepped by pointing at a fresh directory.
//! * Attempt ledgers are never deleted on reconciliation: an account filtered
//!   out by `--account` keeps its spent generation for when it returns.
//! * Locks are host-local, so activation is only permitted with the LOCAL
//!   credential source (the broker host). A remote broker source is
//!   poll-only until a broker-side attempt lease exists.
//! * One attempt per reset generation; a second one requires an explicit
//!   `--rearm` for that account on a new invocation.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use clap::Args;

use synaps_cli::auth::quota_keeper as qk;
use synaps_cli::auth::quota_policy as qp;
use synaps_cli::auth::usage::{self, UsageSnapshot};
use synaps_cli::auth::{
    Account, AccountSummary, BrokerError, CredentialBroker, CredentialRef, OAuthProviderId,
};
use synaps_cli::CancellationToken;

/// Pinned Codex inference endpoint used for the activation request.
pub const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
/// Instructions for the activation turn. No tools, no context, one word.
const ACTIVATION_INSTRUCTIONS: &str =
    "You are a quota keeper probe. Reply with the single word OK and nothing else.";
const ACTIVATION_INPUT: &str = "OK";
/// Routing key for the activation body (mirrors the production field).
const ACTIVATION_PROMPT_CACHE_KEY: &str = "synaps-quota-keeper-activation-v1";
/// Bound on the activation response body we drain (SSE). Exceeding it is
/// reported as ambiguous, never as a completed turn.
const ACTIVATION_MAX_BODY_BYTES: usize = 64 * 1024;
/// Sub-directory holding canonical keeper state.
const KEEPER_DIR_NAME: &str = "quota-keeper";
/// Idle entries without any attempt ledger are pruned after this long.
const PRUNE_IDLE_MS: u64 = 30 * 24 * 60 * 60 * 1000;

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Args, Debug, Clone, Default)]
pub struct QuotaKeeperArgs {
    /// Single pass then exit (default: daemon loop until SIGTERM/Ctrl-C).
    #[arg(long)]
    pub once: bool,
    /// Emit one JSON object per event/pass on stdout (secret-free).
    #[arg(long)]
    pub json: bool,
    /// Restrict to provider(s) with usage support (repeatable).
    #[arg(long = "provider", value_name = "ID")]
    pub providers: Vec<String>,
    /// Restrict to exact account(s) as `provider@label` (repeatable). Unknown
    /// accounts are an error, never a fallback.
    #[arg(long = "account", value_name = "PROVIDER@LABEL")]
    pub accounts: Vec<String>,
    /// EXPLICIT per-account activation opt-in (repeatable). Only
    /// `openai-codex@<label>` (or `openai-codex` for the default slot) is
    /// accepted. Without this flag the keeper is read-only. Requires --model.
    #[arg(long = "activate", value_name = "openai-codex@LABEL", requires = "model")]
    pub activate: Vec<String>,
    /// Codex model for the activation request (required with --activate;
    /// validated against the Codex catalog; `openai-codex/` prefix accepted).
    #[arg(long, value_name = "MODEL")]
    pub model: Option<String>,
    /// Base poll interval in seconds (clamped 60..3600). The next poll is the
    /// sooner of this interval and the reported reset + a short grace.
    #[arg(long, value_name = "SECS", default_value_t = qk::DEFAULT_POLL_INTERVAL_MS / 1000)]
    pub poll_interval: u64,
    /// Error backoff cap in seconds.
    #[arg(long, value_name = "SECS", default_value_t = qk::DEFAULT_MAX_BACKOFF_MS / 1000)]
    pub max_backoff: u64,
    /// A snapshot older than this many seconds is not capacity.
    #[arg(long, value_name = "SECS", default_value_t = qk::DEFAULT_STALE_AFTER_MS / 1000)]
    pub stale_after: u64,
    /// Explicitly allow ONE more activation attempt for an account whose
    /// current generation is `unverified` (repeatable; requires the same
    /// account in --activate). Never automatic.
    #[arg(long = "rearm", value_name = "openai-codex@LABEL", requires = "activate")]
    pub rearm: Vec<String>,
    /// Alternate private state directory for read-only runs (default: the
    /// canonical `quota-keeper/` directory next to the resolved auth.json).
    /// Refused together with --activate; per-account locks always live in
    /// the canonical directory.
    #[arg(long, value_name = "PATH", conflicts_with = "activate")]
    pub state_dir: Option<PathBuf>,
    /// Print the persisted state (secret-free JSON) and exit.
    #[arg(long)]
    pub show_state: bool,
}

impl QuotaKeeperArgs {
    fn keeper_config(&self) -> qk::KeeperConfig {
        qk::KeeperConfig {
            poll_interval_ms: self.poll_interval.saturating_mul(1000),
            max_backoff_ms: self.max_backoff.saturating_mul(1000),
            stale_after_ms: self.stale_after.saturating_mul(1000),
            ..qk::KeeperConfig::default()
        }
        .bounded()
    }
}

/// Canonical keeper directory, derived from the credential SOURCE: the
/// directory of the resolved `auth.json` for a local source (profiles that
/// inherit the base `auth.json` therefore share it), or a per-endpoint
/// directory under the base dir for a remote broker.
pub fn canonical_keeper_dir(source: &synaps_cli::auth::CredentialSource) -> PathBuf {
    match source {
        synaps_cli::auth::CredentialSource::Local => {
            let auth = synaps_cli::auth::auth_file_path();
            auth.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(synaps_cli::config::base_dir)
                .join(KEEPER_DIR_NAME)
        }
        synaps_cli::auth::CredentialSource::Remote { endpoint, .. } => {
            let safe: String = endpoint
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
                .collect();
            synaps_cli::config::base_dir()
                .join(KEEPER_DIR_NAME)
                .join(format!("remote-{safe}"))
        }
    }
}

// ── Seams ────────────────────────────────────────────────────────────────────

/// Time source. Tests inject a synthetic clock.
#[async_trait]
pub(crate) trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
    async fn sleep(&self, d: Duration);
}

struct SystemClock;

#[async_trait]
impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        synaps_cli::epoch_millis()
    }
    async fn sleep(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }
}

/// Performs the bounded activation request. Tests inject a fake.
///
/// `expected_seat` is the strong seat fingerprint the keeper tracks for
/// `cred`; the implementation MUST verify the freshly vended token against
/// it before anything leaves the process.
#[async_trait]
pub(crate) trait Activator: Send + Sync {
    async fn activate(
        &self,
        cred: &CredentialRef,
        expected_seat: &str,
        model: &str,
        timeout: Duration,
    ) -> qk::AttemptOutcome;
}

/// Production activator: pinned Codex responses POST, no tools, bounded.
pub(crate) struct CodexActivator {
    broker: Arc<dyn CredentialBroker>,
    http: reqwest::Client,
    url: String,
}

impl CodexActivator {
    pub(crate) fn new(broker: Arc<dyn CredentialBroker>, url: String) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        Ok(Self { broker, http, url })
    }
}

/// Validated activation plan for `model`: the LOWEST reasoning effort the
/// model supports, authorized through the same execution-plan builder the
/// runtime uses (`plan_codex_execution`, role `Internal` so no multi-agent
/// mode item is ever injected). Fails closed on unknown models/ladders.
pub(crate) fn activation_plan(
    model: &str,
) -> Result<synaps_cli::runtime::openai::catalog::CodexExecutionPlan, String> {
    use synaps_cli::reasoning::ReasoningLevel;
    use synaps_cli::runtime::openai::catalog::{plan_codex_execution, CodexRequestRole};
    let qualified = format!("openai-codex/{model}");
    let mut last_err = String::new();
    for level in [
        ReasoningLevel::Low,
        ReasoningLevel::Medium,
        ReasoningLevel::High,
    ] {
        match plan_codex_execution(&qualified, level, CodexRequestRole::Internal, None) {
            Ok(plan) => return Ok(plan),
            Err(e) => last_err = e.to_string(),
        }
    }
    Err(format!("no minimal reasoning effort authorized for {qualified}: {last_err}"))
}

/// The exact activation body. Mirrors the production Codex request builder
/// field-for-field (`store`, `stream`, `instructions`, `input`,
/// `tool_choice`, `parallel_tool_calls`, `include`, `text`,
/// `prompt_cache_key`, `reasoning`) except that no `tools` are attached,
/// verbosity is `low`, and — like production — `max_output_tokens` is
/// omitted because the ChatGPT backend may reject it. Pure so tests can pin it.
pub(crate) fn activation_body(
    model: &str,
    plan: &synaps_cli::runtime::openai::catalog::CodexExecutionPlan,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": model,
        "store": false,
        "stream": true,
        "instructions": ACTIVATION_INSTRUCTIONS,
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": ACTIVATION_INPUT }],
        }],
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "include": ["reasoning.encrypted_content"],
        "text": { "verbosity": "low" },
        "prompt_cache_key": ACTIVATION_PROMPT_CACHE_KEY,
    });
    if let Some(effort) = plan.wire_effort {
        body["reasoning"] = serde_json::json!({ "effort": effort.as_str() });
    }
    body
}

#[async_trait]
impl Activator for CodexActivator {
    async fn activate(
        &self,
        cred: &CredentialRef,
        expected_seat: &str,
        model: &str,
        timeout: Duration,
    ) -> qk::AttemptOutcome {
        if cred.provider != OAuthProviderId::OpenAiCodex {
            return qk::AttemptOutcome::NotSent {
                stage: qk::NotSentStage::Unsupported,
                reason: "activation is only implemented for openai-codex".into(),
            };
        }
        if !expected_seat.starts_with(qk::SEAT_FP_PREFIX) {
            return qk::AttemptOutcome::NotSent {
                stage: qk::NotSentStage::IdentityMismatch,
                reason: "no strong seat identity for this account".into(),
            };
        }
        // Same credential the keeper tracks; header derived from THIS token.
        let token = match self.broker.access_token_for(cred).await {
            Ok(t) => t.token,
            Err(e) => {
                return qk::AttemptOutcome::NotSent {
                    stage: qk::NotSentStage::TokenVend,
                    reason: format!("token vend failed: {e}"),
                }
            }
        };
        let Some(account_id) = synaps_cli::auth::extract_codex_account_id(&token) else {
            return qk::AttemptOutcome::NotSent {
                stage: qk::NotSentStage::AccountId,
                reason: "token carries no chatgpt account id".into(),
            };
        };
        // Verify the seat NOW, from the token that will carry the request:
        // the usage evidence that authorized this attempt belongs to
        // `expected_seat`, and a re-login under the same label must not
        // spend it on another account.
        let seat = qk::AccountIdentity::seat_fingerprint(cred.provider, &account_id);
        if seat.as_deref() != Some(expected_seat) {
            return qk::AttemptOutcome::NotSent {
                stage: qk::NotSentStage::IdentityMismatch,
                reason: "the vended token belongs to a different provider seat than the tracked one"
                    .into(),
            };
        }
        let plan = match activation_plan(model) {
            Ok(p) => p,
            Err(e) => {
                return qk::AttemptOutcome::NotSent {
                    stage: qk::NotSentStage::RequestBuild,
                    reason: e,
                }
            }
        };
        let body = match serde_json::to_vec(&activation_body(model, &plan)) {
            Ok(b) => b,
            Err(e) => {
                return qk::AttemptOutcome::NotSent {
                    stage: qk::NotSentStage::RequestBuild,
                    reason: format!("body serialization: {e}"),
                }
            }
        };
        let req = self
            .http
            .post(&self.url)
            .bearer_auth(&token)
            .header("chatgpt-account-id", account_id)
            .header("originator", "synaps")
            .header("OpenAI-Beta", "responses=experimental")
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .timeout(timeout)
            .body(body);
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) if e.is_connect() => {
                // Nothing reached the provider.
                return qk::AttemptOutcome::NotSent {
                    stage: qk::NotSentStage::Connect,
                    reason: "connect failed".into(),
                };
            }
            Err(e) if e.is_timeout() => {
                return qk::AttemptOutcome::Ambiguous {
                    reason: "request timed out".into(),
                }
            }
            Err(_) => {
                return qk::AttemptOutcome::Ambiguous {
                    reason: "transport error before a response".into(),
                }
            }
        };
        let status = resp.status().as_u16();
        // Drain the body (bounded) so the turn completes server-side; body
        // contents are never retained or logged. Anything but a natural end
        // of stream within the cap and the deadline is ambiguous.
        let drained = drain_bounded(resp, ACTIVATION_MAX_BODY_BYTES, timeout).await;
        match status {
            200..=299 => match drained {
                Ok(()) => qk::AttemptOutcome::Sent {
                    http_status: status,
                },
                Err(why) => qk::AttemptOutcome::Ambiguous {
                    reason: format!("HTTP {status} but {why}"),
                },
            },
            400..=499 => qk::AttemptOutcome::Rejected {
                http_status: status,
            },
            _ => qk::AttemptOutcome::Ambiguous {
                reason: format!("HTTP {status}"),
            },
        }
    }
}

/// Read the response to its natural end. `Err` names why that did not
/// happen (cap exceeded, stream error, deadline) — all ambiguous outcomes.
async fn drain_bounded(
    resp: reqwest::Response,
    cap: usize,
    timeout: Duration,
) -> Result<(), &'static str> {
    use futures::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut seen = 0usize;
    let drain = async {
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(c) => {
                    seen += c.len();
                    if seen > cap {
                        return Err("the response exceeded the drain cap");
                    }
                }
                Err(_) => return Err("the stream was cut"),
            }
        }
        Ok(())
    };
    match tokio::time::timeout(timeout, drain).await {
        Ok(r) => r,
        Err(_) => Err("the stream did not end before the deadline"),
    }
}

// ── Usage mapping ────────────────────────────────────────────────────────────

/// Map a typed usage snapshot to a keeper observation (pure).
pub(crate) fn observation_from_snapshot(s: &UsageSnapshot) -> qk::UsageObservation {
    let windows = s
        .windows
        .iter()
        .filter_map(|w| {
            let models = match &w.scope {
                usage::WindowScope::Account => None,
                usage::WindowScope::Model { model } => Some(vec![model.clone()]),
                // Feature quotas (code review, OAuth apps) do not gate inference.
                usage::WindowScope::Feature { .. } => return None,
            };
            Some(qp::WindowLimit {
                id: w.id.clone(),
                duration_ms: w.duration_secs.map(|d| d.saturating_mul(1000)),
                used_percent: w.used_percent.valid(),
                limit_reached: w.limit_reached,
                resets_at_ms: w.reset_at,
                models,
            })
        })
        .collect();
    let models = if s.model_availability.is_empty() {
        None
    } else {
        Some(
            s.model_availability
                .iter()
                .map(|m| qp::ModelAvailability {
                    model: m.model.clone(),
                    state: match m.availability {
                        usage::Availability::Available => qp::ModelState::Available,
                        usage::Availability::Exhausted => qp::ModelState::Exhausted,
                        usage::Availability::Unknown => qp::ModelState::Unknown,
                    },
                })
                .collect(),
        )
    };
    let limit_reached = if s.limit_reached == Some(true) || s.spend_control_reached == Some(true) {
        Some(true)
    } else {
        s.limit_reached
    };
    let banked = s.banked_resets.as_ref().map(|b| qk::BankedResets {
        available_count: b.available_count,
        earliest_expiry_ms: b.credits.iter().filter_map(|c| c.expires_at).min(),
        inventory_error: b.inventory_error.clone(),
    });
    qk::UsageObservation {
        observed_at_ms: s.observed_at,
        outcome: qk::ObservationOutcome::Ok {
            windows,
            models,
            limit_reached,
            banked,
            identity_prefix: s.identity_prefix.clone(),
        },
    }
}

/// Map a broker error from `usage()` to a keeper observation (pure).
pub(crate) fn observation_from_error(now_ms: u64, e: &BrokerError) -> qk::UsageObservation {
    let detail = e.to_string();
    let outcome = match e {
        BrokerError::Unauthorized
        | BrokerError::Credential(_)
        | BrokerError::NotConfigured(_)
        | BrokerError::UnknownAccount { .. } => qk::ObservationOutcome::AuthError { detail },
        BrokerError::UnsupportedCapability { .. }
        | BrokerError::UnknownProvider(_)
        | BrokerError::UnsupportedAccount { .. }
        | BrokerError::InvalidAccount(_)
        | BrokerError::RegistrationRequired { .. }
        | BrokerError::Denied(_) => qk::ObservationOutcome::Unsupported { detail },
        BrokerError::Transport(msg) => {
            let m = msg.to_ascii_lowercase();
            if m.contains("malformed") || m.contains("exceeded") {
                qk::ObservationOutcome::Malformed { detail }
            } else {
                qk::ObservationOutcome::Transport { detail }
            }
        }
        _ => qk::ObservationOutcome::Transport { detail },
    };
    qk::UsageObservation {
        observed_at_ms: now_ms,
        outcome,
    }
}

// ── Account resolution ───────────────────────────────────────────────────────

/// Validate a `--model` value against the Codex catalog. Accepts the
/// `openai-codex/` prefix. No default: an absent model is an error when
/// activation is enabled.
pub(crate) fn validate_codex_model(raw: &str) -> Result<String, String> {
    let id = raw
        .trim()
        .strip_prefix("openai-codex/")
        .unwrap_or(raw.trim())
        .to_string();
    if id.is_empty() {
        return Err("--model must not be empty".into());
    }
    let known: Vec<String> = synaps_cli::runtime::openai::catalog::codex_static_catalog_models()
        .into_iter()
        .map(|m| m.id)
        .collect();
    if known.iter().any(|k| k == &id) {
        Ok(id)
    } else {
        Err(format!(
            "unknown Codex model '{id}'; known: {}",
            known.join(", ")
        ))
    }
}

/// Parse a CLI `provider@label` (or bare `provider` for the default slot)
/// into a credential ref, with the storage-key grammar: `@default` is not
/// a label (use the bare provider id).
pub(crate) fn parse_account_arg(raw: &str) -> Result<CredentialRef, String> {
    let raw = raw.trim();
    match raw.split_once('@') {
        None => {
            let provider: OAuthProviderId = raw.parse()?;
            Ok(CredentialRef::default_for(provider))
        }
        Some((p, l)) => {
            let provider: OAuthProviderId = p.parse()?;
            let label = synaps_cli::auth::AccountLabel::parse(l)
                .map_err(|e| format!("'{raw}': {e} (use '{p}' for the default slot)"))?;
            Ok(CredentialRef::new(provider, Account::Named(label)))
        }
    }
}

/// One account the keeper tracks.
#[derive(Debug, Clone)]
pub(crate) struct KeptAccount {
    /// Strong seat identity for opted-in accounts (established from the
    /// vended token); weak metadata preview for read-only ones.
    pub identity: qk::AccountIdentity,
    pub credential: CredentialRef,
    pub opted_in: bool,
    /// Operator asked for one explicit extra attempt (`--rearm`).
    pub rearm: bool,
    /// Provider account-id prefix (≤ 8 chars) used ONLY to pair usage
    /// responses with the tracked seat and to spot alias duplicates. Not an
    /// identity.
    pub account_id_prefix: Option<String>,
}

impl KeptAccount {
    /// Whether two kept accounts are the same provider seat, as far as the
    /// available evidence shows: strong fingerprints when both have one,
    /// otherwise the metadata prefix (a prefix collision is treated as a
    /// duplicate — fail closed).
    fn same_seat(&self, other: &Self) -> bool {
        if self.credential.provider != other.credential.provider {
            return false;
        }
        if self.identity.is_strong() && other.identity.is_strong() {
            return self.identity.identity_fp == other.identity.identity_fp;
        }
        match (&self.account_id_prefix, &other.account_id_prefix) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }
}

/// Identity of a listed row: strong when the broker lists a seat
/// fingerprint (full-id digest), otherwise a weak metadata preview.
fn listed_identity_for(source: &str, cred: &CredentialRef, summary: &AccountSummary) -> qk::AccountIdentity {
    let identity_fp = match summary.seat_fingerprint.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        Some(listed) => qk::AccountIdentity::strong_from_listed(listed),
        None => qk::AccountIdentity::fingerprint(
            summary.account_id_prefix.as_deref(),
            summary.identity.as_deref(),
            summary.added_at,
        ),
    };
    qk::AccountIdentity {
        source: source.to_string(),
        credential: cred.storage_key(),
        identity_fp,
    }
}

/// Establish the STRONG seat identity of an opted-in Codex account from the
/// token the broker vends for exactly this credential — not from the listing.
/// Returns the seat fingerprint and the full id's prefix (for usage
/// pairing). Fails closed when the token carries no account id or disagrees
/// with the listed fingerprint/prefix.
async fn establish_seat_identity(
    broker: &dyn CredentialBroker,
    cred: &CredentialRef,
    summary: &AccountSummary,
) -> Result<(String, String), String> {
    let token = broker
        .access_token_for(cred)
        .await
        .map_err(|e| format!("--activate {cred}: cannot establish seat identity (token vend failed: {e})"))?;
    let Some(account_id) = synaps_cli::auth::extract_codex_account_id(&token.token) else {
        return Err(format!(
            "--activate {cred}: cannot establish seat identity (token carries no provider account id); \
             activation refused"
        ));
    };
    let Some(fp) = qk::AccountIdentity::seat_fingerprint(cred.provider, &account_id) else {
        return Err(format!("--activate {cred}: cannot establish seat identity (empty account id)"));
    };
    let prefix: String = account_id.chars().take(8).collect();
    let listed = listed_identity_for("", cred, summary);
    let disagree = (listed.is_strong() && listed.identity_fp != fp)
        || summary
            .account_id_prefix
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .is_some_and(|p| p != prefix);
    if disagree {
        return Err(format!(
            "--activate {cred}: the listed account metadata and the vended token name different seats; \
             re-login this account before activating"
        ));
    }
    Ok((fp, prefix))
}

/// Enumerate accounts through the broker and apply the CLI filters/opt-ins.
///
/// Returns the kept accounts plus non-fatal warnings. Two aliases that
/// resolve to one provider seat are a warning while everything is read-only
/// and an ERROR as soon as either alias is opted in: the seat would have two
/// labels racing for one activation ledger.
pub(crate) async fn resolve_accounts(
    broker: &dyn CredentialBroker,
    source: &str,
    providers: &[String],
    only: &[String],
    activate: &[String],
    rearm: &[String],
) -> Result<(Vec<KeptAccount>, Vec<String>), String> {
    let providers: Vec<OAuthProviderId> = if providers.is_empty() {
        usage::supported_usage_providers().to_vec()
    } else {
        let mut v = Vec::new();
        for p in providers {
            let id: OAuthProviderId = p.parse()?;
            if !usage::supports_usage(id) {
                return Err(format!("provider '{id}' has no usage support"));
            }
            v.push(id);
        }
        v
    };
    let only: Vec<CredentialRef> = only
        .iter()
        .map(|a| parse_account_arg(a))
        .collect::<Result<_, _>>()?;
    let mut opt_in: BTreeSet<String> = BTreeSet::new();
    for a in activate {
        let cred = parse_account_arg(a)?;
        if cred.provider != OAuthProviderId::OpenAiCodex {
            return Err(format!(
                "--activate {a}: activation is only supported for openai-codex accounts"
            ));
        }
        opt_in.insert(cred.storage_key());
    }
    let mut rearm_set: BTreeSet<String> = BTreeSet::new();
    for r in rearm {
        let key = parse_account_arg(r)?.storage_key();
        if !opt_in.contains(&key) {
            return Err(format!("--rearm {r}: the account must also be listed in --activate"));
        }
        rearm_set.insert(key);
    }
    let mut kept = Vec::new();
    for provider in providers {
        let summaries = match broker.accounts(provider).await {
            Ok(s) => s,
            Err(BrokerError::UnsupportedCapability { .. }) => Vec::new(),
            Err(e) => return Err(format!("listing {provider} accounts: {e}")),
        };
        for s in summaries {
            let account = Account::parse(&s.label)
                .map_err(|e| format!("broker returned an invalid label for {provider}: {e}"))?;
            let cred = CredentialRef::new(provider, account);
            if !only.is_empty() && !only.contains(&cred) {
                continue;
            }
            let key = cred.storage_key();
            let opted_in = opt_in.contains(&key);
            let (identity, account_id_prefix) = if opted_in {
                let (fp, prefix) = establish_seat_identity(broker, &cred, &s).await?;
                (
                    qk::AccountIdentity {
                        source: source.to_string(),
                        credential: key.clone(),
                        identity_fp: fp,
                    },
                    Some(prefix),
                )
            } else {
                (
                    listed_identity_for(source, &cred, &s),
                    s.account_id_prefix
                        .as_deref()
                        .map(str::trim)
                        .filter(|p| !p.is_empty())
                        .map(str::to_string),
                )
            };
            kept.push(KeptAccount {
                identity,
                opted_in,
                rearm: rearm_set.contains(&key),
                credential: cred,
                account_id_prefix,
            });
        }
    }
    for want in &only {
        if !kept.iter().any(|k| &k.credential == want) {
            return Err(format!("account '{want}' is not connected"));
        }
    }
    for key in &opt_in {
        if !kept.iter().any(|k| k.credential.storage_key() == *key) {
            return Err(format!(
                "--activate {key}: account is not connected (or excluded by --account/--provider)"
            ));
        }
    }
    // Alias dedupe: two labels naming the same provider seat. Fail closed
    // when either is opted in; otherwise track the seat once (first label in
    // deterministic order) and warn.
    kept.sort_by(|a, b| {
        a.identity
            .key()
            .cmp(&b.identity.key())
            .then_with(|| a.credential.storage_key().cmp(&b.credential.storage_key()))
    });
    let mut warnings = Vec::new();
    let mut deduped: Vec<KeptAccount> = Vec::new();
    for a in kept {
        if let Some(b) = deduped.iter().find(|b| b.same_seat(&a)) {
            if a.opted_in || b.opted_in {
                return Err(format!(
                    "--activate: '{}' and '{}' are the same provider seat; a seat with two labels \
                     cannot be activated — remove one alias (`synaps auth remove`) or exclude it \
                     with --account",
                    b.credential, a.credential
                ));
            }
            warnings.push(format!(
                "'{}' and '{}' resolve to the same provider seat; tracked once as '{}'",
                b.credential, a.credential, b.credential
            ));
            continue;
        }
        deduped.push(a);
    }
    Ok((deduped, warnings))
}

// ── Runtime ──────────────────────────────────────────────────────────────────

/// Output sink (stdout in production; captured in tests).
pub(crate) trait Sink: Send + Sync {
    fn line(&self, s: &str);
}

struct StdoutSink;
impl Sink for StdoutSink {
    fn line(&self, s: &str) {
        println!("{s}");
    }
}

pub(crate) struct KeeperRuntime {
    pub cfg: qk::KeeperConfig,
    pub store: qk::StateStore,
    pub state: qk::KeeperState,
    pub broker: Arc<dyn CredentialBroker>,
    pub activator: Arc<dyn Activator>,
    pub clock: Arc<dyn Clock>,
    pub sink: Arc<dyn Sink>,
    pub json: bool,
    pub model: Option<String>,
    pub accounts: Vec<KeptAccount>,
    /// Held for the runtime's lifetime.
    _locks: Vec<qk::KeeperLock>,
}

/// Per-pass summary row (secret-free).
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct AccountRow {
    pub account: String,
    pub phase: String,
    pub opted_in: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekly_used_percent: Option<f64>,
    pub next_action_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_idle_delay_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub banked_resets: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub attempts_in_generation: u32,
}

impl KeeperRuntime {
    /// Build a runtime: acquire the state-dir singleton lock and the
    /// canonical per-account locks, load (or fail closed on corrupt) state,
    /// reconcile identities, recover any half-finished attempt.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        cfg: qk::KeeperConfig,
        state_dir: &Path,
        canonical_dir: &Path,
        accounts: Vec<KeptAccount>,
        broker: Arc<dyn CredentialBroker>,
        activator: Arc<dyn Activator>,
        clock: Arc<dyn Clock>,
        sink: Arc<dyn Sink>,
        json: bool,
        model: Option<String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        if accounts.iter().any(|a| a.opted_in) && model.is_none() {
            return Err("activation requested without --model".into());
        }
        let holder = format!("pid {}", std::process::id());
        let mut locks = vec![qk::KeeperLock::acquire(
            state_dir,
            qk::SINGLETON_LOCK_NAME,
            &holder,
        )?];
        let lock_dir = canonical_dir.join(qk::ACCOUNT_LOCK_DIR);
        for a in &accounts {
            // Label lock (always) plus, for a strong identity, the seat lock
            // shared by every alias of that seat.
            let mut names = vec![qk::account_lock_name(&a.credential.storage_key())];
            if a.identity.is_strong() {
                names.push(qk::account_lock_name(&format!(
                    "{}.{}",
                    a.identity.ledger_scope(),
                    a.identity.identity_fp
                )));
            }
            for name in names {
                locks.push(qk::KeeperLock::acquire(&lock_dir, &name, &holder).map_err(|e| {
                    format!("account '{}' is already kept by another keeper: {e}", a.credential)
                })?);
            }
        }
        let store = qk::StateStore::new(state_dir);
        let mut state = store.load()?;
        let ids: Vec<qk::AccountIdentity> = accounts.iter().map(|a| a.identity.clone()).collect();
        let now = clock.now_ms();
        // Never delete an attempt ledger; only idle, never-attempted entries.
        let dropped = state.prune_idle(&ids, now, PRUNE_IDLE_MS);
        for a in &accounts {
            state.entry(&a.identity);
        }
        let mut recovered = qk::recover_after_restart(&mut state, now);
        for a in accounts.iter().filter(|a| a.rearm && a.opted_in) {
            if let Some(ev) = qk::rearm(&mut state, now, &a.identity.key())? {
                recovered.push(ev);
            }
        }
        store.save(&state)?;
        let rt = Self {
            cfg,
            store,
            state,
            broker,
            activator,
            clock,
            sink,
            json,
            model,
            accounts,
            _locks: locks,
        };
        for d in dropped {
            rt.note(&format!("pruned idle state entry {d} (no attempt ledger)"));
        }
        rt.emit(&recovered);
        Ok(rt)
    }

    fn note(&self, msg: &str) {
        if self.json {
            self.sink
                .line(&serde_json::json!({ "event": "note", "message": msg }).to_string());
        } else {
            self.sink.line(&format!("keeper: {msg}"));
        }
    }

    fn emit(&self, events: &[qk::KeeperEvent]) {
        for e in events {
            if self.json {
                if let Ok(s) = serde_json::to_string(e) {
                    self.sink.line(&s);
                }
            } else {
                self.sink.line(&format!("keeper: {}", describe_event(e)));
            }
        }
    }

    fn account(&self, key: &str) -> Option<&KeptAccount> {
        self.accounts.iter().find(|a| a.identity.key() == key)
    }

    fn kept_keys(&self) -> Vec<String> {
        self.accounts.iter().map(|a| a.identity.key()).collect()
    }

    async fn poll(&mut self, key: &str) -> Result<(), Box<dyn std::error::Error>> {
        let Some(acct) = self.account(key).cloned() else {
            return Ok(());
        };
        let timeout = Duration::from_millis(self.cfg.usage_timeout_ms);
        let fetched = tokio::time::timeout(timeout, self.broker.usage(&acct.credential)).await;
        // Evaluate against the clock AFTER the fetch: the snapshot's
        // observed_at is stamped on receipt, so a `now` taken before the
        // await would make every reading look future-dated.
        let now = self.clock.now_ms();
        let obs = match fetched {
            Ok(Ok(snapshot)) => {
                let mut obs = observation_from_snapshot(&snapshot);
                // Identity pairing: the snapshot must name the seat we track.
                // (A differing prefix is proof of a different seat; an equal
                // prefix is not proof of the same one — activation re-checks
                // the full id from the token.)
                if let (Some(p), Some(tracked)) =
                    (snapshot.identity_prefix.as_deref(), acct.account_id_prefix.as_deref())
                {
                    if p != tracked {
                        obs.outcome = qk::ObservationOutcome::Malformed {
                            detail: "usage response names a different account than the tracked seat"
                                .into(),
                        };
                    }
                }
                // Match the full seat, not only its eight-character preview.
                // This also prevents another occupant's usage from falsely
                // verifying a reset on the old ledger after re-login. Older
                // brokers can still be polled read-only, but cannot authorize
                // activation without paired identity evidence.
                if acct.identity.is_strong() {
                    let observed = snapshot.seat_fingerprint.as_deref()
                        .map(qk::AccountIdentity::strong_from_listed);
                    if observed.as_deref().is_some_and(|fp| fp != acct.identity.identity_fp)
                        || (acct.opted_in && observed.is_none())
                    {
                        obs.outcome = qk::ObservationOutcome::Malformed {
                            detail: "usage has no matching full seat identity".into(),
                        };
                    }
                }
                obs
            }
            Ok(Err(e)) => observation_from_error(now, &e),
            Err(_) => qk::UsageObservation {
                observed_at_ms: now,
                outcome: qk::ObservationOutcome::Transport {
                    detail: "usage request timed out".into(),
                },
            },
        };
        let events = qk::observe(&mut self.state, &self.cfg, now, key, &obs)?;
        self.store.save(&self.state)?;
        self.emit(&events);
        Ok(())
    }

    /// One scheduler pass: poll due accounts, activate when authorized,
    /// verify from fresh usage.
    pub(crate) async fn pass(&mut self) -> Result<Vec<AccountRow>, Box<dyn std::error::Error>> {
        let now = self.clock.now_ms();
        let kept = self.kept_keys();
        for key in self.state.due_among(now, &kept) {
            self.poll(&key).await?;
            let Some(acct) = self.account(&key).cloned() else {
                continue;
            };
            let model = self.model.clone().unwrap_or_default();
            let decision = qk::activation_decision(
                &self.state,
                &self.cfg,
                self.clock.now_ms(),
                &key,
                acct.opted_in && !model.is_empty(),
                &model,
            );
            match decision {
                qk::ActivationDecision::Skip { reason } => {
                    if acct.opted_in
                        && matches!(
                            self.state.accounts.get(&key).map(|s| &s.phase),
                            Some(qk::Phase::AwaitingActivation { .. })
                        )
                    {
                        self.note(&format!("{}: activation skipped: {reason}", acct.credential));
                    }
                }
                qk::ActivationDecision::Activate { .. } => {
                    let (ticket, events) = qk::begin_attempt(&mut self.state, self.clock.now_ms(), &key)?;
                    // Persist BEFORE the request leaves the process.
                    self.store.save(&self.state)?;
                    self.emit(&events);
                    let timeout = Duration::from_millis(self.cfg.attempt_timeout_ms);
                    let outcome = match tokio::time::timeout(
                        timeout + Duration::from_secs(5),
                        self.activator.activate(
                            &acct.credential,
                            &acct.identity.identity_fp,
                            &model,
                            timeout,
                        ),
                    )
                    .await
                    {
                        Ok(o) => o,
                        Err(_) => qk::AttemptOutcome::Ambiguous {
                            reason: "activation exceeded its deadline".into(),
                        },
                    };
                    let identity_mismatch = matches!(
                        outcome,
                        qk::AttemptOutcome::NotSent {
                            stage: qk::NotSentStage::IdentityMismatch,
                            ..
                        }
                    );
                    let events = qk::finish_attempt(
                        &mut self.state,
                        &self.cfg,
                        self.clock.now_ms(),
                        &ticket,
                        outcome,
                    )?;
                    self.store.save(&self.state)?;
                    self.emit(&events);
                    if identity_mismatch {
                        // The label now vends a different seat: its usage
                        // evidence is not this ledger's. Fail closed for the
                        // rest of the process; a restart re-establishes the
                        // identity (and a new ledger) from the current token.
                        if let Some(a) = self.accounts.iter_mut().find(|a| a.identity.key() == key) {
                            a.opted_in = false;
                        }
                        self.note(&format!(
                            "{}: provider seat changed since the identity was established; \
                             activation DISABLED for this account until the keeper is restarted",
                            acct.credential
                        ));
                    }
                    // Verify only from fresh usage.
                    self.poll(&key).await?;
                }
            }
        }
        let rows = self.rows();
        if self.json {
            self.sink.line(
                &serde_json::json!({ "event": "pass", "now_ms": self.clock.now_ms(), "accounts": rows })
                    .to_string(),
            );
        } else {
            for r in &rows {
                self.sink.line(&describe_row(r));
            }
        }
        Ok(rows)
    }

    pub(crate) fn rows(&self) -> Vec<AccountRow> {
        self.accounts
            .iter()
            .filter_map(|a| {
                let st = self.state.accounts.get(&a.identity.key())?;
                Some(AccountRow {
                    account: a.credential.storage_key(),
                    phase: describe_phase(&st.phase),
                    opted_in: a.opted_in,
                    generation_ms: st.generation,
                    weekly_used_percent: st.weekly.as_ref().and_then(|w| w.used_percent),
                    next_action_at_ms: st.next_action_at_ms,
                    last_idle_delay_ms: st.last_idle_delay_ms,
                    banked_resets: st.banked.as_ref().and_then(|b| b.available_count),
                    last_error: st.last_error.clone(),
                    attempts_in_generation: st.attempts_in_generation,
                })
            })
            .collect()
    }

    /// Daemon loop. Sleeps until the next scheduled action (bounded by the
    /// poll interval) and exits gracefully on cancellation.
    pub(crate) async fn run_loop(
        &mut self,
        once: bool,
        cancel: CancellationToken,
    ) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            self.pass().await?;
            if once || cancel.is_cancelled() {
                break;
            }
            let now = self.clock.now_ms();
            let next = self
                .state
                .next_wake_among(&self.kept_keys())
                .unwrap_or(now + self.cfg.poll_interval_ms);
            let wait = next
                .saturating_sub(now)
                .clamp(1000, self.cfg.poll_interval_ms);
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = self.clock.sleep(Duration::from_millis(wait)) => {}
            }
        }
        self.store.save(&self.state)?;
        Ok(())
    }
}

// ── Display helpers ──────────────────────────────────────────────────────────

fn fmt_ts(ms: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms as i64)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| ms.to_string())
}

fn fmt_dur(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 86_400 {
        format!("{}d {}h", s / 86_400, (s % 86_400) / 3600)
    } else if s >= 3600 {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

pub(crate) fn describe_phase(p: &qk::Phase) -> String {
    match p {
        qk::Phase::Unknown { reason } => format!("unknown ({reason})"),
        qk::Phase::AuthError { detail } => format!("auth error ({detail})"),
        qk::Phase::Exhausted {
            generation,
            reset_passed: true,
        } => format!(
            "exhausted; reset {} passed but provider still asserts limit",
            fmt_ts(*generation)
        ),
        qk::Phase::Exhausted { generation, .. } => {
            format!("exhausted until {}", fmt_ts(*generation))
        }
        qk::Phase::Active {
            generation,
            used_percent,
            attribution,
            ..
        } => {
            let attr = match attribution {
                qk::Attribution::Observed => "observed".to_string(),
                qk::Attribution::AttemptCorrelated { attempt } => {
                    format!("verified window; keeper attempt {attempt} correlated, not proven causal")
                }
            };
            format!(
                "active ({}%, resets {}; {attr})",
                used_percent.map(|u| format!("{u:.0}")).unwrap_or_else(|| "?".into()),
                fmt_ts(*generation)
            )
        }
        qk::Phase::AwaitingActivation {
            generation,
            due_since_ms,
        } => format!(
            "due: reset {} passed, no new window anchored (due since {})",
            fmt_ts(*generation),
            fmt_ts(*due_since_ms)
        ),
        qk::Phase::ActivationPending {
            generation,
            attempt,
            ..
        } => format!(
            "activation pending verification (generation {}, attempt {attempt})",
            fmt_ts(*generation)
        ),
        qk::Phase::Unverified {
            generation,
            attempts,
        } => format!(
            "unverified: {attempts} attempt(s) for generation {} without a new window; not retrying",
            fmt_ts(*generation)
        ),
    }
}

fn describe_row(r: &AccountRow) -> String {
    let mut s = format!(
        "  {:<28} {}{}",
        r.account,
        if r.opted_in { "[activate] " } else { "[read-only] " },
        r.phase
    );
    if let Some(d) = r.last_idle_delay_ms {
        s.push_str(&format!("; last idle delay {}", fmt_dur(d)));
    }
    if let Some(b) = r.banked_resets.filter(|b| *b > 0) {
        s.push_str(&format!("; {b} banked reset(s) available"));
    }
    if let Some(e) = &r.last_error {
        s.push_str(&format!("; last error: {e}"));
    }
    s.push_str(&format!("; next {}", fmt_ts(r.next_action_at_ms)));
    s
}

fn describe_event(e: &qk::KeeperEvent) -> String {
    match e {
        qk::KeeperEvent::PhaseChanged { key, from, to } => format!("{key}: {from} -> {to}"),
        qk::KeeperEvent::PollFailed {
            key,
            detail,
            backoff_ms,
        } => format!("{key}: poll failed ({detail}); backoff {}", fmt_dur(*backoff_ms)),
        qk::KeeperEvent::NewWindow {
            key,
            from_generation,
            to_generation,
            idle_delay_ms,
            idle_delay_is_upper_bound,
            attribution,
        } => format!(
            "{key}: new weekly window verified (reset {}{}){}{}",
            fmt_ts(*to_generation),
            from_generation
                .map(|f| format!(", previous {}", fmt_ts(f)))
                .unwrap_or_default(),
            idle_delay_ms
                .map(|d| format!(
                    "; idle delay {}{}",
                    fmt_dur(d),
                    if *idle_delay_is_upper_bound { " (upper bound)" } else { "" }
                ))
                .unwrap_or_default(),
            match attribution {
                qk::Attribution::Observed => "".to_string(),
                qk::Attribution::AttemptCorrelated { attempt } =>
                    format!("; keeper attempt {attempt} correlated (not proven causal)"),
            }
        ),
        qk::KeeperEvent::LimitAssertedAfterReset { key, generation } => format!(
            "{key}: reset {} passed but the provider still asserts a limit; not activating",
            fmt_ts(*generation)
        ),
        qk::KeeperEvent::ActivationDue {
            key,
            generation,
            due_since_ms,
        } => format!(
            "{key}: ALERT window idle since reset {} (due since {})",
            fmt_ts(*generation),
            fmt_ts(*due_since_ms)
        ),
        qk::KeeperEvent::AttemptStarted {
            key,
            generation,
            attempt,
        } => format!(
            "{key}: activation attempt {attempt} started for generation {}",
            fmt_ts(*generation)
        ),
        qk::KeeperEvent::AttemptFinished {
            key,
            attempt,
            outcome,
            refunded,
            ..
        } => format!(
            "{key}: attempt {attempt} finished: {}{}",
            match outcome {
                qk::AttemptOutcome::Sent { http_status } => format!("sent (HTTP {http_status})"),
                qk::AttemptOutcome::Rejected { http_status } =>
                    format!("rejected (HTTP {http_status})"),
                qk::AttemptOutcome::NotSent { stage, reason } =>
                    format!("not sent at {stage:?} ({reason})"),
                qk::AttemptOutcome::Ambiguous { reason } => format!("ambiguous ({reason})"),
            },
            if *refunded { "; budget refunded" } else { "; budget consumed" }
        ),
        qk::KeeperEvent::Unverified {
            key,
            generation,
            attempts,
        } => format!(
            "{key}: ALERT {attempts} attempt(s) for generation {} unverified; not retrying",
            fmt_ts(*generation)
        ),
        qk::KeeperEvent::Rearmed {
            key,
            generation,
            previous_attempts,
        } => format!(
            "{key}: operator re-armed one attempt for generation {} (previous attempts: {previous_attempts})",
            fmt_ts(*generation)
        ),
        qk::KeeperEvent::AuthError { key, detail } => {
            format!("{key}: ALERT auth error ({detail}); re-login required")
        }
        qk::KeeperEvent::BankedResets {
            key,
            available_count,
            earliest_expiry_ms,
            expiring_soon,
        } => format!(
            "{key}: ALERT {available_count} banked weekly reset(s) available{}{} (inventory only; nothing is redeemed)",
            match earliest_expiry_ms {
                Some(e) => format!(", earliest expiry {}", fmt_ts(*e)),
                None => ", expiry not exposed by the usage endpoint".to_string(),
            },
            if *expiring_soon { " — EXPIRING SOON" } else { "" }
        ),
    }
}

// ── Entry point ──────────────────────────────────────────────────────────────

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

pub async fn run(args: QuotaKeeperArgs) -> Result<(), Box<dyn std::error::Error>> {
    let config = synaps_cli::config::load_config();
    let source = config.auth.credential_source();
    let canonical = canonical_keeper_dir(&source);
    let state_dir = args.state_dir.clone().unwrap_or_else(|| canonical.clone());
    if args.show_state {
        let state = qk::StateStore::new(&state_dir).load()?;
        println!("{}", serde_json::to_string_pretty(&state)?);
        return Ok(());
    }
    if args.state_dir.is_some() && !args.activate.is_empty() {
        return Err("--state-dir cannot be combined with --activate: the attempt ledger must \
                    live in the canonical keeper directory"
            .into());
    }
    let model = match (&args.model, args.activate.is_empty()) {
        (Some(m), _) => {
            let id = validate_codex_model(m)?;
            // Fail closed before any lock/state work if no minimal effort is authorized.
            let plan = activation_plan(&id)?;
            eprintln!(
                "keeper: activation model {id}, reasoning effort {} (no output-token ceiling on this endpoint)",
                plan.wire_effort_label()
            );
            Some(id)
        }
        (None, false) => return Err("--activate requires --model <codex-model-id>".into()),
        (None, true) => None,
    };
    let cfg = args.keeper_config();
    let source_label = match &source {
        synaps_cli::auth::CredentialSource::Local => {
            format!("local:{}", synaps_cli::auth::auth_file_path().display())
        }
        synaps_cli::auth::CredentialSource::Remote { endpoint, .. } => format!("remote:{endpoint}"),
    };
    if !args.activate.is_empty()
        && matches!(source, synaps_cli::auth::CredentialSource::Remote { .. })
    {
        return Err("activation is only permitted on the broker host (local credential source); \
                    a remote broker source is poll-only because attempt locks are host-local"
            .into());
    }
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()?;
    let broker = synaps_cli::auth::broker_from_source(&source, &synaps_cli::auth::TokenCache::new(), http);

    let (accounts, warnings) = resolve_accounts(
        broker.as_ref(),
        &source_label,
        &args.providers,
        &args.accounts,
        &args.activate,
        &args.rearm,
    )
    .await?;
    for w in &warnings {
        eprintln!("keeper: warning: {w}");
    }
    if accounts.is_empty() {
        return Err("no connected accounts with usage support; run `synaps login` first".into());
    }
    let activator: Arc<dyn Activator> =
        Arc::new(CodexActivator::new(broker.clone(), CODEX_RESPONSES_URL.to_string())?);
    let mut rt = KeeperRuntime::new(
        cfg,
        &state_dir,
        &canonical,
        accounts,
        broker,
        activator,
        Arc::new(SystemClock),
        Arc::new(StdoutSink),
        args.json,
        model,
    )?;
    if !args.json {
        let opted: Vec<String> = rt
            .accounts
            .iter()
            .filter(|a| a.opted_in)
            .map(|a| a.credential.storage_key())
            .collect();
        if opted.is_empty() {
            rt.note("read-only mode: polling and alerting only; no account is opted in to activation");
        } else {
            rt.note(&format!(
                "activation opted in for {} with model {}",
                opted.join(", "),
                rt.model.as_deref().unwrap_or("?")
            ));
        }
    }
    let cancel = CancellationToken::new();
    let cancel_on_signal = cancel.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        cancel_on_signal.cancel();
    });
    rt.run_loop(args.once, cancel).await
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use synaps_cli::auth::{
        AccessToken, ProviderStatus, ProxyByteStream, ProxyRequest, ProxyResponse,
    };

    const HOUR: u64 = 3_600_000;
    const DAY: u64 = 24 * HOUR;
    const WEEK: u64 = 7 * DAY;
    const NOW: u64 = 1_800_000_000_000;
    const MODEL: &str = "gpt-5.4-mini";

    // A structurally valid JWT-shaped token carrying a chatgpt_account_id
    // claim. Not a real credential.
    fn fake_codex_token(account_id: &str) -> String {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::json!({
                "https://api.openai.com/auth": { "chatgpt_account_id": account_id }
            })
            .to_string(),
        );
        format!("{header}.{payload}.sig")
    }

    struct FakeClock(AtomicU64);
    #[async_trait]
    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
        async fn sleep(&self, d: Duration) {
            self.0.fetch_add(d.as_millis() as u64, Ordering::SeqCst);
        }
    }

    #[derive(Default)]
    struct Captured(Mutex<Vec<String>>);
    impl Sink for Captured {
        fn line(&self, s: &str) {
            self.0.lock().unwrap().push(s.to_string());
        }
    }
    impl Captured {
        fn joined(&self) -> String {
            self.0.lock().unwrap().join("\n")
        }
    }

    /// Deterministic full provider account id for a mock label (one seat
    /// per label unless aliased). The first 8 chars are the prefix.
    fn seat_id(label: &str) -> String {
        format!("{label:0<8}-0000-4000-8000-000000000000")
    }
    fn seat_prefix(label: &str) -> String {
        seat_id(label).chars().take(8).collect()
    }

    fn snapshot(label: &str, observed: u64, weekly_used: f64, reset: Option<u64>) -> UsageSnapshot {
        let mut s: UsageSnapshot = serde_json::from_value(serde_json::json!({
            "schema_version": 1, "provider": "openai-codex", "account": label,
            "observed_at": observed, "plan": "plus", "limit_reached": weekly_used >= 100.0,
            "windows": [], "model_availability": [], "credits": null, "banked_resets": null,
            "identity_prefix": seat_prefix(label),
            "seat_fingerprint": synaps_cli::auth::seat_fingerprint(OAuthProviderId::OpenAiCodex, &seat_id(label)), "notes": []
        }))
        .unwrap();
        s.windows = vec![
            usage::UsageWindow {
                id: "primary".into(),
                label: "5h".into(),
                scope: usage::WindowScope::Account,
                duration_secs: Some(18_000),
                used_percent: usage::UsedPercent::from_f64(1.0),
                used: None,
                limit: None,
                reset_at: Some(observed + HOUR),
                reset_kind: usage::ResetKind::QuotaWindow,
                limit_reached: Some(false),
            },
            usage::UsageWindow {
                id: "secondary".into(),
                label: "1w".into(),
                scope: usage::WindowScope::Account,
                duration_secs: Some(604_800),
                used_percent: usage::UsedPercent::from_f64(weekly_used),
                used: None,
                limit: None,
                reset_at: reset,
                reset_kind: usage::ResetKind::QuotaWindow,
                limit_reached: Some(weekly_used >= 100.0),
            },
        ];
        s
    }

    /// Mock broker: scripted usage snapshots per account, fake tokens, no
    /// network, no auth.json. Each label is its own seat unless `alias`ed.
    /// The LISTED seat (metadata: prefix + fingerprint) and the TOKEN seat
    /// (claim in the vended JWT) are tracked separately so tests can make
    /// them disagree (re-login, stale metadata).
    struct MockBroker {
        labels: Vec<String>,
        /// label → full account id carried by the vended token ("" = no claim)
        seats: Mutex<HashMap<String, String>>,
        /// label → full account id the listing reports
        listed: Mutex<HashMap<String, String>>,
        /// labels listed WITHOUT prefix/fingerprint metadata
        hidden: Mutex<Vec<String>>,
        usage: Mutex<HashMap<String, VecDeque<Result<UsageSnapshot, BrokerError>>>>,
        usage_calls: Mutex<Vec<String>>,
        token_calls: Mutex<Vec<String>>,
    }

    impl MockBroker {
        fn new(labels: &[&str]) -> Self {
            let seats: HashMap<String, String> =
                labels.iter().map(|l| (l.to_string(), seat_id(l))).collect();
            Self {
                labels: labels.iter().map(|l| l.to_string()).collect(),
                listed: Mutex::new(seats.clone()),
                seats: Mutex::new(seats),
                hidden: Mutex::new(Vec::new()),
                usage: Mutex::new(HashMap::new()),
                usage_calls: Mutex::new(Vec::new()),
                token_calls: Mutex::new(Vec::new()),
            }
        }
        fn summary(&self, label: &str) -> AccountSummary {
            let listed = self.listed.lock().unwrap().get(label).cloned();
            let hidden = self.hidden.lock().unwrap().iter().any(|h| h == label);
            let account = Account::parse(label).unwrap();
            let mut s = AccountSummary::new(OAuthProviderId::OpenAiCodex, &account)
                .with_account_id(if hidden { None } else { listed.as_deref() });
            s.identity = Some(format!("{label}@example.test"));
            s.added_at = Some(1);
            s.selected = label == "default";
            s
        }
        fn script(&self, key: &str, items: Vec<Result<UsageSnapshot, BrokerError>>) {
            self.usage
                .lock()
                .unwrap()
                .entry(key.to_string())
                .or_default()
                .extend(items);
        }
        /// Make `label` a second alias of `of`'s seat (token AND listing).
        fn alias(&self, label: &str, of: &str) {
            self.seats.lock().unwrap().insert(label.to_string(), seat_id(of));
            self.listed.lock().unwrap().insert(label.to_string(), seat_id(of));
        }
        /// Re-login `label` to seat `new_seat` WITHOUT touching the listing
        /// (the keeper must notice from the token itself).
        fn relogin_token_only(&self, label: &str, new_seat: &str) {
            self.seats.lock().unwrap().insert(label.to_string(), seat_id(new_seat));
        }
        /// Vend a token with no account-id claim for `label`.
        fn drop_token_claim(&self, label: &str) {
            self.seats.lock().unwrap().insert(label.to_string(), String::new());
        }
        /// Listing names `other`'s seat while the token is unchanged.
        fn set_listed_seat(&self, label: &str, other: &str) {
            self.listed.lock().unwrap().insert(label.to_string(), seat_id(other));
        }
        /// List `label` without prefix/fingerprint metadata.
        fn hide_metadata(&self, label: &str) {
            self.hidden.lock().unwrap().push(label.to_string());
        }
    }

    #[async_trait]
    impl CredentialBroker for MockBroker {
        async fn access_token(&self, provider: OAuthProviderId) -> Result<AccessToken, BrokerError> {
            self.access_token_for(&CredentialRef::default_for(provider)).await
        }
        async fn access_token_for(&self, cred: &CredentialRef) -> Result<AccessToken, BrokerError> {
            self.token_calls.lock().unwrap().push(cred.storage_key());
            let label = cred.account.label_str();
            if !self.labels.iter().any(|l| l == label) {
                return Err(BrokerError::UnknownAccount {
                    provider: cred.provider.as_str().into(),
                    label: label.into(),
                });
            }
            let seat = self.seats.lock().unwrap().get(label).cloned();
            Ok(AccessToken {
                token: match seat.as_deref() {
                    Some("") => "no.claims.here".to_string(),
                    Some(id) => fake_codex_token(id),
                    None => fake_codex_token(&seat_id(label)),
                },
                expires: u64::MAX,
            })
        }
        async fn accounts(&self, provider: OAuthProviderId) -> Result<Vec<AccountSummary>, BrokerError> {
            if provider == OAuthProviderId::OpenAiCodex {
                Ok(self.labels.iter().map(|l| self.summary(l)).collect())
            } else {
                Ok(Vec::new())
            }
        }
        async fn usage(&self, cred: &CredentialRef) -> Result<UsageSnapshot, BrokerError> {
            let key = cred.storage_key();
            self.usage_calls.lock().unwrap().push(key.clone());
            let mut map = self.usage.lock().unwrap();
            match map.get_mut(&key).and_then(|q| {
                if q.len() > 1 {
                    q.pop_front()
                } else {
                    q.front().cloned()
                }
            }) {
                Some(r) => r,
                None => Err(BrokerError::Transport("no scripted usage".into())),
            }
        }
        async fn proxy(&self, _r: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
            Err(BrokerError::Denied("mock".into()))
        }
        async fn proxy_stream(&self, _r: ProxyRequest) -> Result<ProxyByteStream, BrokerError> {
            Err(BrokerError::Denied("mock".into()))
        }
        async fn anthropic_usage(&self) -> Result<serde_json::Value, BrokerError> {
            Err(BrokerError::Denied("mock".into()))
        }
        async fn capabilities(&self) -> Result<Vec<ProviderStatus>, BrokerError> {
            Ok(Vec::new())
        }
    }

    /// Scripted activator that records calls.
    struct FakeActivator {
        outcomes: Mutex<VecDeque<qk::AttemptOutcome>>,
        calls: Mutex<Vec<(String, String)>>,
    }
    #[async_trait]
    impl Activator for FakeActivator {
        async fn activate(
            &self,
            cred: &CredentialRef,
            expected_seat: &str,
            model: &str,
            _t: Duration,
        ) -> qk::AttemptOutcome {
            assert!(
                expected_seat.starts_with(qk::SEAT_FP_PREFIX),
                "runner must never activate without a strong seat identity"
            );
            self.calls
                .lock()
                .unwrap()
                .push((cred.storage_key(), model.to_string()));
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(qk::AttemptOutcome::Sent { http_status: 200 })
        }
    }

    struct Harness {
        broker: Arc<MockBroker>,
        activator: Arc<FakeActivator>,
        clock: Arc<FakeClock>,
        sink: Arc<Captured>,
        dir: tempfile::TempDir,
        rearm: Mutex<Vec<String>>,
    }

    impl Harness {
        fn new(labels: &[&str]) -> Self {
            Self {
                broker: Arc::new(MockBroker::new(labels)),
                activator: Arc::new(FakeActivator {
                    outcomes: Mutex::new(VecDeque::new()),
                    calls: Mutex::new(Vec::new()),
                }),
                clock: Arc::new(FakeClock(AtomicU64::new(NOW))),
                sink: Arc::new(Captured::default()),
                dir: tempfile::tempdir().unwrap(),
                rearm: Mutex::new(Vec::new()),
            }
        }
        async fn runtime(&self, activate: &[&str], model: Option<&str>) -> KeeperRuntime {
            self.runtime_in(&self.dir.path().join("state"), activate, model)
                .await
                .unwrap()
        }
        async fn runtime_in(
            &self,
            state_dir: &Path,
            activate: &[&str],
            model: Option<&str>,
        ) -> Result<KeeperRuntime, Box<dyn std::error::Error>> {
            let activate: Vec<String> = activate.iter().map(|s| s.to_string()).collect();
            let rearm: Vec<String> = self.rearm.lock().unwrap().clone();
            let (accounts, _warnings) = resolve_accounts(
                self.broker.as_ref(),
                "local:/tmp/test-auth.json",
                &[],
                &[],
                &activate,
                &rearm,
            )
            .await?;
            KeeperRuntime::new(
                qk::KeeperConfig::default().bounded(),
                state_dir,
                &self.dir.path().join("canonical"),
                accounts,
                self.broker.clone(),
                self.activator.clone(),
                self.clock.clone(),
                self.sink.clone(),
                false,
                model.map(str::to_string),
            )
        }
        fn set_time(&self, t: u64) {
            self.clock.0.store(t, Ordering::SeqCst);
        }
    }

    #[test]
    fn activation_body_mirrors_production_shape_without_tools_or_token_cap() {
        let plan = activation_plan("gpt-6-astra").unwrap();
        assert_eq!(plan.wire_effort_label(), "low");
        assert!(plan.multi_agent_mode.is_none(), "internal role never injects a mode item");
        let b = activation_body("gpt-6-astra", &plan);
        assert_eq!(b["model"], "gpt-6-astra");
        assert_eq!(b["store"], false);
        assert_eq!(b["stream"], true);
        assert!(b.get("tools").is_none(), "no tools ever");
        assert_eq!(b["tool_choice"], "auto");
        assert_eq!(b["parallel_tool_calls"], true);
        assert_eq!(b["include"], serde_json::json!(["reasoning.encrypted_content"]));
        assert_eq!(b["text"]["verbosity"], "low");
        assert_eq!(b["reasoning"]["effort"], "low");
        assert_eq!(b["prompt_cache_key"], ACTIVATION_PROMPT_CACHE_KEY);
        // Production omits max_output_tokens on this endpoint; so do we.
        assert!(b.get("max_output_tokens").is_none());
        let input = b["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["text"], ACTIVATION_INPUT);
        // Every catalog model gets a validated minimal effort.
        for m in synaps_cli::runtime::openai::catalog::codex_static_catalog_models() {
            let p = activation_plan(&m.id).unwrap_or_else(|e| panic!("{}: {e}", m.id));
            assert_eq!(p.wire_effort_label(), "low", "{}", m.id);
        }
        assert!(activation_plan("gpt-99-imaginary").is_err());
    }

    #[test]
    fn model_validation_requires_catalog_id() {
        assert_eq!(validate_codex_model("gpt-5.4-mini").unwrap(), "gpt-5.4-mini");
        assert_eq!(validate_codex_model("openai-codex/gpt-6-astra").unwrap(), "gpt-6-astra");
        assert!(validate_codex_model("").is_err());
        assert!(validate_codex_model("gpt-99-imaginary").is_err());
        assert!(validate_codex_model("claude-sonnet-4-6").is_err());
    }

    #[test]
    fn account_arg_parsing_is_strict() {
        assert_eq!(parse_account_arg("openai-codex").unwrap().storage_key(), "openai-codex");
        assert_eq!(
            parse_account_arg("openai-codex@astra2").unwrap().storage_key(),
            "openai-codex@astra2"
        );
        assert!(parse_account_arg("openai-codex@Bad Label").is_err());
        assert!(parse_account_arg("nope@x").is_err());
        assert!(parse_account_arg("openai-codex@default").is_err());
    }

    #[test]
    fn snapshot_mapping_preserves_unknowns_and_folds_flags() {
        let mut s = snapshot("a", NOW, 10.0, Some(NOW + DAY));
        s.windows.push(usage::UsageWindow {
            id: "code_review.primary".into(),
            label: "5h".into(),
            scope: usage::WindowScope::Feature {
                feature: "code_review".into(),
            },
            duration_secs: Some(18_000),
            used_percent: usage::UsedPercent::from_f64(100.0),
            used: None,
            limit: None,
            reset_at: None,
            reset_kind: usage::ResetKind::QuotaWindow,
            limit_reached: Some(true),
        });
        s.windows.push(usage::UsageWindow {
            id: "gpt-6-astra".into(),
            label: "1w".into(),
            scope: usage::WindowScope::Model {
                model: "gpt-6-astra".into(),
            },
            duration_secs: Some(604_800),
            used_percent: usage::UsedPercent::Unknown {
                reason: "missing".into(),
            },
            used: None,
            limit: None,
            reset_at: None,
            reset_kind: usage::ResetKind::QuotaWindow,
            limit_reached: None,
        });
        s.model_availability = vec![usage::ModelAvailability {
            model: "gpt-6-astra".into(),
            availability: usage::Availability::Unknown,
            used_percent: usage::UsedPercent::Unknown {
                reason: "missing".into(),
            },
            reset_at: None,
            credits_would_enable: Some(true),
            source: "model_usage".into(),
        }];
        s.spend_control_reached = Some(true);
        s.banked_resets = Some(usage::BankedResets {
            available_count: None,
            credits: vec![usage::BankedResetCredit {
                reset_type: None,
                granted_at: None,
                expires_at: Some(NOW + DAY),
            }],
            inventory_error: None,
        });
        let obs = observation_from_snapshot(&s);
        let qk::ObservationOutcome::Ok {
            windows,
            models,
            limit_reached,
            banked,
            identity_prefix,
        } = obs.outcome
        else {
            panic!("expected ok");
        };
        // feature window dropped; model window scoped; unknown percent stays None
        assert_eq!(windows.len(), 3);
        assert!(windows.iter().all(|w| w.id != "code_review.primary"));
        let mw = windows.iter().find(|w| w.id == "gpt-6-astra").unwrap();
        assert_eq!(mw.models, Some(vec!["gpt-6-astra".to_string()]));
        assert_eq!(mw.used_percent, None);
        assert_eq!(windows[0].duration_ms, Some(18_000_000));
        // Unknown availability preserved as Unknown, never Available
        assert_eq!(models.unwrap()[0].state, qp::ModelState::Unknown);
        // spend control folds into the overall flag
        assert_eq!(limit_reached, Some(true));
        let b = banked.unwrap();
        assert_eq!(b.available_count, None);
        assert_eq!(b.earliest_expiry_ms, Some(NOW + DAY));
        assert_eq!(identity_prefix.as_deref(), Some(seat_prefix("a").as_str()));
        assert_eq!(obs.observed_at_ms, NOW);
    }

    #[test]
    fn error_mapping_classifies_broker_errors() {
        let auth = observation_from_error(NOW, &BrokerError::Credential("usage: 401".into()));
        assert!(matches!(auth.outcome, qk::ObservationOutcome::AuthError { .. }));
        let unk = observation_from_error(
            NOW,
            &BrokerError::UnknownAccount {
                provider: "openai-codex".into(),
                label: "x".into(),
            },
        );
        assert!(matches!(unk.outcome, qk::ObservationOutcome::AuthError { .. }));
        let uns = observation_from_error(
            NOW,
            &BrokerError::UnsupportedCapability {
                provider: "github-copilot".into(),
                capability: "usage".into(),
            },
        );
        assert!(matches!(uns.outcome, qk::ObservationOutcome::Unsupported { .. }));
        let mal = observation_from_error(
            NOW,
            &BrokerError::Transport("usage: usage response malformed: rate_limit".into()),
        );
        assert!(matches!(mal.outcome, qk::ObservationOutcome::Malformed { .. }));
        let tr = observation_from_error(NOW, &BrokerError::Transport("usage: timed out".into()));
        assert!(matches!(tr.outcome, qk::ObservationOutcome::Transport { .. }));
        assert_eq!(tr.observed_at_ms, NOW);
    }

    #[tokio::test]
    async fn resolve_accounts_validates_filters_and_opt_ins() {
        let h = Harness::new(&["default", "astra2"]);
        let (all, warnings) = resolve_accounts(h.broker.as_ref(), "local:x", &[], &[], &[], &[])
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|a| !a.opted_in));
        // read-only rows take the LISTED seat fingerprint (strong, seat-canonical
        // key) and never vend a token
        assert!(all.iter().all(|a| a.identity.is_strong()));
        assert!(all.iter().all(|a| a.account_id_prefix.as_deref()
            == Some(seat_prefix(a.credential.account.label_str()).as_str())));
        assert!(h.broker.token_calls.lock().unwrap().is_empty());
        assert!(warnings.is_empty(), "{warnings:?}");
        // a listing without metadata yields a weak, label-scoped identity
        h.broker.hide_metadata("default");
        let (all, _) = resolve_accounts(h.broker.as_ref(), "local:x", &[], &[], &[], &[])
            .await
            .unwrap();
        let d = all.iter().find(|a| a.credential.storage_key() == "openai-codex").unwrap();
        assert!(!d.identity.is_strong());
        assert!(d.identity.identity_fp.starts_with("who:"), "{}", d.identity.identity_fp);
        assert_eq!(d.account_id_prefix, None);
        h.broker.hidden.lock().unwrap().clear();
        let (only, _) = resolve_accounts(
            h.broker.as_ref(),
            "local:x",
            &[],
            &["openai-codex@astra2".into()],
            &["openai-codex@astra2".into()],
            &[],
        )
        .await
        .unwrap();
        assert_eq!(only.len(), 1);
        assert!(only[0].opted_in);
        // an opted-in row has a STRONG identity from the vended token (digest, not the id)
        assert!(only[0].identity.is_strong());
        assert!(!only[0].identity.identity_fp.contains(&seat_prefix("astra2")));
        assert_eq!(only[0].account_id_prefix.as_deref(), Some(seat_prefix("astra2").as_str()));
        assert_eq!(h.broker.token_calls.lock().unwrap().as_slice(), ["openai-codex@astra2"]);
        // two labels on ONE seat: warning + tracked once while read-only …
        h.broker.alias("default", "astra2");
        let (kept, warnings) = resolve_accounts(h.broker.as_ref(), "local:x", &[], &[], &[], &[])
            .await
            .unwrap();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(kept.len(), 1, "one seat is tracked once");
        assert!(warnings[0].contains("tracked once"));
        // … an error as soon as EITHER alias is opted in, and for both
        for activate in [
            vec!["openai-codex@astra2".to_string()],
            vec!["openai-codex".to_string()],
            vec!["openai-codex@astra2".to_string(), "openai-codex".to_string()],
        ] {
            let err = resolve_accounts(h.broker.as_ref(), "local:x", &[], &[], &activate, &[])
                .await
                .err()
                .unwrap_or_default();
            assert!(err.contains("same provider seat"), "{activate:?}: {err}");
        }
        // excluding the alias with --account makes activation possible again
        assert!(resolve_accounts(
            h.broker.as_ref(),
            "local:x",
            &[],
            &["openai-codex@astra2".into()],
            &["openai-codex@astra2".into()],
            &[]
        )
        .await
        .is_ok());
        // unknown explicit account → error, never fallback
        assert!(resolve_accounts(
            h.broker.as_ref(),
            "local:x",
            &[],
            &["openai-codex@ghost".into()],
            &[],
            &[]
        )
        .await
        .is_err());
        // opt-in for a non-connected or non-codex account → error
        assert!(resolve_accounts(h.broker.as_ref(), "local:x", &[], &[], &["openai-codex@ghost".into()], &[])
            .await
            .is_err());
        assert!(resolve_accounts(h.broker.as_ref(), "local:x", &[], &[], &["anthropic".into()], &[])
            .await
            .is_err());
        // provider without usage support → error
        assert!(resolve_accounts(h.broker.as_ref(), "local:x", &["github-copilot".into()], &[], &[], &[])
            .await
            .is_err());
        // rearm without activate → error
        assert!(resolve_accounts(
            h.broker.as_ref(),
            "local:x",
            &[],
            &[],
            &[],
            &["openai-codex@astra2".into()]
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn read_only_default_never_activates_and_alerts_when_due() {
        let h = Harness::new(&["astra2"]);
        let g1 = NOW + HOUR;
        h.broker.script(
            "openai-codex@astra2",
            vec![
                Ok(snapshot("astra2", NOW, 100.0, Some(g1))),
                Ok(snapshot("astra2", g1 + 60_000, 0.0, Some(g1))),
            ],
        );
        let mut rt = h.runtime(&[], None).await;
        rt.pass().await.unwrap();
        assert!(rt.rows()[0].phase.starts_with("exhausted"));
        h.set_time(g1 + 60_000);
        for _ in 0..3 {
            rt.pass().await.unwrap();
            h.clock.0.fetch_add(rt.cfg.poll_interval_ms, Ordering::SeqCst);
        }
        assert!(rt.rows()[0].phase.starts_with("due"), "{}", rt.rows()[0].phase);
        assert!(h.activator.calls.lock().unwrap().is_empty(), "read-only must never activate");
        assert!(h.broker.token_calls.lock().unwrap().is_empty(), "read-only never vends a token");
        assert!(h.sink.joined().contains("ALERT window idle since reset"));
        assert!(h.sink.joined().contains("[read-only]"));
    }

    #[tokio::test]
    async fn usage_requires_full_paired_identity_not_a_matching_prefix() {
        for missing_identity in [false, true] {
            let h = Harness::new(&["astra2"]);
            let g1 = NOW + HOUR;
            let t = g1 + 60_000;
            let mut wrong = snapshot("astra2", t, 0.0, Some(g1));
            // Display prefix still matches. Missing or another full identity
            // must fail closed, even if the activation token is correct.
            wrong.seat_fingerprint = if missing_identity { None } else {
                synaps_cli::auth::seat_fingerprint(OAuthProviderId::OpenAiCodex, "different-full-seat")
            };
            h.broker.script("openai-codex@astra2", vec![
                Ok(snapshot("astra2", NOW, 100.0, Some(g1))), Ok(wrong),
            ]);
            let mut rt = h.runtime(&["openai-codex@astra2"], Some(MODEL)).await;
            rt.pass().await.unwrap();
            h.set_time(t);
            rt.pass().await.unwrap();
            assert!(h.activator.calls.lock().unwrap().is_empty());
            assert!(rt.rows()[0].phase.starts_with("unknown"), "{}", rt.rows()[0].phase);
        }
    }

    #[tokio::test]
    async fn opt_in_activates_once_and_verifies_from_fresh_usage() {
        let h = Harness::new(&["default", "astra2"]);
        let g1 = NOW + HOUR;
        let t = g1 + 60_000;
        let g2 = t + WEEK;
        h.broker.script(
            "openai-codex@astra2",
            vec![
                Ok(snapshot("astra2", NOW, 100.0, Some(g1))),
                Ok(snapshot("astra2", t, 0.0, Some(g1))), // due
                Ok(snapshot("astra2", t + 1000, 0.0, Some(g2))), // verification after attempt
            ],
        );
        h.broker.script(
            "openai-codex",
            vec![Ok(snapshot("default", NOW, 5.0, Some(NOW + 3 * DAY)))],
        );
        let mut rt = h.runtime(&["openai-codex@astra2"], Some(MODEL)).await;
        rt.pass().await.unwrap();
        h.set_time(t);
        rt.pass().await.unwrap();
        let calls = h.activator.calls.lock().unwrap().clone();
        assert_eq!(calls, vec![("openai-codex@astra2".to_string(), MODEL.to_string())]);
        let row = rt.rows().into_iter().find(|r| r.account == "openai-codex@astra2").unwrap();
        assert!(row.phase.contains("active"), "{}", row.phase);
        assert!(row.phase.contains("not proven causal"));
        assert_eq!(row.attempts_in_generation, 0);
        // the default (not opted in) account was polled but never activated
        let default_row = rt.rows().into_iter().find(|r| r.account == "openai-codex").unwrap();
        assert!(!default_row.opted_in);
        // more passes: nothing else fires
        for _ in 0..5 {
            h.clock.0.fetch_add(rt.cfg.poll_interval_ms, Ordering::SeqCst);
            rt.pass().await.unwrap();
        }
        assert_eq!(h.activator.calls.lock().unwrap().len(), 1);
        // state on disk is private and secret-free
        let raw = std::fs::read_to_string(rt.store.path()).unwrap();
        assert!(!raw.contains("eyJ") && !raw.contains("Bearer"));
        assert!(raw.contains("attempt_correlated"));
    }

    #[tokio::test]
    async fn provider_still_asserting_limit_after_reset_blocks_activation() {
        let h = Harness::new(&["astra2"]);
        let g1 = NOW + HOUR;
        let t = g1 + 60_000;
        h.broker.script(
            "openai-codex@astra2",
            vec![
                Ok(snapshot("astra2", NOW, 100.0, Some(g1))),
                Ok(snapshot("astra2", t, 100.0, Some(g1))),
            ],
        );
        let mut rt = h.runtime(&["openai-codex@astra2"], Some(MODEL)).await;
        rt.pass().await.unwrap();
        h.set_time(t);
        for _ in 0..4 {
            rt.pass().await.unwrap();
            h.clock.0.fetch_add(rt.cfg.poll_interval_ms, Ordering::SeqCst);
        }
        assert!(h.activator.calls.lock().unwrap().is_empty());
        assert!(rt.rows()[0].phase.contains("still asserts limit"), "{}", rt.rows()[0].phase);
        assert!(h.sink.joined().contains("not activating"));
    }

    #[tokio::test]
    async fn unauthorized_and_transport_failures_never_activate() {
        let h = Harness::new(&["astra2"]);
        let g1 = NOW + HOUR;
        let t = g1 + 60_000;
        h.broker.script(
            "openai-codex@astra2",
            vec![
                Ok(snapshot("astra2", NOW, 100.0, Some(g1))),
                Err(BrokerError::Transport("usage: timed out".into())),
                Err(BrokerError::Credential("usage: provider rejected the access token (HTTP 401)".into())),
            ],
        );
        let mut rt = h.runtime(&["openai-codex@astra2"], Some(MODEL)).await;
        rt.pass().await.unwrap();
        h.set_time(t);
        rt.pass().await.unwrap(); // transport error → backoff, no activation
        assert!(rt.rows()[0].last_error.as_deref().unwrap().contains("timed out"));
        let next = rt.rows()[0].next_action_at_ms;
        assert!(next > t);
        h.set_time(next);
        rt.pass().await.unwrap(); // 401 → auth error
        assert!(rt.rows()[0].phase.starts_with("auth error"));
        assert!(h.activator.calls.lock().unwrap().is_empty());
        assert!(h.sink.joined().contains("re-login required"));
    }

    #[tokio::test]
    async fn crash_after_begin_is_recovered_without_a_second_burn() {
        let h = Harness::new(&["astra2"]);
        let g1 = NOW + HOUR;
        let t = g1 + 60_000;
        h.broker.script(
            "openai-codex@astra2",
            vec![
                Ok(snapshot("astra2", NOW, 100.0, Some(g1))),
                Ok(snapshot("astra2", t, 0.0, Some(g1))),
            ],
        );
        let state_dir = h.dir.path().join("state");
        {
            let mut rt = h.runtime_in(&state_dir, &["openai-codex@astra2"], Some(MODEL)).await.unwrap();
            rt.pass().await.unwrap();
            h.set_time(t);
            // Simulate a crash between begin_attempt and finish_attempt by
            // persisting a begun attempt and dropping the runtime.
            let key = rt.accounts[0].identity.key();
            rt.poll(&key).await.unwrap();
            let (_ticket, _) = qk::begin_attempt(&mut rt.state, t, &key).unwrap();
            rt.store.save(&rt.state).unwrap();
        }
        // Restart (locks released by drop). The pending attempt counts.
        let mut rt = h.runtime_in(&state_dir, &["openai-codex@astra2"], Some(MODEL)).await.unwrap();
        assert!(h.sink.joined().contains("ambiguous (process restarted"));
        let c = rt.cfg.clone();
        let mut now = t + 1000;
        for _ in 0..40 {
            h.set_time(now);
            rt.pass().await.unwrap();
            now += c.verify_poll_ms;
        }
        assert!(h.activator.calls.lock().unwrap().is_empty(), "no second burn after crash");
        assert!(rt.rows()[0].phase.starts_with("unverified"), "{}", rt.rows()[0].phase);
        // Duplicate restart: still nothing.
        drop(rt);
        let mut rt = h.runtime_in(&state_dir, &["openai-codex@astra2"], Some(MODEL)).await.unwrap();
        h.set_time(now);
        rt.pass().await.unwrap();
        assert!(h.activator.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ambiguous_activation_outcome_consumes_budget() {
        let h = Harness::new(&["astra2"]);
        let g1 = NOW + HOUR;
        let t = g1 + 60_000;
        h.broker.script(
            "openai-codex@astra2",
            vec![
                Ok(snapshot("astra2", NOW, 100.0, Some(g1))),
                Ok(snapshot("astra2", t, 0.0, Some(g1))),
            ],
        );
        h.activator
            .outcomes
            .lock()
            .unwrap()
            .push_back(qk::AttemptOutcome::Ambiguous {
                reason: "request timed out".into(),
            });
        let mut rt = h.runtime(&["openai-codex@astra2"], Some(MODEL)).await;
        rt.pass().await.unwrap();
        h.set_time(t);
        let mut now = t;
        for _ in 0..60 {
            rt.pass().await.unwrap();
            now += rt.cfg.verify_poll_ms;
            h.set_time(now);
        }
        assert_eq!(h.activator.calls.lock().unwrap().len(), 1);
        assert!(rt.rows()[0].phase.starts_with("unverified"));
    }

    #[tokio::test]
    async fn explicit_rearm_allows_exactly_one_more_attempt() {
        let h = Harness::new(&["astra2"]);
        let g1 = NOW + HOUR;
        let t = g1 + 60_000;
        h.broker.script(
            "openai-codex@astra2",
            vec![
                Ok(snapshot("astra2", NOW, 100.0, Some(g1))),
                Ok(snapshot("astra2", t, 0.0, Some(g1))),
            ],
        );
        h.activator
            .outcomes
            .lock()
            .unwrap()
            .push_back(qk::AttemptOutcome::Ambiguous {
                reason: "request timed out".into(),
            });
        let state_dir = h.dir.path().join("state");
        let mut now = t;
        {
            let mut rt = h.runtime_in(&state_dir, &["openai-codex@astra2"], Some(MODEL)).await.unwrap();
            rt.pass().await.unwrap();
            h.set_time(t);
            for _ in 0..60 {
                rt.pass().await.unwrap();
                now += rt.cfg.verify_poll_ms;
                h.set_time(now);
            }
            assert!(rt.rows()[0].phase.starts_with("unverified"));
            assert_eq!(h.activator.calls.lock().unwrap().len(), 1);
        }
        // Restart WITHOUT --rearm: still nothing.
        {
            let mut rt = h.runtime_in(&state_dir, &["openai-codex@astra2"], Some(MODEL)).await.unwrap();
            rt.pass().await.unwrap();
            assert_eq!(h.activator.calls.lock().unwrap().len(), 1);
        }
        // Restart WITH --rearm: exactly one more attempt.
        h.rearm.lock().unwrap().push("openai-codex@astra2".into());
        h.broker.script(
            "openai-codex@astra2",
            vec![Ok(snapshot("astra2", now, 0.0, Some(g1)))],
        );
        let mut rt = h.runtime_in(&state_dir, &["openai-codex@astra2"], Some(MODEL)).await.unwrap();
        assert!(h.sink.joined().contains("re-armed one attempt"));
        for _ in 0..60 {
            rt.pass().await.unwrap();
            now += rt.cfg.verify_poll_ms;
            h.set_time(now);
            h.broker.script(
                "openai-codex@astra2",
                vec![Ok(snapshot("astra2", now, 0.0, Some(g1)))],
            );
        }
        assert_eq!(h.activator.calls.lock().unwrap().len(), 2);
        assert!(rt.rows()[0].phase.starts_with("unverified"));
    }

    #[tokio::test]
    async fn not_sent_is_retried_with_backoff_then_bounded() {
        let h = Harness::new(&["astra2"]);
        let g1 = NOW + HOUR;
        let t = g1 + 60_000;
        h.broker.script(
            "openai-codex@astra2",
            vec![
                Ok(snapshot("astra2", NOW, 100.0, Some(g1))),
                Ok(snapshot("astra2", t, 0.0, Some(g1))),
            ],
        );
        for _ in 0..10 {
            h.activator
                .outcomes
                .lock()
                .unwrap()
                .push_back(qk::AttemptOutcome::NotSent {
                    stage: qk::NotSentStage::Connect,
                    reason: "connect failed".into(),
                });
        }
        let mut rt = h.runtime(&["openai-codex@astra2"], Some(MODEL)).await;
        rt.pass().await.unwrap();
        let mut now = t;
        for _ in 0..40 {
            h.set_time(now);
            rt.pass().await.unwrap();
            now = rt.state.next_wake_ms().unwrap().max(now + 1000);
            // keep the observation fresh for the retry
            h.broker.script(
                "openai-codex@astra2",
                vec![Ok(snapshot("astra2", now, 0.0, Some(g1)))],
            );
        }
        let calls = h.activator.calls.lock().unwrap().len();
        assert_eq!(calls as u32, qk::MAX_NOT_SENT_PER_GENERATION);
        assert!(h.sink.joined().contains("budget refunded"));
    }

    #[tokio::test]
    async fn second_keeper_on_same_account_is_refused_even_with_other_state_dir() {
        let h = Harness::new(&["astra2"]);
        h.broker
            .script("openai-codex@astra2", vec![Ok(snapshot("astra2", NOW, 1.0, Some(NOW + DAY)))]);
        let _first = h.runtime_in(&h.dir.path().join("s1"), &[], None).await.unwrap();
        let second = h.runtime_in(&h.dir.path().join("s2"), &[], None).await;
        let err = second.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("already kept by another keeper"), "{err}");
        let same_dir = h.runtime_in(&h.dir.path().join("s1"), &[], None).await;
        assert!(same_dir.is_err());
    }

    #[tokio::test]
    async fn opt_in_without_model_is_refused() {
        let h = Harness::new(&["astra2"]);
        let err = h
            .runtime_in(&h.dir.path().join("s"), &["openai-codex@astra2"], None)
            .await
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.contains("--model"), "{err}");
    }

    #[tokio::test]
    async fn corrupt_state_fails_closed() {
        let h = Harness::new(&["astra2"]);
        let dir = h.dir.path().join("s");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(qk::STATE_FILE_NAME), b"{").unwrap();
        assert!(h.runtime_in(&dir, &[], None).await.is_err());
    }

    #[tokio::test]
    async fn run_loop_once_and_cancel_exit_cleanly() {
        let h = Harness::new(&["astra2"]);
        h.broker
            .script("openai-codex@astra2", vec![Ok(snapshot("astra2", NOW, 1.0, Some(NOW + DAY)))]);
        let mut rt = h.runtime(&[], None).await;
        rt.run_loop(true, CancellationToken::new()).await.unwrap();
        assert_eq!(h.broker.usage_calls.lock().unwrap().len(), 1);
        let cancel = CancellationToken::new();
        cancel.cancel();
        rt.run_loop(false, cancel).await.unwrap();
        assert!(rt.store.path().exists());
    }

    #[tokio::test]
    async fn json_output_is_machine_readable_and_secret_free() {
        let h = Harness::new(&["astra2"]);
        h.broker
            .script("openai-codex@astra2", vec![Ok(snapshot("astra2", NOW, 1.0, Some(NOW + DAY)))]);
        let (accounts, _) = resolve_accounts(h.broker.as_ref(), "local:x", &[], &[], &[], &[])
            .await
            .unwrap();
        let mut rt = KeeperRuntime::new(
            qk::KeeperConfig::default().bounded(),
            &h.dir.path().join("s"),
            &h.dir.path().join("c"),
            accounts,
            h.broker.clone(),
            h.activator.clone(),
            h.clock.clone(),
            h.sink.clone(),
            true,
            None,
        )
        .unwrap();
        rt.pass().await.unwrap();
        let out = h.sink.joined();
        for line in out.lines() {
            let v: serde_json::Value = serde_json::from_str(line).expect("json line");
            assert!(v.get("event").is_some());
        }
        assert!(out.contains("\"event\":\"pass\""));
        assert!(!out.contains("eyJ"));
    }

    // ── Loopback HTTP: the real CodexActivator ─────────────────────────────

    /// Minimal HTTP/1.1 server: records one request, replies with `status`.
    async fn one_shot_server(
        status: u16,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<(String, String, String)>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let (head, body_start) = loop {
                let n = sock.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break (String::from_utf8_lossy(&buf).to_string(), buf.len());
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break (String::from_utf8_lossy(&buf[..pos]).to_string(), pos + 4);
                }
            };
            let len: usize = head
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                .unwrap_or(0);
            while buf.len() < body_start + len {
                let n = sock.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let req_body = String::from_utf8_lossy(&buf[body_start..]).to_string();
            let resp = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            let _ = sock.shutdown().await;
            let path = head.lines().next().unwrap_or_default().to_string();
            (path, head, req_body)
        });
        (format!("http://{addr}/codex/responses"), handle)
    }

    #[tokio::test]
    async fn codex_activator_sends_pinned_tool_free_request_with_paired_headers() {
        let h = Harness::new(&["astra2"]);
        let (url, server) = one_shot_server(
            200,
            "event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n",
        )
        .await;
        let act = CodexActivator::new(h.broker.clone(), url).unwrap();
        let cred = parse_account_arg("openai-codex@astra2").unwrap();
        let seat = qk::AccountIdentity::seat_fingerprint(OAuthProviderId::OpenAiCodex, &seat_id("astra2")).unwrap();
        let outcome = act.activate(&cred, &seat, "gpt-5.4-mini", Duration::from_secs(5)).await;
        assert_eq!(outcome, qk::AttemptOutcome::Sent { http_status: 200 });
        let (path, head, body) = server.await.unwrap();
        assert!(path.starts_with("POST /codex/responses "), "{path}");
        let lower = head.to_ascii_lowercase();
        assert!(lower.contains(&format!("chatgpt-account-id: {}", seat_id("astra2"))));
        assert!(lower.contains("authorization: bearer "));
        assert!(lower.contains("openai-beta: responses=experimental"));
        assert!(lower.contains("originator: synaps"));
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v, activation_body("gpt-5.4-mini", &activation_plan("gpt-5.4-mini").unwrap()));
        assert!(v.get("tools").is_none());
        assert!(v.get("max_output_tokens").is_none());
        // token vend used the SAME credential the keeper tracks
        assert_eq!(h.broker.token_calls.lock().unwrap().as_slice(), ["openai-codex@astra2"]);
    }

    #[tokio::test]
    async fn codex_activator_treats_oversized_body_as_ambiguous() {
        let h = Harness::new(&["astra2"]);
        let cred = parse_account_arg("openai-codex@astra2").unwrap();
        let seat = qk::AccountIdentity::seat_fingerprint(OAuthProviderId::OpenAiCodex, &seat_id("astra2")).unwrap();
        let big: &'static str = Box::leak("x".repeat(ACTIVATION_MAX_BODY_BYTES + 1).into_boxed_str());
        let (url, server) = one_shot_server(200, big).await;
        let act = CodexActivator::new(h.broker.clone(), url).unwrap();
        assert!(matches!(
            act.activate(&cred, &seat, "gpt-5.4-mini", Duration::from_secs(5)).await,
            qk::AttemptOutcome::Ambiguous { reason } if reason.contains("drain cap")
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn filtered_out_account_keeps_its_spent_generation() {
        let h = Harness::new(&["default", "astra2"]);
        let g1 = NOW + HOUR;
        let t = g1 + 60_000;
        h.broker.script(
            "openai-codex@astra2",
            vec![
                Ok(snapshot("astra2", NOW, 100.0, Some(g1))),
                Ok(snapshot("astra2", t, 0.0, Some(g1))),
            ],
        );
        h.broker
            .script("openai-codex", vec![Ok(snapshot("default", NOW, 5.0, Some(NOW + 3 * DAY)))]);
        h.activator
            .outcomes
            .lock()
            .unwrap()
            .push_back(qk::AttemptOutcome::Ambiguous {
                reason: "request timed out".into(),
            });
        let state_dir = h.dir.path().join("state");
        let mut now = t;
        {
            let mut rt = h.runtime_in(&state_dir, &["openai-codex@astra2"], Some(MODEL)).await.unwrap();
            rt.pass().await.unwrap();
            h.set_time(t);
            for _ in 0..60 {
                rt.pass().await.unwrap();
                now += rt.cfg.verify_poll_ms;
                h.set_time(now);
            }
            assert_eq!(h.activator.calls.lock().unwrap().len(), 1);
        }
        // Run with ONLY the default account kept (astra2 filtered out).
        {
            let (accounts, _) = resolve_accounts(
                h.broker.as_ref(),
                "local:/tmp/test-auth.json",
                &[],
                &["openai-codex".into()],
                &[],
                &[],
            )
            .await
            .unwrap();
            let mut rt = KeeperRuntime::new(
                qk::KeeperConfig::default().bounded(),
                &state_dir,
                &h.dir.path().join("canonical"),
                accounts,
                h.broker.clone(),
                h.activator.clone(),
                h.clock.clone(),
                h.sink.clone(),
                false,
                None,
            )
            .unwrap();
            rt.pass().await.unwrap();
            assert_eq!(rt.rows().len(), 1);
            assert_eq!(rt.rows()[0].account, "openai-codex");
            assert!(rt.state.accounts.len() == 2, "ledger for astra2 retained");
        }
        // Restore astra2 with activation: the generation is still spent.
        h.broker
            .script("openai-codex@astra2", vec![Ok(snapshot("astra2", now, 0.0, Some(g1)))]);
        let mut rt = h.runtime_in(&state_dir, &["openai-codex@astra2"], Some(MODEL)).await.unwrap();
        rt.pass().await.unwrap();
        assert_eq!(h.activator.calls.lock().unwrap().len(), 1, "no second burn after filter switch");
        let row = rt.rows().into_iter().find(|r| r.account == "openai-codex@astra2").unwrap();
        assert!(row.phase.starts_with("unverified"), "{}", row.phase);
    }

    #[tokio::test]
    async fn codex_activator_classifies_statuses_and_pre_send_failures() {
        let h = Harness::new(&["astra2"]);
        let cred = parse_account_arg("openai-codex@astra2").unwrap();
        let seat = qk::AccountIdentity::seat_fingerprint(OAuthProviderId::OpenAiCodex, &seat_id("astra2")).unwrap();
        for (status, expect) in [
            (401u16, qk::AttemptOutcome::Rejected { http_status: 401 }),
            (429, qk::AttemptOutcome::Rejected { http_status: 429 }),
            (400, qk::AttemptOutcome::Rejected { http_status: 400 }),
        ] {
            let (url, server) = one_shot_server(status, "{}").await;
            let act = CodexActivator::new(h.broker.clone(), url).unwrap();
            assert_eq!(act.activate(&cred, &seat, "gpt-5.4-mini", Duration::from_secs(5)).await, expect);
            server.await.unwrap();
        }
        let (url, server) = one_shot_server(503, "busy").await;
        let act = CodexActivator::new(h.broker.clone(), url).unwrap();
        assert!(matches!(
            act.activate(&cred, &seat, "gpt-5.4-mini", Duration::from_secs(5)).await,
            qk::AttemptOutcome::Ambiguous { .. }
        ));
        server.await.unwrap();
        // Nothing listening → connect failure → NotSent (refundable).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = format!("http://{}/codex/responses", listener.local_addr().unwrap());
        drop(listener);
        let act = CodexActivator::new(h.broker.clone(), dead).unwrap();
        assert!(matches!(
            act.activate(&cred, &seat, "gpt-5.4-mini", Duration::from_secs(2)).await,
            qk::AttemptOutcome::NotSent { .. }
        ));
        // Unknown account → token vend fails → NotSent, no request.
        let ghost = parse_account_arg("openai-codex@ghost").unwrap();
        let (url, server) = one_shot_server(200, "").await;
        let act = CodexActivator::new(h.broker.clone(), url).unwrap();
        assert!(matches!(
            act.activate(&ghost, &seat, "gpt-5.4-mini", Duration::from_secs(2)).await,
            qk::AttemptOutcome::NotSent { stage: qk::NotSentStage::TokenVend, .. }
        ));
        server.abort();
        // Non-Codex provider → NotSent without any token vend.
        let before = h.broker.token_calls.lock().unwrap().len();
        let anthropic = parse_account_arg("anthropic").unwrap();
        let act = CodexActivator::new(h.broker.clone(), "http://127.0.0.1:9/x".into()).unwrap();
        assert!(matches!(
            act.activate(&anthropic, &seat, "gpt-5.4-mini", Duration::from_secs(1)).await,
            qk::AttemptOutcome::NotSent { stage: qk::NotSentStage::Unsupported, .. }
        ));
        assert_eq!(h.broker.token_calls.lock().unwrap().len(), before);
        // Weak/absent expected seat → NotSent before any token vend.
        let act = CodexActivator::new(h.broker.clone(), "http://127.0.0.1:9/x".into()).unwrap();
        assert!(matches!(
            act.activate(&cred, "id:astra200", "gpt-5.4-mini", Duration::from_secs(1)).await,
            qk::AttemptOutcome::NotSent { stage: qk::NotSentStage::IdentityMismatch, .. }
        ));
        assert_eq!(h.broker.token_calls.lock().unwrap().len(), before);
    }

    #[tokio::test]
    async fn codex_activator_refuses_a_token_from_another_seat_before_any_bytes_leave() {
        let h = Harness::new(&["astra2"]);
        let cred = parse_account_arg("openai-codex@astra2").unwrap();
        let tracked = qk::AccountIdentity::seat_fingerprint(OAuthProviderId::OpenAiCodex, &seat_id("astra2")).unwrap();
        // Re-login under the same label to another seat: the listing is
        // unchanged, only the token differs.
        h.broker.relogin_token_only("astra2", "other");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/codex/responses", listener.local_addr().unwrap());
        let act = CodexActivator::new(h.broker.clone(), url).unwrap();
        let outcome = act.activate(&cred, &tracked, "gpt-5.4-mini", Duration::from_secs(2)).await;
        assert!(
            matches!(outcome, qk::AttemptOutcome::NotSent { stage: qk::NotSentStage::IdentityMismatch, .. }),
            "{outcome:?}"
        );
        // Nothing connected to the server.
        let waited = tokio::time::timeout(Duration::from_millis(200), listener.accept()).await;
        assert!(waited.is_err(), "no request may reach the provider on a seat mismatch");
        // A token with no account-id claim is also refused before sending.
        h.broker.drop_token_claim("astra2");
        let outcome = act.activate(&cred, &tracked, "gpt-5.4-mini", Duration::from_secs(2)).await;
        assert!(matches!(outcome, qk::AttemptOutcome::NotSent { stage: qk::NotSentStage::AccountId, .. }));
    }

    #[tokio::test]
    async fn activation_requires_seat_identity_from_the_token_and_matching_metadata() {
        // Token without an account-id claim → opt-in refused at startup,
        // even though the LISTING carries a fingerprint.
        let h = Harness::new(&["astra2"]);
        h.broker.drop_token_claim("astra2");
        let err = h
            .runtime_in(&h.dir.path().join("s"), &["openai-codex@astra2"], Some(MODEL))
            .await
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.contains("seat identity"), "{err}");
        // Listed metadata names another seat than the token → refused.
        let h = Harness::new(&["astra2"]);
        h.broker.set_listed_seat("astra2", "stale");
        let err = h
            .runtime_in(&h.dir.path().join("s"), &["openai-codex@astra2"], Some(MODEL))
            .await
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.contains("different seats"), "{err}");
        // A listing without metadata is tolerated: the token is authoritative
        // and the identity is still strong (token-derived).
        let h = Harness::new(&["astra2"]);
        h.broker.hide_metadata("astra2");
        let rt = h.runtime(&["openai-codex@astra2"], Some(MODEL)).await;
        assert!(rt.accounts[0].identity.is_strong());
        assert_eq!(
            rt.accounts[0].identity.identity_fp,
            qk::AccountIdentity::seat_fingerprint(OAuthProviderId::OpenAiCodex, &seat_id("astra2")).unwrap()
        );
        assert_eq!(rt.accounts[0].account_id_prefix.as_deref(), Some(seat_prefix("astra2").as_str()));
    }

    #[tokio::test]
    async fn mid_run_relogin_to_another_seat_never_spends_and_disables_activation() {
        let h = Harness::new(&["astra2"]);
        let g1 = NOW + HOUR;
        let t = g1 + 60_000;
        h.broker.script(
            "openai-codex@astra2",
            vec![
                Ok(snapshot("astra2", NOW, 100.0, Some(g1))),
                Ok(snapshot("astra2", t, 0.0, Some(g1))), // due (seat A evidence)
            ],
        );
        // Real activator against a loopback listener that must stay silent.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/codex/responses", listener.local_addr().unwrap());
        let activator: Arc<dyn Activator> = Arc::new(CodexActivator::new(h.broker.clone(), url).unwrap());
        let (accounts, _) = resolve_accounts(
            h.broker.as_ref(),
            "local:/tmp/test-auth.json",
            &[],
            &[],
            &["openai-codex@astra2".into()],
            &[],
        )
        .await
        .unwrap();
        let mut rt = KeeperRuntime::new(
            qk::KeeperConfig::default().bounded(),
            &h.dir.path().join("state"),
            &h.dir.path().join("canonical"),
            accounts,
            h.broker.clone(),
            activator,
            h.clock.clone(),
            h.sink.clone(),
            false,
            Some(MODEL.into()),
        )
        .unwrap();
        rt.pass().await.unwrap();
        // Operator re-logs the label into seat B between the poll and the attempt.
        h.broker.relogin_token_only("astra2", "seatb");
        h.set_time(t);
        rt.pass().await.unwrap();
        let out = h.sink.joined();
        assert!(out.contains("identity_mismatch") || out.contains("IdentityMismatch"), "{out}");
        assert!(out.contains("activation DISABLED"), "{out}");
        assert!(!rt.accounts[0].opted_in, "opt-in dropped for the rest of the process");
        let row = &rt.rows()[0];
        assert_eq!(row.attempts_in_generation, 0, "nothing sent → refunded");
        assert!(row.phase.starts_with("due"), "{}", row.phase);
        // Nothing ever reached the listener, and later passes stay silent.
        for _ in 0..5 {
            h.clock.0.fetch_add(rt.cfg.poll_interval_ms, Ordering::SeqCst);
            h.broker.script(
                "openai-codex@astra2",
                vec![Ok(snapshot("astra2", h.clock.now_ms(), 0.0, Some(g1)))],
            );
            rt.pass().await.unwrap();
        }
        let waited = tokio::time::timeout(Duration::from_millis(200), listener.accept()).await;
        assert!(waited.is_err(), "no bytes may reach the provider for another seat");
        assert!(h.sink.joined().contains("[read-only]"));
    }

    #[tokio::test]
    async fn sequential_alias_switch_shares_one_ledger() {
        // Run 1: `astra2` opted in, attempt ambiguous → generation spent.
        let h = Harness::new(&["default", "astra2"]);
        h.broker.alias("default", "astra2");
        let g1 = NOW + HOUR;
        let t = g1 + 60_000;
        h.broker.script(
            "openai-codex@astra2",
            vec![
                Ok(snapshot("astra2", NOW, 100.0, Some(g1))),
                Ok(snapshot("astra2", t, 0.0, Some(g1))),
            ],
        );
        h.activator.outcomes.lock().unwrap().push_back(qk::AttemptOutcome::Ambiguous {
            reason: "request timed out".into(),
        });
        let state_dir = h.dir.path().join("state");
        let mut now = t;
        let key_a;
        {
            // The alias must be excluded with --account for activation to be allowed.
            let (accounts, _) = resolve_accounts(
                h.broker.as_ref(),
                "local:/tmp/test-auth.json",
                &[],
                &["openai-codex@astra2".into()],
                &["openai-codex@astra2".into()],
                &[],
            )
            .await
            .unwrap();
            key_a = accounts[0].identity.key();
            let mut rt = KeeperRuntime::new(
                qk::KeeperConfig::default().bounded(),
                &state_dir,
                &h.dir.path().join("canonical"),
                accounts,
                h.broker.clone(),
                h.activator.clone(),
                h.clock.clone(),
                h.sink.clone(),
                false,
                Some(MODEL.into()),
            )
            .unwrap();
            rt.pass().await.unwrap();
            h.set_time(t);
            for _ in 0..60 {
                rt.pass().await.unwrap();
                now += rt.cfg.verify_poll_ms;
                h.set_time(now);
            }
            assert_eq!(h.activator.calls.lock().unwrap().len(), 1);
            assert!(rt.rows()[0].phase.starts_with("unverified"));
        }
        // Run 2: the SAME seat under its other alias `default`, opted in.
        // The seat-canonical key finds the spent generation.
        h.broker.script(
            "openai-codex",
            vec![Ok(snapshot("astra2", now, 0.0, Some(g1)))],
        );
        let (accounts, _) = resolve_accounts(
            h.broker.as_ref(),
            "local:/tmp/test-auth.json",
            &[],
            &["openai-codex".into()],
            &["openai-codex".into()],
            &[],
        )
        .await
        .unwrap();
        assert_eq!(accounts[0].identity.key(), key_a, "alias shares the seat ledger");
        let mut rt = KeeperRuntime::new(
            qk::KeeperConfig::default().bounded(),
            &state_dir,
            &h.dir.path().join("canonical"),
            accounts,
            h.broker.clone(),
            h.activator.clone(),
            h.clock.clone(),
            h.sink.clone(),
            false,
            Some(MODEL.into()),
        )
        .unwrap();
        for _ in 0..5 {
            rt.pass().await.unwrap();
            now += rt.cfg.poll_interval_ms;
            h.set_time(now);
            h.broker.script("openai-codex", vec![Ok(snapshot("astra2", now, 0.0, Some(g1)))]);
        }
        assert_eq!(h.activator.calls.lock().unwrap().len(), 1, "no second burn via the other alias");
        assert_eq!(rt.state.accounts.len(), 1, "one ledger for one seat");
        let row = &rt.rows()[0];
        assert_eq!(row.account, "openai-codex");
        assert!(row.phase.starts_with("unverified"), "{}", row.phase);
        // The persisted entry now names the alias in use, still secret-free.
        let raw = std::fs::read_to_string(rt.store.path()).unwrap();
        let persisted: qk::KeeperState = serde_json::from_str(&raw).unwrap();
        assert_eq!(persisted.accounts[&key_a].identity.credential, "openai-codex");
        assert!(!raw.contains(&seat_id("astra2")));
    }

    #[tokio::test]
    async fn poll_evaluates_with_the_clock_after_the_fetch() {
        // The broker's snapshot is stamped when received; the clock advances
        // during the fetch. With the pre-fetch clock the reading would be
        // "in the future" (> skew) and rejected; after the fetch it is fresh.
        struct SlowBroker {
            inner: Arc<MockBroker>,
            clock: Arc<FakeClock>,
        }
        #[async_trait]
        impl CredentialBroker for SlowBroker {
            async fn access_token(&self, p: OAuthProviderId) -> Result<AccessToken, BrokerError> {
                self.inner.access_token(p).await
            }
            async fn access_token_for(&self, c: &CredentialRef) -> Result<AccessToken, BrokerError> {
                self.inner.access_token_for(c).await
            }
            async fn accounts(&self, p: OAuthProviderId) -> Result<Vec<AccountSummary>, BrokerError> {
                self.inner.accounts(p).await
            }
            async fn usage(&self, c: &CredentialRef) -> Result<UsageSnapshot, BrokerError> {
                // The fetch "takes" 10 minutes; the body is stamped on receipt.
                self.clock.0.fetch_add(10 * 60_000, Ordering::SeqCst);
                let mut s = self.inner.usage(c).await?;
                s.observed_at = self.clock.now_ms();
                Ok(s)
            }
            async fn proxy(&self, r: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
                self.inner.proxy(r).await
            }
            async fn proxy_stream(&self, r: ProxyRequest) -> Result<ProxyByteStream, BrokerError> {
                self.inner.proxy_stream(r).await
            }
            async fn anthropic_usage(&self) -> Result<serde_json::Value, BrokerError> {
                self.inner.anthropic_usage().await
            }
            async fn capabilities(&self) -> Result<Vec<ProviderStatus>, BrokerError> {
                self.inner.capabilities().await
            }
        }
        let h = Harness::new(&["astra2"]);
        h.broker
            .script("openai-codex@astra2", vec![Ok(snapshot("astra2", NOW, 5.0, Some(NOW + 3 * DAY)))]);
        let slow = Arc::new(SlowBroker {
            inner: h.broker.clone(),
            clock: h.clock.clone(),
        });
        let (accounts, _) = resolve_accounts(slow.as_ref(), "local:x", &[], &[], &[], &[])
            .await
            .unwrap();
        let mut rt = KeeperRuntime::new(
            qk::KeeperConfig::default().bounded(),
            &h.dir.path().join("s"),
            &h.dir.path().join("c"),
            accounts,
            slow.clone(),
            h.activator.clone(),
            h.clock.clone(),
            h.sink.clone(),
            false,
            None,
        )
        .unwrap();
        rt.pass().await.unwrap();
        let st = rt.state.accounts.values().next().unwrap();
        assert!(st.last_poll_ok, "reading must not be rejected as future-dated: {:?}", st.last_error);
        assert_eq!(st.last_poll_ms, Some(h.clock.now_ms()), "evaluated at the post-fetch clock");
        assert_eq!(st.last_ok_observation_ms, Some(h.clock.now_ms()));
        assert!(rt.rows()[0].phase.starts_with("active"), "{}", rt.rows()[0].phase);
    }
}
