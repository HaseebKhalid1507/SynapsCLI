// A v2 worker's context settings on its runtime, included into
// `rolling_summary::transport_tests::warm_tests::worker_tests`
// (`docs/specs/subagent-context-rollover.md` §2, §7.2; slice 7, dark). The
// parent's live settings are snapshotted as configured, resolved for the
// worker's model and window, and applied to the worker's runtime; its
// summary model is then chosen by route capability (the worker's own model
// first, then the inherited compaction model) and a chosen model without a
// credential is skipped, never replaced. Sessions keep their choice. Every
// endpoint is the loopback mock.
use crate::runtime::rolling_summary::SummarySkip;
use crate::tools::ledger_record::{LedgerEntry, RequestPurpose};
use crate::tools::{InheritedWindow, LedgerSlot, RequestLedgerHandle, WorkerContextPolicy};
use agent_core::compaction::{CompactionMode, ContentClass};
use agent_core::config::{ContextManagementConfig, ContextManagementMode};
use agent_core::core::context_policy::ContextThresholds;

const SONNET: &str = "claude-sonnet-4-6";
const CODEX: &str = "openai-codex/gpt-5.5";
const GROQ: &str = "groq/llama-3.3-70b-versatile";
const LOCAL: &str = "local/test-model";

/// A parent's live settings: context management `auto`, nothing else set.
fn auto_parent() -> WorkerContextPolicy {
    WorkerContextPolicy {
        context_management: ContextManagementConfig {
            mode: ContextManagementMode::Auto,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A worker runtime on `model` given `policy` resolved for it, as a v2
/// run's start applies it.
fn worker_on(model: &str, policy: &WorkerContextPolicy) -> crate::Runtime {
    let mut rt = crate::Runtime::new_headless();
    rt.set_model(model.to_string());
    rt.set_api_retries(0);
    let context = policy.resolve(model).expect("resolves");
    rt.apply_worker_context(&context);
    rt
}

fn proxied(mut rt: crate::Runtime) -> crate::Runtime {
    rt.credential_source = crate::auth::CredentialSource::Remote {
        endpoint: "http://127.0.0.1:9".to_string(),
        machine_token: "machine-token".to_string(),
    };
    rt
}

/// m1–m5, m12: the worker's own model first, then the inherited
/// compaction model (deduplicated), chosen by route capability only; the
/// chosen model's credential is checked after, and a missing one skips the
/// summary instead of choosing again. An explicit model is used alone.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn m1_m5_a_workers_summary_model_is_chosen_by_capability_then_checked_for_credentials() {
    use SummarySkip::*;
    let _base = crate::test_env::BaseDirGuard::new();
    let _k = crate::test_env::EnvVarGuard::set("GROQ_API_KEY", "");
    let choice = |worker: &str, compaction: Option<&str>, explicit: Option<&str>| {
        let policy = WorkerContextPolicy {
            compaction_model: compaction.map(str::to_owned),
            summary_model: explicit.map(str::to_owned),
            ..auto_parent()
        };
        worker_on(worker, &policy).summary_model_choice()
    };
    let ok = |m: &str, n| Ok((m.to_string(), n));
    // m1: the worker's own model first, so the warm path can reuse its cache.
    assert_eq!(choice(SONNET, None, None), ok(SONNET, None));
    assert_eq!(choice(SONNET, Some(LOCAL), None), ok(SONNET, None));
    assert_eq!(choice(LOCAL, Some(SONNET), None), ok(LOCAL, None));
    // m2: an ineligible own model: the inherited compaction model, said why;
    // unset, it is the default the parent would resolve.
    assert_eq!(
        choice(CODEX, Some(LOCAL), None),
        ok(LOCAL, Some(CompactionModelForCeiling))
    );
    assert_eq!(
        choice(CODEX, None, None),
        ok(SONNET, Some(CompactionModelForCeiling)),
        "the default compaction model"
    );
    assert_eq!(
        choice("plugin:provider:model", Some(LOCAL), None),
        ok(LOCAL, Some(CompactionModelForCeiling))
    );
    assert_eq!(
        proxied(worker_on(LOCAL, &auto_parent())).summary_model_choice(),
        ok(SONNET, Some(CompactionModelForRemoteProxy)),
        "a proxied route is a capability"
    );
    // m3: both ineligible: skipped, never a third model.
    assert_eq!(
        choice(CODEX, Some("openai-codex/gpt-5.4"), None),
        Err(NoEnforceableCeiling)
    );
    assert_eq!(choice(CODEX, Some(""), None), Err(NoEnforceableCeiling));
    // m12: a compaction model equal to the worker's own is not tried twice.
    assert_eq!(choice(CODEX, Some(CODEX), None), Err(NoEnforceableCeiling));
    // m4: an explicit model only, even when it is ineligible.
    assert_eq!(choice(CODEX, Some(SONNET), Some(LOCAL)), ok(LOCAL, None));
    assert_eq!(
        choice(SONNET, Some(LOCAL), Some(CODEX)),
        Err(NoEnforceableCeiling)
    );
    assert_eq!(
        choice(SONNET, None, Some("plugin:provider:model")),
        Err(ExtensionHosted)
    );
    // m5: the chosen model has no credential: skipped, never replaced.
    assert_eq!(
        choice(GROQ, Some(LOCAL), None),
        Err(MissingCredentials),
        "the worker's own model is chosen; its key is missing"
    );
    assert_eq!(choice(CODEX, Some(GROQ), None), Err(MissingCredentials));
    assert_eq!(choice(SONNET, None, Some(GROQ)), Err(MissingCredentials));
    let invalid = WorkerContextPolicy {
        summary_model_invalid: true,
        compaction_model: Some(LOCAL.into()),
        ..auto_parent()
    };
    assert_eq!(
        worker_on(SONNET, &invalid).summary_model_choice(),
        Err(InvalidSummaryModel)
    );
    // Sessions are unchanged: availability still re-chooses there.
    let mut session = crate::Runtime::new_headless();
    session.set_model(GROQ.to_string());
    session.set_compaction_model(Some(LOCAL.to_string()));
    assert_eq!(
        session.summary_model_choice(),
        ok(LOCAL, Some(CompactionModelForCredentials))
    );
}

/// m6: the parent's model is never added as a candidate; a compaction
/// model configured equal to it is still the configured candidate.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn m6_a_worker_never_summarizes_with_its_parents_model_implicitly() {
    let _base = crate::test_env::BaseDirGuard::new();
    const PARENT: &str = "claude-opus-4-6";
    let mut parent = crate::Runtime::new_headless();
    parent.set_model(PARENT.to_string());
    parent.continuation.lock().unwrap().config.mode = ContextManagementMode::Auto;
    let policy = parent.worker_context_policy();
    assert_eq!(policy.compaction_model, None, "copied as configured: unset");
    assert_eq!(policy.summary_model, None, "the setting (auto), not a summarizer");
    for worker in [CODEX, LOCAL, SONNET, "plugin:provider:model"] {
        let chosen = worker_on(worker, &policy).summary_model_choice();
        assert!(
            chosen.as_ref().is_ok_and(|(model, _)| model != PARENT),
            "{worker}: {chosen:?}"
        );
    }
    parent.set_compaction_model(Some(PARENT.to_string()));
    assert_eq!(
        worker_on(CODEX, &parent.worker_context_policy()).summary_model_choice(),
        Ok((
            PARENT.to_string(),
            Some(SummarySkip::CompactionModelForCeiling)
        )),
        "the configured compaction model, not an exclusion by id"
    );
}

/// m11: the snapshot is the parent's live setting when it is taken, and a
/// copy: a later change to the parent reaches only a later snapshot (a
/// resume takes its own).
#[test]
fn m11_the_snapshot_is_the_parents_live_setting_and_a_copy() {
    let mut parent = crate::Runtime::new_headless();
    parent.set_model(SONNET.to_string());
    parent.set_context_window(Some(150_000));
    parent.set_summary_model(Some(LOCAL.to_string()));
    parent.set_compaction_mode(CompactionMode::LocalOnly);
    parent.set_compaction_exclusions(vec![ContentClass::ToolResults]);
    parent.continuation.lock().unwrap().config.mode = ContextManagementMode::Auto;
    let snapshot = parent.worker_context_policy();
    assert_eq!(
        snapshot,
        WorkerContextPolicy {
            context_management: ContextManagementConfig {
                mode: ContextManagementMode::Auto,
                ..Default::default()
            },
            summary_model: Some(LOCAL.into()),
            summary_model_invalid: false,
            compaction_model: None,
            compaction_mode: CompactionMode::LocalOnly,
            compaction_exclusions: vec![ContentClass::ToolResults],
            window_override: Some(InheritedWindow {
                tokens: 150_000,
                model: SONNET.into(),
            }),
            operator_rollover: Default::default(),
            resume_refuse_tokens: Some(250_000),
            worker_target_tokens: None,
        }
    );
    parent.set_summary_model(None);
    parent.continuation.lock().unwrap().config.mode = ContextManagementMode::Off;
    assert_eq!(snapshot.summary_model.as_deref(), Some(LOCAL), "a copy");
    let later = parent.worker_context_policy();
    assert_eq!(later.summary_model, None);
    assert_eq!(later.context_management.mode, ContextManagementMode::Off, "live");
}

/// m8, m9: applying the worker context gives the runtime the parent's
/// disclosure policy, its window override (same model), its continuation
/// settings resolved for the worker, and the worker summary choice.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn m8_m9_applying_the_worker_context_gives_the_runtime_the_resolved_settings() {
    let _base = crate::test_env::BaseDirGuard::new();
    let config = ContextManagementConfig {
        mode: ContextManagementMode::Auto,
        pressure_tokens: Some(60_000),
        rollover_tokens: Some(80_000),
        target_tokens: Some(25_000),
        summary_tokens: Some(4_000),
        reserve_tokens: 20_000,
        finish_rounds: 3,
        summary_timeout_secs: 60,
    };
    let policy = WorkerContextPolicy {
        context_management: config,
        compaction_mode: CompactionMode::LocalOnly,
        compaction_exclusions: vec![ContentClass::ToolResults, ContentClass::Thinking],
        window_override: Some(InheritedWindow {
            tokens: 150_000,
            model: SONNET.into(),
        }),
        ..Default::default()
    };
    let rt = worker_on(SONNET, &policy);
    assert_eq!(rt.context_window(), 150_000);
    assert_eq!(rt.continuation.lock().unwrap().config, config);
    let disclosure = rt.compaction_policy();
    assert_eq!(disclosure.mode, CompactionMode::LocalOnly);
    assert_eq!(
        disclosure.exclude,
        vec![ContentClass::ToolResults, ContentClass::Thinking]
    );
    assert!(
        matches!(rt.rollover_summarizer(), Err(None)),
        "local only: no remote summary transport"
    );
    // Another model: the parent's window override is not inherited.
    let rt = worker_on("claude-opus-4-6", &policy);
    assert_eq!(rt.context_window(), 200_000);
}

