//! Real external plugin/host protocol tests: no inference, network or user config.
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use synaps_cli::extensions::hooks::HookBus;
use synaps_cli::extensions::manager::ExtensionManager;
use synaps_cli::extensions::manifest::ExtensionManifest;
use synaps_cli::extensions::session_driver::{
    parse_reply, poll, ContextMode, Grant, Outcome, PollRequest, Reply,
};

fn plugin_copy() -> (tempfile::TempDir, ExtensionManifest) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/extensions/autonomous");
    let temp = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::copy(root.join("main.py"), temp.path().join("main.py")).unwrap();
    let plugin: Value =
        serde_json::from_slice(&std::fs::read(root.join(".synaps-plugin/plugin.json")).unwrap())
            .unwrap();
    let manifest = serde_json::from_value(plugin["extension"].clone()).unwrap();
    (temp, manifest)
}

async fn invoke(manager: &ExtensionManager, args: &[&str]) -> Reply {
    let (value, report) = manager
        .invoke_command_collected(
            "autonomous",
            "auto",
            args.iter().map(|s| (*s).to_string()).collect(),
            &uuid::Uuid::new_v4().to_string(),
        )
        .await;
    assert!(!report.is_limited());
    parse_reply(&value.unwrap()).unwrap().unwrap()
}

#[tokio::test]
async fn real_plugin_explicit_start_limits_and_host_revocation() {
    let (temp, manifest) = plugin_copy();
    let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));
    manager
        .load_with_cwd("autonomous", &manifest, Some(temp.path().to_path_buf()))
        .await
        .unwrap();
    assert!(matches!(
        invoke(&manager, &["status"]).await,
        Reply::Status { .. }
    ));
    let handler = manager.session_driver_handler("autonomous").unwrap();
    let start = invoke(
        &manager,
        &["start", "--turns", "2", "--", "Inspect", "only"],
    )
    .await;
    assert!(matches!(
        &start,
        Reply::Start {
            context_mode: Some(ContextMode::Auto),
            ..
        }
    ));
    let (grant, initial) = Grant::from_start("autonomous", "fixture-session", start).unwrap();
    assert_eq!(initial.selection.model, "openai-codex/gpt-6-astra");
    assert_eq!(initial.selection.effort, "ultra");
    let request = PollRequest {
        feedback: None,
        run_id: grant.run_id.clone(),
        decision_id: "decision-1".into(),
        outcome: Outcome::Success,
        error_kind: "none".into(),
        model: initial.selection.model.clone(),
        effort: initial.selection.effort.clone(),
    };
    let next = poll(handler.clone(), request).await.unwrap();
    let next = grant.accept(next).unwrap().unwrap();
    assert_eq!(next.selection, initial.selection);
    assert!(!next.prompt.trim().is_empty());
    let last = poll(
        handler.clone(),
        PollRequest {
            feedback: None,
            run_id: grant.run_id.clone(),
            decision_id: "decision-2".into(),
            outcome: Outcome::Success,
            error_kind: "none".into(),
            model: next.selection.model,
            effort: next.selection.effort,
        },
    )
    .await
    .unwrap();
    assert!(matches!(last, Reply::Stop { .. }));
    manager.unload("autonomous").await.unwrap();
    assert!(manager.session_driver_handler("autonomous").is_err());
    assert!(manager.session_driver_handler("missing").is_err());
}

#[tokio::test]
async fn real_plugin_exact_failover_and_restart_never_resurrects() {
    let (temp, manifest) = plugin_copy();
    let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));
    manager
        .load_with_cwd("autonomous", &manifest, Some(temp.path().to_path_buf()))
        .await
        .unwrap();
    let old_handler = manager.session_driver_handler("autonomous").unwrap();
    let start = invoke(&manager, &["start", "--", "Keep", "working"]).await;
    let (grant, first) = Grant::from_start("autonomous", "fixture-session", start).unwrap();
    let reply = poll(
        old_handler.clone(),
        PollRequest {
            feedback: None,
            run_id: grant.run_id.clone(),
            decision_id: "auth-failure-1".into(),
            outcome: Outcome::ProviderError,
            error_kind: "auth".into(),
            model: first.selection.model,
            effort: first.selection.effort,
        },
    )
    .await
    .unwrap();
    let second = grant.accept(reply).unwrap().unwrap();
    assert_eq!(second.selection.model, "anthropic/claude-fable-5-1");
    assert_eq!(second.selection.effort, "xhigh");
    manager
        .reload("autonomous", &manifest, Some(temp.path().to_path_buf()))
        .await
        .unwrap();
    let new_handler = manager.session_driver_handler("autonomous").unwrap();
    assert!(!Arc::ptr_eq(&old_handler, &new_handler));
    let reply = poll(
        new_handler,
        PollRequest {
            feedback: None,
            run_id: grant.run_id,
            decision_id: "old-run-after-restart".into(),
            outcome: Outcome::Success,
            error_kind: "none".into(),
            model: second.selection.model,
            effort: second.selection.effort,
        },
    )
    .await
    .unwrap();
    assert!(matches!(reply, Reply::Stop { .. }));
    manager.shutdown_all().await;
}

