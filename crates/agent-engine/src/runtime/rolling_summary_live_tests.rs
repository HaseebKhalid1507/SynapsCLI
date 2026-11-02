// Live acceptance of the warm rolling summary on Anthropic, included into
// `rolling_summary::transport_tests::warm_tests::live` (its helpers).
//
// Ignored, and inert unless SYNAPS_LIVE_SUMMARY is set:
// - `estimate`: the same harness against the loopback mock, no spend. It
//   prints each request's size, output limit and thinking settings for the
//   upfront worst-case estimate.
// - `run`: real requests on one pinned Anthropic seat, three single-attempt
//   trials (exact hit, controlled miss, near expiry).
// A second test, inert unless SYNAPS_LIVE_PROBE is set, sends one cold
// (screened) summary request only, with the same settings and guards, and
// reports its HTTP status and vetted error type; for a rejected request
// (4xx or an error envelope) it also prints the provider's error message,
// token-like runs redacted, at most 300 characters (harness only: the
// product records no provider text).
//
// Credentials: the runtime runs on a scratch base dir with a remote
// credential source pointed at this harness's own /token endpoint. That
// endpoint reads the pinned seat's access token and expiry from the user's
// auth.json ONCE (read-only, held in memory, never printed) and has no
// refresh code: a second vend, another seat, an automatic selection or a
// token too close to expiry is a hard error. The engine never opens the
// user's auth.json, so no OAuth refresh can happen.
//
// Money: ceilings live only here (per trial and in total), never in the
// product. Evidence: telemetry fields (numbers, path, terminal), the model
// and seat labels, counts. Never content, never credentials.

use std::sync::atomic::{AtomicUsize, Ordering};

const LIVE_MODEL: &str = "anthropic/claude-opus-5-5";
/// Harness-only money ceilings (USD).
const TRIAL_MAX_USD: f64 = 4.0;
const TOTAL_MAX_USD: f64 = 12.0;
/// Opus 5.5 list prices (USD per million tokens) used for the harness's own
/// worst case and running total: input, output; cache read 0.05x, 5m write
/// 1.25x, 1h write 2x.
const IN_USD: f64 = 4.0;
const OUT_USD: f64 = 20.0;
/// The pinned seat's token must outlive the run by this much.
const RUN_SECS: u64 = 20 * 60;
const SAFETY_SECS: u64 = 30 * 60;