/// 8c: starting a v2 worker run binds the runtime's continuation to the
/// run before the settings are applied: the run's logical ID and a fresh
/// state that keeps nothing of an earlier binding (no note, phase, window,
/// archive or durability block of it; the window and latest archive come
/// only from the opening's marker), then the settings as
/// `apply_worker_context` gives them. The archive scope is the run's
/// memory binding, which the binding does not touch.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn starting_a_worker_run_binds_its_runtime_to_the_run_and_nothing_else() {
    let _base = crate::test_env::BaseDirGuard::new();
    let mut rt = crate::Runtime::new_headless();
    rt.set_model(SONNET.to_string());
    let earlier = {
        let mut s = rt.continuation.lock().unwrap();
        s.logical_id = "parent-session".into();
        s.note = "the parent's pending note".into();
        s.policy.report_phase(agent_core::core::context_policy::WorkPhase::WrapUp);
        s.window = 7;
        s.latest_archive = Some("parent-archive".into());
        s.durability_blocked = true;
        s.policy
    };
    assert_ne!(earlier, Default::default());
    let binding = format!("{:?}", rt.memory_backend_for_test().scope().map(|s| s.key().to_owned()));
    let context = auto_parent().resolve(SONNET).expect("resolves");
    let opening = vec![std::sync::Arc::new(serde_json::json!({"role": "user", "content": "the task"}))];
    rt.start_worker_run("subagent-run-0123", &context, &opening);
    let s = rt.continuation.lock().unwrap();
    assert_eq!(s.logical_id, "subagent-run-0123");
    assert_eq!(s.note, "");
    assert_eq!(s.policy, Default::default(), "no phase of the earlier binding");
    assert_eq!(
        (s.window, s.latest_archive.as_deref(), s.durability_blocked),
        (1, None, false)
    );
    assert_eq!(s.config, context.context_management, "then the settings");
    drop(s);
    assert_eq!(format!("{:?}", rt.memory_backend_for_test().scope().map(|s| s.key().to_owned())), binding, "the archive scope is unchanged");
}