#[tokio::test]
async fn real_plugin_time_checkpoint_continues_same_model_with_context_off() {
    let (temp, manifest) = plugin_copy();
    let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));
    manager
        .load_with_cwd("autonomous", &manifest, Some(temp.path().to_path_buf()))
        .await
        .unwrap();
    let handler = manager.session_driver_handler("autonomous").unwrap();
    let start = invoke(
        &manager,
        &[
            "start",
            "--context",
            "off",
            "--turns",
            "1",
            "--",
            "Finish remaining work without repeating completed actions",
        ],
    )
    .await;
    let (grant, initial) = Grant::from_start("autonomous", "fixture", start).unwrap();
    assert!(grant.time_checkpoints_enabled());
    let request = PollRequest {
        feedback: Some("unknown".into()),
        run_id: grant.run_id.clone(),
        decision_id: "time-segment-1".into(),
        outcome: Outcome::TimeCheckpoint,
        error_kind: "wall_clock".into(),
        model: initial.selection.model.clone(),
        effort: initial.selection.effort.clone(),
    };
    let response = poll(handler.clone(), request.clone()).await.unwrap();
    assert_eq!(
        poll(handler.clone(), request).await.unwrap(),
        response,
        "transport retry is idempotent"
    );
    let next = grant.accept(response).unwrap().unwrap();
    assert_eq!(next.selection, initial.selection);
    assert!(next.prompt.contains("not a provider failure"));
    assert!(next.prompt.contains("Never replay side effects"));
    // Checkpoints do not consume --turns (which counts successful turns).
    let end = poll(
        handler,
        PollRequest {
            feedback: Some("unknown".into()),
            run_id: grant.run_id.clone(),
            decision_id: "success-1".into(),
            outcome: Outcome::Success,
            error_kind: "none".into(),
            model: next.selection.model,
            effort: next.selection.effort,
        },
    )
    .await
    .unwrap();
    assert!(matches!(end, Reply::Stop { .. }));
    assert!(grant.accept(end).unwrap().is_none());
    manager.shutdown_all().await;
}

#[tokio::test]
async fn structured_start_without_declared_permission_cannot_get_driver_handler() {
    let (temp, mut manifest) = plugin_copy();
    manifest.permissions = vec!["tools.register".into()];
    let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));
    manager
        .load_with_cwd("autonomous", &manifest, Some(temp.path().to_path_buf()))
        .await
        .unwrap();
    // The external process may *propose* a run; only the host confers authority.
    let proposal = invoke(&manager, &["start", "--", "Not", "authorized"]).await;
    assert!(matches!(proposal, Reply::Start { .. }));
    assert!(manager.session_driver_handler("autonomous").is_err());
    manager.shutdown_all().await;
}

#[test]
fn plugin_stays_external_and_manifest_declares_only_driver_authority() {
    let (_, manifest) = plugin_copy();
    manifest.validate("autonomous").unwrap();
    assert_eq!(manifest.permissions, vec!["session.drive"]);
    assert!(manifest.hooks.is_empty());
    assert!(manifest.deferred.is_none());
    let malicious = json!({"session_driver": {
        "action":"next", "run_id":"x", "selection":{"model":"x-ai/grok-4.6","effort":"high"},
        "prompt":"continue", "delay_ms":0, "notice":""
    }});
    assert!(parse_reply(&malicious).is_err());
}