fn live_mode() -> Option<String> {
    std::env::var("SYNAPS_LIVE_SUMMARY")
        .ok()
        .filter(|m| !m.is_empty())
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn now_ms() -> u64 {
    crate::epoch_millis()
}

// ── Synthetic window ────────────────────────────────────────────────────

/// `pairs` user batches of registry facts, each acknowledged, then one
/// user request for a one-word reply. The run nonce leads the first
/// message, so every trial seeds a prefix the provider has never cached.
fn live_history(nonce: u64, facts: usize, pairs: usize) -> (Vec<SharedMessage>, usize) {
    const COLORS: [&str; 8] = [
        "amber", "cobalt", "crimson", "jade", "ivory", "onyx", "saffron", "teal",
    ];
    const THINGS: [&str; 8] = [
        "lantern", "compass", "ledger", "kettle", "anchor", "prism", "spindle", "beacon",
    ];
    let mut state = nonce | 1;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let per = facts.div_ceil(pairs.max(1));
    let mut m = Vec::new();
    let mut id = 0;
    for p in 0..pairs {
        let mut text = if p == 0 {
            format!(
                "Registry run {nonce:016x}. Keep these registry facts; acknowledge each batch in one short line.\n"
            )
        } else {
            String::from("More registry facts:\n")
        };
        let first = id;
        while id < facts && id < first + per {
            let r = next();
            text.push_str(&format!(
                "F{id:04}: the {} {} in vault {} holds code {:06x}.\n",
                COLORS[(r % 8) as usize],
                THINGS[((r >> 3) % 8) as usize],
                (r >> 6) % 97,
                (r >> 16) & 0xff_ffff
            ));
            id += 1;
        }
        m.push(msg(json!({"role":"user","content":text})));
        m.push(msg(json!({"role":"assistant","content":[{"type":"text","text":format!("Noted F{first:04} to F{:04}.", id - 1)}]})));
    }
    m.push(msg(
        json!({"role":"user","content":"Reply with exactly the word: ready"}),
    ));
    (m, id)
}

/// How many of the `facts` seeded ids the summary text names.
fn fact_coverage(summary: &str, facts: usize) -> usize {
    (0..facts)
        .filter(|i| summary.contains(&format!("F{i:04}")))
        .count()
}

// ── The harness's token endpoint ─────────────────────────────────────────

#[derive(Clone)]
struct SeatVendor {
    label: String,
    token: Arc<String>,
    expires: u64,
    vends: Arc<AtomicUsize>,
    violations: Arc<Mutex<Vec<String>>>,
}

impl SeatVendor {
    fn vends(&self) -> usize {
        self.vends.load(Ordering::SeqCst)
    }
    fn violations(&self) -> Vec<String> {
        self.violations.lock().unwrap().clone()
    }
}

/// The pinned seat: the Anthropic slot whose token expires last, read once
/// from `auth_path`. `None` when no slot outlives the run plus the margin.
fn pin_seat(auth_path: &std::path::Path) -> Option<(String, String, u64)> {
    let raw = std::fs::read_to_string(auth_path).expect("auth.json readable");
    let file: Value = serde_json::from_str(&raw).expect("auth.json parses");
    drop(raw);
    let mut best: Option<(String, String, u64)> = None;
    for (key, slot) in file.as_object().expect("an object") {
        let label = match key.as_str() {
            "anthropic" => "default",
            k => match k.strip_prefix("anthropic@") {
                Some(l) => l,
                None => continue,
            },
        };
        if slot["type"] != "oauth" {
            continue;
        }
        let (Some(access), Some(expires)) = (slot["access"].as_str(), slot["expires"].as_u64())
        else {
            continue;
        };
        if access.is_empty() {
            continue;
        }
        if best.as_ref().is_none_or(|(_, _, e)| expires > *e) {
            best = Some((label.to_string(), access.to_string(), expires));
        }
    }
    best.filter(|(_, _, e)| *e > now_ms() + (RUN_SECS + SAFETY_SECS) * 1000)
}

async fn spawn_vendor(label: String, token: String, expires: u64) -> (SeatVendor, String) {
    use axum::extract::Query;
    let vendor = SeatVendor {
        label,
        token: Arc::new(token),
        expires,
        vends: Arc::default(),
        violations: Arc::default(),
    };
    let app = Router::new()
        .route(
            "/token",
            axum::routing::get(
                |State(v): State<SeatVendor>,
                 Query(q): Query<std::collections::HashMap<String, String>>| async move {
                    let n = v.vends.fetch_add(1, Ordering::SeqCst) + 1;
                    let account = q.get("account").cloned().unwrap_or_default();
                    let provider = q.get("provider").cloned().unwrap_or_default();
                    let refuse = |why: String| {
                        v.violations.lock().unwrap().push(why);
                        (StatusCode::CONFLICT, String::new())
                    };
                    if provider != "anthropic" || account != v.label {
                        return refuse(format!("vend for {provider}/{account}"));
                    }
                    if n > 1 {
                        return refuse(format!("re-vend #{n}"));
                    }
                    if v.expires <= now_ms() + SAFETY_SECS * 1000 {
                        return refuse("token near expiry".into());
                    }
                    let body = json!({
                        "access_token": v.token.as_str(),
                        "expires": v.expires,
                        "account": v.label,
                    });
                    (StatusCode::OK, body.to_string())
                },
            ),
        )
        .fallback(|uri: axum::http::Uri| async move {
            // Any other broker call (selection, proxy, usage) is recorded
            // as a violation by the caller's check of the vend count only;
            // answer it with an error and no data.
            let _ = uri;
            (StatusCode::NOT_FOUND, String::new())
        })
        .with_state(vendor.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (vendor, format!("http://{addr}"))
}

/// (sha256 hex, mtime ns) of a file: compared before and after the run.
fn file_fingerprint(path: &std::path::Path) -> (String, u128) {
    use sha2::Digest;
    let bytes = std::fs::read(path).expect("readable");
    let digest = sha2::Sha256::digest(&bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    (hex, mtime)
}

// ── One trial ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trial {
    ExactHit,
    ControlledMiss,
    NearExpiry,
}

impl Trial {
    fn name(self) -> &'static str {
        match self {
            Trial::ExactHit => "exact_hit",
            Trial::ControlledMiss => "controlled_miss",
            Trial::NearExpiry => "near_expiry",
        }
    }
}

/// The rollover thresholds for turn 2 from turn 1's calibration sample
/// (`provider` tokens measured, `estimated` by the conservative
/// estimator). Pressure and rollover are compared with the estimate of the
/// next request, so they are set in estimator units below turn 1's
/// estimate: turn 2 rolls over whatever the tokenizer ratio. The target is
/// in provider units (the policy converts it with the same sample) and
/// keeps about half of the window's history, so the summary's cap (at most
/// a fifth of the successor's message budget) is not tiny.
#[derive(Debug, Clone, Copy)]
struct Thresholds {
    pressure: u64,
    rollover: u64,
    target: u64,
    summary: u64,
}

fn thresholds_after(provider: u64, estimated: u64) -> Thresholds {
    // The request overhead (system prompt and tool schemas) in estimator
    // units, as the mock dry run measured it.
    let overhead = env_u64("SYNAPS_LIVE_OVERHEAD_EST", 10_000);
    let rollover = estimated * 95 / 100;
    let pressure = estimated * 90 / 100;
    let est_target = overhead + estimated.saturating_sub(overhead) / 2;
    let target =
        (u128::from(est_target) * u128::from(provider) / u128::from(estimated.max(1))) as u64;
    let target = target.min(pressure * 85 / 100);
    let summary = env_u64("SYNAPS_LIVE_SUMMARY_TOKENS", 3_000).min(target.saturating_sub(1));
    Thresholds {
        pressure,
        rollover,
        target,
        summary,
    }
}

fn live_runtime(endpoint: &str, ttl: crate::core::config::CacheTtl) -> crate::Runtime {
    let mut rt = crate::Runtime::new_headless();
    rt.set_model(LIVE_MODEL.to_string());
    rt.set_reasoning_level_explicit(agent_core::reasoning::ReasoningLevel::XHigh);
    rt.set_context_window(Some(1_000_000));
    rt.set_cache_ttl(ttl);
    // No retries; no product cost limit (the default config has none, and a
    // limit would change the summary's reservation path): the money
    // ceilings are the harness's own, checked against the upfront worst
    // case.
    rt.set_api_retries(0);
    rt.credential_source = crate::auth::CredentialSource::Remote {
        endpoint: endpoint.to_string(),
        machine_token: "live-harness".to_string(),
    };
    {
        // Turn 1 stays far below the thresholds; turn 2 gets real ones.
        let mut s = rt.continuation.lock().unwrap();
        s.config.mode = agent_core::config::ContextManagementMode::Auto;
        s.config.pressure_tokens = Some(300_000);
        s.config.rollover_tokens = Some(400_000);
        s.config.target_tokens = Some(200_000);
        s.config.summary_tokens = Some(3_000);
    }
    rt
}

/// Lowers `rt`'s thresholds for turn 2 from turn 1's calibration sample.
fn lower_thresholds(rt: &crate::Runtime) -> Thresholds {
    let mut s = rt.continuation.lock().unwrap();
    let c = s
        .token_calibration
        .expect("turn 1 left a calibration sample");
    let t = thresholds_after(c.provider_tokens, c.estimated_tokens);
    s.config.pressure_tokens = Some(t.pressure);
    s.config.rollover_tokens = Some(t.rollover);
    s.config.target_tokens = Some(t.target);
    s.config.summary_tokens = Some(t.summary);
    t
}

/// Every whitelisted field of the `context rollover` event (numbers, path,
/// terminal, reasons; no ids, labels of sessions or content).
const EVIDENCE_KEYS: &[&str] = &[
    "window",
    "summary",
    "summary_reason",
    "summary_attempted_path",
    "summary_warm_miss",
    "summary_terminal",
    "summary_limit_tokens",
    "summary_cache_age_s",
    "summary_reserved_microusd",
    "summary_settled_microusd",
    "summary_duration_ms",
    "summary_error_status",
    "provider_summary_input_tokens",
    "provider_summary_output_tokens",
    "provider_summary_cache_read_tokens",
    "provider_summary_cache_write_tokens",
    "provider_summary_cache_write_5m_tokens",
    "provider_summary_cache_write_1h_tokens",
    "summary_cache_read_pct",
    // The estimator against the provider at the rollover: the closing
    // window's estimate, the latest provider count and their ratio.
    "footprint_tokens",
    "provider_context_tokens",
    "calibration_ratio",
    "successor_tokens",
    "target_tokens",
];

/// `est=… provider=… ratio=…` of `rt`'s latest calibration sample (the
/// estimator's `used_tokens` for the last turn request against the
/// provider's input plus cache counts), or why there is none.
fn estimate_vs_provider(rt: &crate::Runtime) -> String {
    match rt.continuation.lock().unwrap().token_calibration {
        Some(c) => format!(
            "est={} provider={} est_over_provider={:.3}",
            c.estimated_tokens,
            c.provider_tokens,
            c.estimated_tokens as f64 / c.provider_tokens.max(1) as f64
        ),
        None => "no plausible sample".into(),
    }
}

/// One turn's measured usage, priced at the harness's list prices.
#[derive(Default, Debug, Clone, Copy)]
struct TurnSpend {
    input: u64,
    output: u64,
    read: u64,
    write: u64,
    write_5m: u64,
    write_1h: u64,
}

impl TurnSpend {
    fn usd(&self) -> f64 {
        let split = self.write_5m + self.write_1h;
        let (w5, w1) = if split == self.write {
            (self.write_5m, self.write_1h)
        } else {
            // Unsplit or inconsistent: price every write at the 1h rate.
            (0, self.write)
        };
        (self.input as f64 * IN_USD
            + self.read as f64 * IN_USD * 0.05
            + w5 as f64 * IN_USD * 1.25
            + w1 as f64 * IN_USD * 2.0
            + self.output as f64 * OUT_USD)
            / 1e6
    }
}

/// A turn with its full usage (the shared `turn` keeps three counters).
async fn live_turn(rt: &crate::Runtime, messages: Vec<SharedMessage>) -> (Turn, Vec<TurnSpend>) {
    let mut stream = rt
        .run_stream_with_messages(messages, Default::default(), None, None, false)
        .await;
    let mut out = Turn {
        history: Vec::new(),
        heads: Vec::new(),
        notices: Vec::new(),
        errors: Vec::new(),
        usages: Vec::new(),
    };
    let mut spend = Vec::new();
    while let Some(event) = stream.next().await {
        match event {
            agent_core::StreamEvent::Session(agent_core::SessionEvent::Error(e)) => {
                out.errors.push(e)
            }
            agent_core::StreamEvent::Session(agent_core::SessionEvent::MessageHistory(h)) => {
                out.history = h
            }
            agent_core::StreamEvent::Session(agent_core::SessionEvent::Notice(n)) => {
                out.notices.push(n)
            }
            agent_core::StreamEvent::Session(agent_core::SessionEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                cache_creation_input_tokens,
                cache_creation_5m,
                cache_creation_1h,
                model,
                ..
            }) => {
                out.usages.push((input_tokens, output_tokens, model));
                spend.push(TurnSpend {
                    input: input_tokens,
                    output: output_tokens,
                    read: cache_read_input_tokens,
                    write: cache_creation_input_tokens,
                    write_5m: cache_creation_5m.unwrap_or(0),
                    write_1h: cache_creation_1h.unwrap_or(0),
                });
            }
            agent_core::StreamEvent::Session(agent_core::SessionEvent::ContextHeadCheckpoint {
                messages,
                receipt,
                ..
            }) => {
                out.heads.push(messages);
                receipt.complete(Ok(()));
            }
            _ => {}
        }
    }
    (out, spend)
}