/// Worker successor trial (pins c, e): `subagent.context_target_tokens`
/// reaches the snapshot workers inherit and nothing of the main session:
/// its own continuation settings (an explicit target included) are what
/// they are without the key, and its windows carry no worker marker. A
/// worker run's runtime gets the key's target over the inherited one and
/// the marker, which a reset of its binding keeps; unset, the marker says
/// `auto`.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn the_worker_successor_target_reaches_worker_runs_only() {
    use crate::runtime::context_telemetry::WorkerTarget;
    let _base = crate::test_env::BaseDirGuard::new();
    let config = |key| crate::config::SynapsConfig {
        context_management: ContextManagementConfig {
            mode: ContextManagementMode::Auto,
            target_tokens: Some(90_000),
            ..Default::default()
        },
        subagent: crate::config::SubagentLimits {
            context_target_tokens: key,
            ..Default::default()
        },
        ..Default::default()
    };
    let main_on = |key| {
        let mut rt = crate::Runtime::new_headless();
        rt.apply_config(&config(key));
        rt.set_model(SONNET.to_string());
        rt
    };
    let (today, main) = (main_on(None), main_on(Some(48_000)));
    let main_config = main.continuation.lock().unwrap().config;
    assert_eq!(
        main_config,
        today.continuation.lock().unwrap().config,
        "the main session's own"
    );
    assert_eq!(main_config.target_tokens, Some(90_000));
    assert_eq!(
        main.continuation.lock().unwrap().worker_target,
        None,
        "no worker marker"
    );
    assert_eq!(
        main.worker_context_policy(),
        WorkerContextPolicy {
            worker_target_tokens: Some(48_000),
            ..today.worker_context_policy()
        }
    );
    for (parent, marker, target) in [(&main, Some(48_000), 48_000), (&today, None, 90_000)] {
        let context = parent
            .worker_context_policy()
            .resolve(SONNET)
            .expect("resolves");
        let mut worker = crate::Runtime::new_headless();
        worker.set_model(SONNET.to_string());
        worker.start_worker_run("worker-run:0123", &context, &[]);
        worker.reset_context_continuation("worker-run:0123", &[]);
        let s = worker.continuation.lock().unwrap();
        assert_eq!(s.config.target_tokens, Some(target));
        assert_eq!(
            s.worker_target,
            Some(WorkerTarget(marker)),
            "kept across a reset"
        );
    }
    assert_eq!(
        main.continuation.lock().unwrap().config,
        main_config,
        "untouched by its workers"
    );
}