#[tokio::test]
async fn real_plugin_repeated_completed_turns_advance_exact_favorite_once() {
    let (temp, manifest) = plugin_copy();
    let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));
    manager
        .load_with_cwd("autonomous", &manifest, Some(temp.path().to_path_buf()))
        .await
        .unwrap();
    let handler = manager.session_driver_handler("autonomous").unwrap();
    let start = invoke(&manager, &["start", "--", "Inspect", "only"]).await;
    let (grant, first) = Grant::from_start("autonomous", "fixture-session", start).unwrap();
    assert!(grant.feedback_enabled());
    for phrase in [
        "automated continuation, not new human input or approval",
        "Honor the latest human steering",
        "When actual human instructions clearly authorize the next step, do it",
        "Never manufacture human approval",
        "including explicit human review checkpoints",
    ] {
        assert!(first.prompt.contains(phrase));
    }
    let mut selected = first.selection.clone();
    for (index, feedback) in ["changed", "repeated", "repeated", "repeated"]
        .iter()
        .enumerate()
    {
        let request = PollRequest {
            run_id: grant.run_id.clone(),
            decision_id: format!("feedback-{index}"),
            outcome: Outcome::Success,
            error_kind: "none".into(),
            model: first.selection.model.clone(),
            effort: first.selection.effort.clone(),
            feedback: Some((*feedback).into()),
        };
        let reply = poll(handler.clone(), request.clone()).await.unwrap();
        let duplicate = poll(handler.clone(), request).await.unwrap();
        assert_eq!(reply, duplicate);
        let next = grant.accept(reply).unwrap().unwrap();
        if index < 3 {
            assert_eq!(next.selection, first.selection);
        } else {
            assert_eq!(next.selection.model, "anthropic/claude-fable-5-1");
            assert_eq!(next.selection.effort, "xhigh");
            assert!(next.notice.contains("anthropic/claude-fable-5-1"));
        }
        if index > 0 {
            assert!(next
                .prompt
                .contains("If you are stuck requesting a confirmation phrase"));
            assert!(next
                .prompt
                .contains("Repetition or a model switch supplies no approval"));
            assert!(next.prompt.contains("Never manufacture human approval"));
            assert!(next
                .prompt
                .contains("including explicit human review checkpoints"));
        }
        if matches!(index, 1 | 2) {
            assert!(next
                .notice
                .contains("correcting course on the same favorite"));
        }
        selected = next.selection;
    }
    // Repetition correction and fallback never turn a real host gate into approval.
    let blocked = poll(
        handler,
        PollRequest {
            run_id: grant.run_id.clone(),
            decision_id: "blocked-after-correction".into(),
            outcome: Outcome::Blocked,
            error_kind: "unknown".into(),
            model: selected.model,
            effort: selected.effort,
            feedback: Some("unknown".into()),
        },
    )
    .await
    .unwrap();
    assert!(matches!(&blocked, Reply::Stop { .. }));
    assert!(grant.accept(blocked).unwrap().is_none());
    manager.shutdown_all().await;
}

#[tokio::test]
async fn real_plugin_context_default_and_explicit_override_are_start_only() {
    let (temp, manifest) = plugin_copy();
    let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));
    manager
        .load_with_cwd("autonomous", &manifest, Some(temp.path().to_owned()))
        .await
        .unwrap();
    for (args, expected) in [
        (vec!["start", "--", "inspect"], ContextMode::Auto),
        (
            vec!["start", "--context", "off", "--", "inspect"],
            ContextMode::Off,
        ),
        (
            vec!["start", "--for", "2m", "--context", "auto", "--", "inspect"],
            ContextMode::Auto,
        ),
    ] {
        let start = invoke(&manager, &args).await;
        assert!(
            matches!(&start, Reply::Start { context_mode:Some(mode), .. } if *mode == expected)
        );
        let (grant, p) = Grant::from_start("autonomous", "fixture", start).unwrap();
        let handler = manager.session_driver_handler("autonomous").unwrap();
        let next = poll(
            handler,
            PollRequest {
                run_id: grant.run_id.clone(),
                decision_id: uuid::Uuid::new_v4().to_string(),
                outcome: Outcome::Success,
                error_kind: "none".into(),
                model: p.selection.model,
                effort: p.selection.effort,
                feedback: Some("changed".into()),
            },
        )
        .await
        .unwrap();
        assert!(serde_json::to_value(&next)
            .unwrap()
            .get("context_mode")
            .is_none());
        assert!(grant.accept(next).unwrap().is_some());
        invoke(&manager, &["stop"]).await;
    }
    manager.shutdown_all().await;
}