struct TrialOut {
    name: &'static str,
    gates: Vec<(String, bool)>,
    evidence: BTreeMap<String, String>,
    /// Measured where available, else the upper bound (the running total;
    /// the ceiling gate uses the upper bound).
    counted_usd: f64,
}

/// Numbers only, when turn 2 did not roll over: the thresholds, the
/// calibration sample, every measured usage and the `synaps::context`
/// events' numeric and code fields.
fn diagnose(
    trial: Trial,
    rt: &crate::Runtime,
    t: &Thresholds,
    spend1: &[TurnSpend],
    spend2: &[TurnSpend],
    events: &ContextEvents,
) {
    let name = trial.name();
    println!("LIVE-DIAG trial={name} thresholds={t:?}");
    let calibration = rt.continuation.lock().unwrap().token_calibration;
    println!("LIVE-DIAG trial={name} calibration={calibration:?}");
    println!("LIVE-DIAG trial={name} turn1={spend1:?}");
    println!("LIVE-DIAG trial={name} turn2={spend2:?}");
    let code = |v: &str| {
        v.parse::<f64>().is_ok()
            || (v.len() <= 40 && v.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
    };
    for (message, fields) in events.0.lock().unwrap().iter() {
        let kept: BTreeMap<&String, &String> = fields
            .iter()
            .filter(|(k, v)| k.as_str() != "session" && k.as_str() != "archive" && code(v))
            .collect();
        let message = if code(&message.replace(' ', "_")) {
            message.as_str()
        } else {
            "(other)"
        };
        println!("LIVE-DIAG trial={name} event={message:?} {kept:?}");
    }
}

/// One trial: turn 1 seeds and records the prefix (no rollover); turn 2,
/// with the thresholds lowered, rolls over and writes the summary before
/// the successor's first request.
async fn run_trial(
    trial: Trial,
    vendor_endpoint: &str,
    events: &ContextEvents,
    mock: Option<&WarmMock>,
) -> (TrialOut, crate::Runtime, Turn, Turn) {
    use crate::core::config::CacheTtl;
    use crate::runtime::telemetry::{TelemetryLevel, TelemetryWriter, WriterOptions};
    crate::runtime::helpers::MAX_TOKENS_FOR_TEST
        .with(|m| m.set(Some(env_u64("SYNAPS_LIVE_MAX_TOKENS", 16_000))));
    let mut rt = live_runtime(vendor_endpoint, CacheTtl::FiveMinutes);
    rt.set_telemetry_level(TelemetryLevel::Basic);
    let log = crate::config::base_dir().join(format!("api-log-{}.jsonl", trial.name()));
    let writer = TelemetryWriter::new(WriterOptions {
        telemetry_path: Some(log.clone()),
        ..WriterOptions::default()
    });
    rt.telemetry_writer = Some(writer.clone());
    let facts = env_u64("SYNAPS_LIVE_FACTS", 1_200) as usize;
    let pairs = env_u64("SYNAPS_LIVE_PAIRS", 8) as usize;
    let (start, seeded) = live_history(rand::random::<u64>(), facts, pairs);
    if let Some(mock) = mock {
        // Dry run: the mock reports a plausible prompt size for the turns
        // (about 3.2 bytes per token plus the measured overhead).
        let bytes: usize = start.iter().map(|m| m.to_string().len()).sum();
        // SYNAPS_LIVE_MOCK_BPT: bytes per token x10 (default 32).
        let prompt = bytes as u64 * 10 / env_u64("SYNAPS_LIVE_MOCK_BPT", 32).max(1)
            + env_u64("SYNAPS_LIVE_OVERHEAD_EST", 10_000);
        *mock.turn_usage.lock().unwrap() = Some(
            json!({"input_tokens":100,"output_tokens":0,"cache_creation_input_tokens":prompt,"cache_read_input_tokens":0}),
        );
    }

    events.0.lock().unwrap().clear();
    let (first, spend1) = live_turn(&rt, start).await;
    assert!(
        first.errors.is_empty(),
        "{}: turn 1 {:?}",
        trial.name(),
        first.errors
    );
    assert!(
        first.heads.is_empty(),
        "{}: no rollover in turn 1",
        trial.name()
    );
    if trial == Trial::NearExpiry {
        // Past TTL minus the 60 s margin (5 min TTL): the record is stale.
        tokio::time::sleep(Duration::from_secs(250)).await;
    }
    let turn1_ratio = estimate_vs_provider(&rt);
    let t = lower_thresholds(&rt);
    let mut next = first.history.clone();
    next.push(msg(
        json!({"role":"user","content":"Reply with exactly the word: done"}),
    ));
    if trial == Trial::ControlledMiss {
        crate::runtime::api::WARM_BODY_SALT_FOR_TEST.with(|s| s.set(Some(rand::random())));
    }
    let (second, spend2) = live_turn(&rt, next).await;
    let successor_ratio = estimate_vs_provider(&rt);
    crate::runtime::api::WARM_BODY_SALT_FOR_TEST.with(|s| s.set(None));
    if second.heads.is_empty() || !second.errors.is_empty() {
        diagnose(trial, &rt, &t, &spend1, &spend2, events);
    }
    assert!(
        second.errors.is_empty(),
        "{}: turn 2 {:?}",
        trial.name(),
        second.errors
    );
    assert!(
        !second.heads.is_empty(),
        "{}: turn 2 rolls over",
        trial.name()
    );

    let event = events.rollover();
    let mut evidence: BTreeMap<String, String> = EVIDENCE_KEYS
        .iter()
        .filter_map(|k| event.get(*k).map(|v| (k.to_string(), v.clone())))
        .collect();
    let summary = summary_of(&second.heads[0]).unwrap_or_default();
    evidence.insert("turn1_request_estimate_vs_provider".into(), turn1_ratio);
    evidence.insert(
        "successor_request_estimate_vs_provider".into(),
        successor_ratio,
    );
    // Raw, whether or not the sample was plausible: the rollover's
    // successor estimate against the provider's count for the successor
    // request (the last usage of turn 2).
    let successor_prompt = spend2.last().map_or(0, |s| s.input + s.read + s.write);
    evidence.insert(
        "successor_estimate_vs_last_turn2_provider".into(),
        format!(
            "est={} provider={successor_prompt}",
            event.get("successor_tokens").map_or("-", String::as_str)
        ),
    );
    // The summary request has no estimator count of its own: the warm one
    // replays turn 1's request plus the instruction, the cold one is a
    // separate screened request. Its provider prompt count:
    let prompt: u64 = [
        "provider_summary_input_tokens",
        "provider_summary_cache_read_tokens",
        "provider_summary_cache_write_tokens",
    ]
    .iter()
    .filter_map(|k| event.get(*k).and_then(|v| v.parse::<u64>().ok()))
    .sum();
    evidence.insert(
        "summary_request_provider_prompt_tokens".into(),
        prompt.to_string(),
    );
    evidence.insert(
        "thresholds".into(),
        format!(
            "pressure={} rollover={} target={} summary={}",
            t.pressure, t.rollover, t.target, t.summary
        ),
    );
    evidence.insert("seeded_facts".into(), seeded.to_string());
    evidence.insert(
        "summary_fact_ids_named".into(),
        fact_coverage(&summary, seeded).to_string(),
    );
    evidence.insert("summary_chars".into(), summary.chars().count().to_string());
    let prefix_tokens: u64 = spend1.iter().map(|s| s.input + s.read + s.write).sum();
    evidence.insert("turn1_prompt_tokens".into(), prefix_tokens.to_string());
    let fmt = |s: &[TurnSpend]| {
        s.iter()
            .map(|s| {
                format!(
                    "in={} read={} write={} (5m={} 1h={}) out={}",
                    s.input, s.read, s.write, s.write_5m, s.write_1h, s.output
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    evidence.insert("turn1_usage".into(), fmt(&spend1));
    evidence.insert("turn2_usage".into(), fmt(&spend2));
    let settled = event
        .get("summary_settled_microusd")
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or_else(|| {
            event
                .get("summary_reserved_microusd")
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0)
        })
        / 1e6;
    // Actual: every measured request of the trial (turns and the summary,
    // each with one usage event) at list prices. Upper bound: the summary's
    // booked cost is added again.
    let actual = spend1
        .iter()
        .chain(&spend2)
        .map(TurnSpend::usd)
        .sum::<f64>();
    let spend_usd = actual + settled;
    evidence.insert("trial_usd_actual".into(), format!("{actual:.4}"));
    evidence.insert("trial_usd_upper".into(), format!("{spend_usd:.4}"));
    // The running total counts the measured spend when every request of
    // the trial was measured (the summary settled, or none was sent), the
    // upper bound otherwise.
    let measured = event.contains_key("summary_settled_microusd")
        || event.get("summary_attempted_path").map(String::as_str) == Some("none");
    let counted_usd = if measured { actual } else { spend_usd };
    evidence.insert("trial_usd_counted".into(), format!("{counted_usd:.4}"));
    let timeout_ms = rt
        .continuation
        .lock()
        .unwrap()
        .config
        .summary_timeout_secs
        .max(1)
        .saturating_mul(1_000);
    evidence.insert("summary_timeout_ms".into(), timeout_ms.to_string());

    // API log: the warm request's record carries the event's summary id.
    assert!(writer.shutdown(Duration::from_secs(5)).is_flushed());
    let id = event.get("summary_id").cloned();
    let tagged: Vec<String> = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|r| {
            r.get("rollover_summary")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect();
    let correlated = id.is_some() && tagged.len() == 1 && tagged.first() == id.as_ref();
    evidence.insert(
        "api_log_rollover_summary_match".into(),
        correlated.to_string(),
    );

    let num = |k: &str| evidence.get(k).and_then(|v| v.parse::<u64>().ok());
    let field = |k: &str| evidence.get(k).map(String::as_str);
    let mut gates = Vec::new();
    let mut gate = |name: &str, ok: bool| gates.push((name.to_string(), ok));
    match trial {
        Trial::ExactHit => {
            gate("path=warm", field("summary_attempted_path") == Some("warm"));
            gate("summary=written", field("summary") == Some("written"));
            gate(
                "terminal=written",
                field("summary_terminal") == Some("written"),
            );
            gate("summary text nonempty", num("summary_chars").is_some_and(|c| c > 0));
            gate(
                "duration < timeout",
                num("summary_duration_ms").is_some_and(|d| d < timeout_ms),
            );
            gate(
                "cache_write=0",
                num("provider_summary_cache_write_tokens") == Some(0),
            );
            gate(
                "cache_read_pct>=90",
                num("summary_cache_read_pct").is_some_and(|p| p >= 90),
            );
            gate(
                "settled<=reserved",
                num("summary_settled_microusd") <= num("summary_reserved_microusd")
                    && num("summary_settled_microusd").is_some(),
            );
            gate("api_log_correlation", correlated);
        }
        Trial::ControlledMiss => {
            gate("path=warm", field("summary_attempted_path") == Some("warm"));
            gate(
                "cache_write>0",
                num("provider_summary_cache_write_tokens").is_some_and(|w| w > 0),
            );
            gate(
                "reads absent or reduced (<90%)",
                num("summary_cache_read_pct").is_none_or(|p| p < 90),
            );
            gate(
                "settled<=reserved",
                num("summary_settled_microusd") <= num("summary_reserved_microusd")
                    && num("summary_settled_microusd").is_some(),
            );
            gate(
                "5m/1h split booked",
                match (
                    num("provider_summary_cache_write_tokens"),
                    num("provider_summary_cache_write_5m_tokens"),
                    num("provider_summary_cache_write_1h_tokens"),
                ) {
                    (Some(w), Some(a), b) => a + b.unwrap_or(0) == w,
                    (Some(w), None, Some(b)) => b == w,
                    _ => false,
                },
            );
        }
        Trial::NearExpiry => {
            gate("path=cold", field("summary_attempted_path") == Some("cold"));
            gate(
                "warm_miss=stale",
                field("summary_warm_miss") == Some("stale"),
            );
            gate("summary=written", field("summary") == Some("written"));
            gate(
                "no full-window replay (cold input < turn-1 prompt)",
                num("provider_summary_input_tokens").is_some_and(|i| i < prefix_tokens),
            );
            gate(
                "no read share claimed",
                num("summary_cache_read_pct").is_none_or(|p| p < 90),
            );
            let settled = num("summary_settled_microusd");
            gate(
                "settled<=reserved",
                settled.is_some() && settled <= num("summary_reserved_microusd"),
            );
        }
    }
    gate("trial within ceiling", spend_usd <= TRIAL_MAX_USD);
    (
        TrialOut {
            name: trial.name(),
            gates,
            evidence,
            counted_usd,
        },
        rt,
        first,
        second,
    )
}

/// The estimate: the mock run's requests, with the worst case per trial at
/// list prices (every input token at the 1h write rate, every output
/// token up to the request's limit).
fn print_estimate(bodies: &[Value]) -> f64 {
    let mut worst = 0.0;
    for (i, body) in bodies.iter().enumerate() {
        let bytes = body.to_string().len() as u64;
        // A hard bound: a token covers at least one byte of the body.
        let input = bytes;
        let max_out = body["max_tokens"].as_u64().unwrap_or(0);
        let usd = (input as f64 * IN_USD * 2.0 + max_out as f64 * OUT_USD) / 1e6;
        worst += usd;
        let kind = if body.to_string().contains(WARM_MARK) {
            "warm_summary"
        } else if body["stream"] == true {
            "turn"
        } else {
            "cold_summary"
        };
        println!(
            "LIVE-ESTIMATE request={i} kind={kind} bytes={bytes} input_tokens_bound={input} max_tokens={max_out} thinking={} output_config={} worst_usd={usd:.3}",
            body["thinking"], body["output_config"]
        );
    }
    worst
}

#[tokio::test]
#[ignore = "live acceptance: SYNAPS_LIVE_SUMMARY=estimate|run"]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn live_anthropic_warm_summary_acceptance() {
    let Some(mode) = live_mode() else {
        eprintln!("SYNAPS_LIVE_SUMMARY not set: nothing to do");
        return;
    };
    let home = std::env::var("HOME").expect("HOME");
    let real_auth = std::path::PathBuf::from(&home)
        .join(".synaps-cli")
        .join("auth.json");
    assert!(
        std::env::var("SYNAPS_ANTHROPIC_BASE_URL").is_err(),
        "refusing: an Anthropic base URL override is set"
    );
    let _base = crate::test_env::BaseDirGuard::new();
    assert_ne!(
        crate::config::base_dir(),
        std::path::PathBuf::from(&home).join(".synaps-cli"),
        "refusing: not on a scratch base dir"
    );
    let (events, _guard) = ContextEvents::install();

    let only = std::env::var("SYNAPS_LIVE_TRIALS").unwrap_or_default();
    let selected = |trial: Trial| only.is_empty() || only.split(',').any(|t| t == trial.name());
    if mode == "estimate" {
        // The exact live sequence against the loopback mock: no spend. The
        // gates are informational here (the mock always answers from cache).
        let (mock, _g) = spawn_warm().await;
        let _seat = crate::test_env::EnvVarGuard::set("SYNAPS_ACCOUNT_ANTHROPIC", "live");
        let mut total = 0.0;
        for trial in [Trial::ExactHit, Trial::ControlledMiss, Trial::NearExpiry] {
            if !selected(trial) {
                continue;
            }
            let before = mock.requests.lock().unwrap().len();
            let (vendor, endpoint) = spawn_vendor(
                "live".into(),
                "sk-ant-fake-estimate".into(),
                9_999_999_999_999,
            )
            .await;
            let (out, _rt, _first, _second) =
                run_trial(trial, &endpoint, &events, Some(&mock)).await;
            println!(
                "LIVE-ESTIMATE trial={} gates (mock): {:?}",
                out.name, out.gates
            );
            println!(
                "LIVE-ESTIMATE trial={} evidence (mock): {:?}",
                out.name, out.evidence
            );
            let worst = print_estimate(&mock.bodies()[before..]);
            println!("LIVE-ESTIMATE trial={} worst_usd={worst:.3}", out.name);
            total += worst;
            assert_eq!(vendor.vends(), 1, "{:?}", vendor.violations());
            assert!(vendor.violations().is_empty(), "{:?}", vendor.violations());
        }
        println!("LIVE-ESTIMATE all_trials_worst_usd={total:.3}");
        return;
    }
    assert_eq!(mode, "run", "SYNAPS_LIVE_SUMMARY is estimate or run");

    let before = file_fingerprint(&real_auth);
    let (label, token, expires) = pin_seat(&real_auth)
        .expect("refusing: no Anthropic seat whose token outlives the run plus 30 min");
    let _seat = crate::test_env::EnvVarGuard::set("SYNAPS_ACCOUNT_ANTHROPIC", &label);
    println!(
        "LIVE seat={label} model={LIVE_MODEL} token_valid_for_min={}",
        (expires - now_ms()) / 60_000
    );
    // Spend of earlier runs (measured where known, else the upper bound,
    // USD) counts against the total.
    let mut total: f64 = std::env::var("SYNAPS_LIVE_SPENT_USD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    let mut all_pass = true;
    for trial in [Trial::ExactHit, Trial::ControlledMiss, Trial::NearExpiry] {
        if !selected(trial) {
            continue;
        }
        assert!(
            total + TRIAL_MAX_USD <= TOTAL_MAX_USD,
            "refusing {}: total ceiling",
            trial.name()
        );
        let (vendor, endpoint) = spawn_vendor(label.clone(), token.clone(), expires).await;
        let (out, _rt, _first, _second) = run_trial(trial, &endpoint, &events, None).await;
        total += out.counted_usd;
        let vend_ok = vendor.vends() == 1 && vendor.violations().is_empty();
        println!(
            "LIVE trial={} vends={} violations={}",
            out.name,
            vendor.vends(),
            vendor.violations().len()
        );
        for (k, v) in &out.evidence {
            println!("LIVE trial={} {k}={v}", out.name);
        }
        for (g, ok) in out.gates.iter().chain(std::iter::once(&(
            "one vend, no refresh".to_string(),
            vend_ok,
        ))) {
            println!(
                "LIVE trial={} gate {g}: {}",
                out.name,
                if *ok { "PASS" } else { "FAIL" }
            );
            all_pass &= *ok;
        }
        if !all_pass {
            // Stop: no further trial, no retry.
            println!("LIVE stopped after trial={}: a gate failed", out.name);
            break;
        }
    }
    drop(token);
    let after = file_fingerprint(&real_auth);
    println!(
        "LIVE auth.json sha256_match={} mtime_match={} total_usd={total:.4}",
        before.0 == after.0,
        before.1 == after.1
    );
    assert!(all_pass, "a gate failed: see the LIVE lines");
}

// ── Cold-only probe ──────────────────────────────────────────────────────

/// What the probe saw: no text, no provider message.
struct ProbeOut {
    lines: Vec<String>,
    /// Measured cost when the provider reported usage, else the
    /// reservation (the upper bound the product holds), USD.
    counted_usd: f64,
}

/// One screened (cold) summary through `update()`, with the near-expiry
/// trial's session settings (Opus 5.5, adaptive xhigh, 1M window, one
/// pinned seat) and synthetic window, but no turn: one turn-start
/// credential refresh binds the seat (the one vend), then exactly one
/// summary request goes out. Reports the HTTP status or failure class and
/// the vetted error type, never provider text.
async fn cold_probe(vendor_endpoint: &str, mock: Option<&WarmMock>) -> ProbeOut {
    use crate::core::config::CacheTtl;
    crate::runtime::helpers::MAX_TOKENS_FOR_TEST
        .with(|m| m.set(Some(env_u64("SYNAPS_LIVE_MAX_TOKENS", 16_000))));
    let rt = live_runtime(vendor_endpoint, CacheTtl::FiveMinutes);
    rt.refresh_if_needed().await.expect("the pinned seat is bound");
    let auth_type = rt.auth.read().await.auth_type.clone();
    let facts = env_u64("SYNAPS_LIVE_FACTS", 1_200) as usize;
    let pairs = env_u64("SYNAPS_LIVE_PAIRS", 8) as usize;
    let (start, _seeded) = live_history(rand::random::<u64>(), facts, pairs);
    // The window as a rollover drops it: every message but the last
    // request.
    let dropped = start[..start.len() - 1].to_vec();
    let cap = env_u64("SYNAPS_LIVE_PROBE_CAP", 2_519);
    let summarizer = rt.rollover_summarizer().expect("a summarizer");
    let usage = crate::runtime::budget::UsageCounters::default();
    let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = tokio_util::sync::CancellationToken::new();
    let session = rt.continuation.lock().unwrap().logical_id.clone();
    let archive = format!("{:032x}", rand::random::<u128>());
    let before = mock.map_or(0, |m| m.requests.lock().unwrap().len());
    *crate::runtime::api_sync::ERROR_BODY_FOR_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    let started = std::time::Instant::now();
    let (out, report) = crate::runtime::rolling_summary::update_with_report(
        &summarizer,
        crate::runtime::rolling_summary::SummaryRequest {
            logical_id: &session,
            previous: None,
            dropped: &dropped,
            window: 1,
            archive_id: &archive,
            cap_tokens: cap,
            timeout: Duration::from_secs(120),
            cancel: &cancel,
            accounting: Some(crate::runtime::rolling_summary::SummaryAccounting {
                usage: &usage,
                max_cost_usd: None,
                events: &events,
                tap: None,
                ledger: None,
            }),
            snapshot: Some(ws::SnapshotGate::new(&rt.continuation)),
            run_deadline: None,
        },
    )
    .await;
    let ms = started.elapsed().as_millis();
    let mut lines = vec![
        format!("model={}", summarizer.model()),
        format!("auth_type={auth_type}"),
        format!("dropped_messages={}", dropped.len()),
        format!("path={:?}", report.path),
        format!("terminal={}", report.terminal.unwrap_or("-")),
        format!(
            "error_status={}",
            report.error.map_or("-".into(), |f| f.class.label())
        ),
        format!(
            "error_type={}",
            report.error.and_then(|f| f.error_type).unwrap_or("-")
        ),
        format!("duration_ms={ms}"),
        format!("limit_tokens={}", report.limit_tokens.unwrap_or(0)),
        format!(
            "reserved_usd={:.4}",
            report.reserved_usd.unwrap_or(0.0)
        ),
        format!("settled_usd={:?}", report.settled_usd),
        format!(
            "usage_input={:?} usage_output={:?}",
            report.usage.and_then(|u| u.input),
            report.usage.and_then(|u| u.output)
        ),
        format!(
            "outcome={}",
            match &out {
                Ok(text) => format!("written chars={}", text.chars().count()),
                Err(skip) => format!("not written ({skip:?})"),
            }
        ),
    ];
    if let Some(mock) = mock {
        // The request's shape (no content): what a hypothesis needs.
        let seen = mock.requests.lock().unwrap()[before..].to_vec();
        for s in &seen {
            let body: Value = serde_json::from_str(&s.body).unwrap();
            let roles: Vec<&str> = body["messages"]
                .as_array()
                .map(|m| m.iter().filter_map(|m| m["role"].as_str()).collect())
                .unwrap_or_default();
            lines.push(format!(
                "shape bytes={} max_tokens={} thinking={} output_config={} stream={} system_blocks={} roles={roles:?} content_is_string={} beta={} auth_header={}",
                s.body.len(),
                body["max_tokens"],
                body["thinking"],
                body["output_config"],
                body["stream"],
                body["system"].as_array().map_or(0, Vec::len),
                body["messages"][0]["content"].is_string(),
                s.headers.get("anthropic-beta").map_or("-", String::as_str),
                if s.headers.contains_key("authorization") {
                    "authorization"
                } else if s.headers.contains_key("x-api-key") {
                    "x-api-key"
                } else {
                    "none"
                },
            ));
        }
        lines.push(format!("worst_usd={:.3}", print_estimate(&mock.bodies()[before..])));
    }
    // Harness only: why a sent request was rejected (a 4xx or an error
    // envelope), the provider's message redacted and truncated.
    let rejected = report.error.is_some_and(|f| match f.class {
        crate::runtime::api_sync::FailureClass::Status(s) => (400..500).contains(&s),
        crate::runtime::api_sync::FailureClass::Envelope => true,
        _ => false,
    });
    let body = crate::runtime::api_sync::ERROR_BODY_FOR_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let (true, Some(body)) = (rejected, body) {
        lines.push(format!("provider_error={}", probe_error_line(&body)));
    }
    let counted_usd = report
        .settled_usd
        .unwrap_or_else(|| report.reserved_usd.unwrap_or(0.0));
    ProbeOut { lines, counted_usd }
}

/// A rejected request's error, for the probe's evidence: the provider's
/// `error.type` and `error.message` (the raw body when it is not an error
/// envelope), anything token-like redacted, at most 300 characters.
fn probe_error_line(body: &str) -> String {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let error = &parsed["error"];
    let text = match (error["type"].as_str(), error["message"].as_str()) {
        (kind, Some(message)) => format!("type={} message={message}", kind.unwrap_or("-")),
        _ => format!("raw={body}"),
    };
    redact_token_like(&text).chars().take(300).collect()
}

/// Replaces `sk-…` keys, the word after `Bearer`, and any run of 24 or more
/// base64/hex-like characters with `[REDACTED]`.
fn redact_token_like(text: &str) -> String {
    let token = regex::Regex::new(
        r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]+|\bsk-[A-Za-z0-9_-]+|[A-Za-z0-9+/_-]{24,}=*",
    )
    .expect("valid pattern");
    token.replace_all(text, "[REDACTED]").into_owned()
}

#[test]
fn the_probe_error_line_hides_token_like_runs() {
    let body = json!({"type":"error","error":{"type":"invalid_request_error","message":"bad: sk-ant-oat01-abcDEF_12 and Bearer abc.def and c2VjcmV0c2VjcmV0c2VjcmV0c2VjcmV0 and deadbeefdeadbeefdeadbeefdeadbeef; `max_tokens` too small"}}).to_string();
    let line = probe_error_line(&body);
    assert!(line.starts_with("type=invalid_request_error message=bad: "), "{line}");
    for secret in ["sk-ant", "abcDEF", "abc.def", "c2VjcmV0", "deadbeef"] {
        assert!(!line.contains(secret), "{line}");
    }
    assert!(line.contains("`max_tokens` too small"), "{line}");
    let long = probe_error_line(&"x ".repeat(400));
    assert!(long.starts_with("raw=") && long.chars().count() == 300);
}

#[tokio::test]
#[ignore = "live probe: SYNAPS_LIVE_PROBE=estimate|run"]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn live_anthropic_cold_summary_probe() {
    let Some(mode) = std::env::var("SYNAPS_LIVE_PROBE")
        .ok()
        .filter(|m| !m.is_empty())
    else {
        eprintln!("SYNAPS_LIVE_PROBE not set: nothing to do");
        return;
    };
    let home = std::env::var("HOME").expect("HOME");
    let real_auth = std::path::PathBuf::from(&home)
        .join(".synaps-cli")
        .join("auth.json");
    assert!(
        std::env::var("SYNAPS_ANTHROPIC_BASE_URL").is_err(),
        "refusing: an Anthropic base URL override is set"
    );
    let _base = crate::test_env::BaseDirGuard::new();
    assert_ne!(
        crate::config::base_dir(),
        std::path::PathBuf::from(&home).join(".synaps-cli"),
        "refusing: not on a scratch base dir"
    );
    if mode == "estimate" {
        let (mock, _g) = spawn_warm().await;
        if std::env::var("SYNAPS_LIVE_PROBE_MOCK_FAIL").is_ok() {
            // Exercise the rejection path at no cost.
            *mock.fail.lock().unwrap() = Some((
                400,
                json!({"type":"error","error":{"type":"invalid_request_error","message":"mock rejection near sk-ant-fake-estimate"}}).to_string(),
            ));
        }
        let _seat = crate::test_env::EnvVarGuard::set("SYNAPS_ACCOUNT_ANTHROPIC", "live");
        let (vendor, endpoint) = spawn_vendor(
            "live".into(),
            "sk-ant-fake-estimate".into(),
            9_999_999_999_999,
        )
        .await;
        let out = cold_probe(&endpoint, Some(&mock)).await;
        for line in &out.lines {
            println!("LIVE-PROBE-ESTIMATE {line}");
        }
        assert_eq!(vendor.vends(), 1, "{:?}", vendor.violations());
        assert!(vendor.violations().is_empty(), "{:?}", vendor.violations());
        return;
    }
    assert_eq!(mode, "run", "SYNAPS_LIVE_PROBE is estimate or run");
    let spent: f64 = std::env::var("SYNAPS_LIVE_SPENT_USD")
        .ok()
        .and_then(|v| v.parse().ok())
        .expect("refusing: SYNAPS_LIVE_SPENT_USD (the running total) is required");
    let probe_max: f64 = std::env::var("SYNAPS_LIVE_PROBE_MAX_USD")
        .ok()
        .and_then(|v| v.parse().ok())
        .expect("refusing: SYNAPS_LIVE_PROBE_MAX_USD (the probe's upper bound) is required");
    // The probe and one later near-expiry trial must both fit the total.
    assert!(
        probe_max <= TRIAL_MAX_USD && spent + probe_max + TRIAL_MAX_USD <= TOTAL_MAX_USD,
        "refusing: total ceiling"
    );
    let before = file_fingerprint(&real_auth);
    let (label, token, expires) = pin_seat(&real_auth)
        .expect("refusing: no Anthropic seat whose token outlives the run plus 30 min");
    let _seat = crate::test_env::EnvVarGuard::set("SYNAPS_ACCOUNT_ANTHROPIC", &label);
    println!(
        "LIVE-PROBE seat={label} model={LIVE_MODEL} token_valid_for_min={}",
        (expires - now_ms()) / 60_000
    );
    let (vendor, endpoint) = spawn_vendor(label.clone(), token.clone(), expires).await;
    drop(token);
    let out = cold_probe(&endpoint, None).await;
    for line in &out.lines {
        println!("LIVE-PROBE {line}");
    }
    let vend_ok = vendor.vends() == 1 && vendor.violations().is_empty();
    println!(
        "LIVE-PROBE vends={} violations={} counted_usd={:.4} total_usd={:.4}",
        vendor.vends(),
        vendor.violations().len(),
        out.counted_usd,
        spent + out.counted_usd
    );
    let after = file_fingerprint(&real_auth);
    println!(
        "LIVE-PROBE auth.json sha256_match={} mtime_match={}",
        before.0 == after.0,
        before.1 == after.1
    );
    assert!(vend_ok, "one vend, no refresh");
}