/// The old silent path is gone. A parent's thresholds that don't fit the
/// worker's window used to reach the stream as they were: every
/// assessment was `InvalidConfig` (a hard stop, so a rollover each round)
/// and each rollover ran unbounded with no summary. A worker gets its
/// window's defaults instead, said once, and a small turn does not roll
/// over.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn thresholds_that_dont_fit_never_reach_a_workers_stream() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (_mock, _g) = spawn_warm().await;
    let policy = WorkerContextPolicy {
        context_management: ContextManagementConfig {
            mode: ContextManagementMode::Auto,
            pressure_tokens: Some(600_000),
            rollover_tokens: Some(800_000),
            target_tokens: Some(300_000),
            summary_tokens: Some(20_000),
            ..Default::default()
        },
        window_override: Some(InheritedWindow {
            tokens: 1_000_000,
            model: "claude-opus-4-6".into(),
        }),
        ..Default::default()
    };
    let context = policy.resolve(SONNET).expect("the defaults fit");
    assert!(context.notice.is_some());
    let rt = worker_on(SONNET, &policy);
    let config = rt.continuation.lock().unwrap().config;
    assert_eq!(config.mode, ContextManagementMode::Auto);
    let thresholds = ContextThresholds::resolve(&config, rt.context_window())
        .expect("thresholds the worker's window resolves");
    assert!(thresholds.summary_tokens(&config) > 0, "a bounded summary");
    let small = vec![msg(json!({"role":"user","content":"hello"}))];
    let out = turn(&rt, small, Default::default()).await;
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert!(out.heads.is_empty(), "no rollover in a small turn");
}

fn booked_entries(ledger: &RequestLedgerHandle) -> Vec<LedgerEntry> {
    ledger.snapshot().entries().cloned().collect()
}

/// m7: through the stream, a v2 worker's rollover summary goes to the
/// summary model its inherited settings choose, and its run ledger prices
/// that summary at that model.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn m7_a_v2_workers_summary_is_booked_at_the_model_its_settings_choose() {
    const CHOSEN: &str = "claude-haiku-4-5";
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let policy = WorkerContextPolicy {
        context_management: ContextManagementConfig {
            mode: ContextManagementMode::Auto,
            target_tokens: Some(TARGET),
            summary_tokens: Some(SUMMARY),
            ..Default::default()
        },
        summary_model: Some(CHOSEN.into()),
        window_override: Some(InheritedWindow {
            tokens: 500_000,
            model: SONNET.into(),
        }),
        ..Default::default()
    };
    let mut rt = runtime(Some(SUMMARY));
    rt.set_summary_model(None);
    rt.apply_worker_context(&policy.resolve(SONNET).expect("resolves"));
    let slot = LedgerSlot::default();
    rt.arm_request_ledger(slot.clone());
    assert!(slot.install(RequestLedgerHandle::new(SONNET)));
    let ledger = rt.request_ledger().expect("the run's ledger");
    let (_first, second) = two_turns(&rt).await;
    let summaries: Vec<_> = booked_entries(&ledger)
        .into_iter()
        .filter(|e| e.purpose == RequestPurpose::RolloverSummary)
        .collect();
    assert_eq!(summaries.len(), 1, "{summaries:?}");
    assert_eq!(summaries[0].model, CHOSEN);
    let cold: Vec<_> = mock
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|seen| !seen.body.contains("\"stream\":true"))
        .map(|seen| seen.body.clone())
        .collect();
    assert_eq!(cold.len(), 1, "one cold summary");
    assert!(cold[0].contains(&format!("\"model\":\"{CHOSEN}\"")), "{}", &cold[0][..200.min(cold[0].len())]);
    assert!(second
        .heads
        .iter()
        .any(|head| summary_of(head).is_some_and(|s| s.contains("COLD-SUMMARY"))));
}
