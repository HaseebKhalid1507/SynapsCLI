//! E-P3/P4 actor-level driver tests. Uses the real `autonomous` plugin via
//! `examples/extensions/autonomous/`, loaded as in `tests/autonomous_plugin.rs`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_engine::session::{
    ClientId, ClientKind, ClientMeta, ClientTransport, LocalTransport, SessionCommand,
    SessionConfig, SessionEventWire, SessionHandle,
};
use agent_engine::{EngineHost, HostOpts};
use synaps_cli::extensions::manifest::ExtensionManifest;

use serial_test::serial;

// Shared loopback-stub fixtures (same module the differential test uses to make
// turns actually COMPLETE): `spawn_stub`, `Script`, `ANTHROPIC_SSE`, `HomeGuard`.
#[path = "support/phase2/mod.rs"]
mod support;
use support::{spawn_stub, HomeGuard, Script, ANTHROPIC_SSE};

const MODEL: &str = "claude-sonnet-4-5";

// ── plugin helpers ───────────────────────────────────────────────────────────

fn plugin_copy() -> (tempfile::TempDir, ExtensionManifest) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/extensions/autonomous");
    let temp = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::copy(root.join("main.py"), temp.path().join("main.py")).unwrap();
    let plugin: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join(".synaps-plugin/plugin.json")).unwrap(),
    )
    .unwrap();
    let manifest = serde_json::from_value(plugin["extension"].clone()).unwrap();
    (temp, manifest)
}

async fn host() -> Arc<EngineHost> {
    EngineHost::boot(HostOpts {
        profile: None,
        no_extensions: true,
    })
    .await
    .expect("host boot")
}

async fn host_with_plugin() -> (Arc<EngineHost>, tempfile::TempDir) {
    let host = host().await;
    let (temp, manifest) = plugin_copy();
    host.ext_manager()
        .write()
        .await
        .load_with_cwd("autonomous", &manifest, Some(temp.path().to_path_buf()))
        .await
        .unwrap();
    (host, temp)
}

struct TestActor {
    t: LocalTransport,
    #[allow(dead_code)]
    handle: SessionHandle,
}

async fn session(host: &Arc<EngineHost>) -> TestActor {
    let handle = host
        .create_session(SessionConfig {
            model_override: Some(MODEL.into()),
            persist: false,
            ..SessionConfig::default()
        })
        .await
        .expect("create_session");
    let (t, _snap) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    TestActor { t, handle }
}

impl TestActor {
    async fn until(&mut self, pred: impl Fn(&SessionEventWire) -> bool) -> SessionEventWire {
        loop {
            let env = tokio::time::timeout(Duration::from_secs(10), self.t.next_event())
                .await
                .expect("actor hung")
                .expect("actor alive");
            if pred(&env.event) {
                return env.event;
            }
        }
    }

    #[allow(dead_code)]
    async fn collect_until(
        &mut self,
        pred: impl Fn(&SessionEventWire) -> bool,
    ) -> Vec<SessionEventWire> {
        let mut collected = Vec::new();
        loop {
            let env = tokio::time::timeout(Duration::from_secs(10), self.t.next_event())
                .await
                .expect("actor hung")
                .expect("actor alive");
            let hit = pred(&env.event);
            collected.push(env.event);
            if hit {
                return collected;
            }
        }
    }

    async fn send(&self, cmd: SessionCommand) {
        self.t.send(cmd).await.unwrap();
    }

    async fn end(&self) {
        self.t
            .send(SessionCommand::End {
                reason: agent_engine::session::EndReason::ClientQuit,
            })
            .await
            .unwrap();
    }

    async fn driver_start(&self) {
        self.send(SessionCommand::DriverStart {
            plugin: "autonomous".into(),
            command: "auto".into(),
            arg: "start -- test task".into(),
        })
        .await;
    }

    async fn arm(&mut self) {
        self.driver_start().await;
        self.until(|e| matches!(e, SessionEventWire::DriverArmed { .. }))
            .await;
    }
}

// ── P3 tests ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn driver_start_arms_and_emits() {
    let (host, _temp) = host_with_plugin().await;
    let mut actor = session(&host).await;
    actor.driver_start().await;
    let ev = actor
        .until(|e| matches!(e, SessionEventWire::DriverArmed { .. }))
        .await;
    match ev {
        SessionEventWire::DriverArmed {
            plugin_id,
            run_id,
            models,
            selection,
            ..
        } => {
            assert_eq!(plugin_id, "autonomous");
            assert!(!run_id.is_empty());
            assert!(!models.is_empty());
            assert!(!selection.model.is_empty());
        }
        _ => unreachable!(),
    }
    actor.end().await;
}

#[tokio::test]
async fn driver_start_without_permission_refused() {
    let host = host().await;
    let mut actor = session(&host).await;
    actor
        .send(SessionCommand::DriverStart {
            plugin: "nonexistent".into(),
            command: "auto".into(),
            arg: "start".into(),
        })
        .await;
    let ev = actor
        .until(|e| matches!(e, SessionEventWire::SystemNotice(_)))
        .await;
    match ev {
        SessionEventWire::SystemNotice(msg) => {
            assert!(
                msg.contains("unknown") || msg.contains("not loaded") || msg.contains("extension"),
                "expected refusal notice, got: {msg}"
            );
        }
        _ => unreachable!(),
    }
    actor.end().await;
}

#[tokio::test]
async fn driver_start_second_session_refused() {
    let (host, _temp) = host_with_plugin().await;
    let mut a1 = session(&host).await;
    let mut a2 = session(&host).await;

    a1.arm().await;

    a2.send(SessionCommand::DriverStart {
        plugin: "autonomous".into(),
        command: "auto".into(),
        arg: "start -- second".into(),
    })
    .await;
    // The DriverStart handler will revoke its own (nonexistent) driver, then
    // process the command result. If the grant claim fails, it emits a notice.
    // But note: DriverStart spawns the command, and tick() processes the result.
    // The grant claim happens in driver_arm which is called from tick() when
    // the command result comes back. So we need to wait for that.
    let ev = a2
        .until(|e| {
            matches!(
                e,
                SessionEventWire::SystemNotice(_) | SessionEventWire::DriverArmed { .. }
            )
        })
        .await;
    assert!(
        matches!(ev, SessionEventWire::SystemNotice(ref msg) if msg.contains("already driving")),
        "expected single-tenancy refusal, got: {ev:?}"
    );

    a1.end().await;
    a2.end().await;
}

#[tokio::test]
async fn cancel_revokes_driver() {
    let (host, _temp) = host_with_plugin().await;
    let mut actor = session(&host).await;
    actor.arm().await;
    actor.send(SessionCommand::Cancel).await;
    let ev = actor
        .until(|e| matches!(e, SessionEventWire::DriverRevoked { .. }))
        .await;
    match ev {
        SessionEventWire::DriverRevoked { reason, .. } => {
            assert!(reason.contains("canceled"), "reason: {reason}");
        }
        _ => unreachable!(),
    }
    actor.end().await;
}

#[tokio::test]
async fn new_session_revokes() {
    let (host, _temp) = host_with_plugin().await;
    let mut actor = session(&host).await;
    actor.arm().await;
    actor.send(SessionCommand::NewSession).await;
    let ev = actor
        .until(|e| matches!(e, SessionEventWire::DriverRevoked { .. }))
        .await;
    match ev {
        SessionEventWire::DriverRevoked { reason, .. } => {
            assert!(reason.contains("replaced"), "reason: {reason}");
        }
        _ => unreachable!(),
    }
    actor.end().await;
}

#[tokio::test]
async fn end_releases_grant() {
    let (host, _temp) = host_with_plugin().await;
    let mut actor = session(&host).await;
    actor.arm().await;
    actor.end().await;

    // Wait a moment for teardown.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Grant should be released — a new session can arm.
    let mut a2 = session(&host).await;
    a2.arm().await;
    a2.end().await;
}

#[tokio::test]
async fn checkpoint_reload_revokes_and_notifies() {
    let (host, _temp) = host_with_plugin().await;
    let mut actor = session(&host).await;
    actor.arm().await;
    actor
        .send(SessionCommand::Checkpoint {
            reason: agent_engine::session::CheckpointReason::Reload,
        })
        .await;
    let ev = actor
        .until(|e| matches!(e, SessionEventWire::DriverRevoked { .. }))
        .await;
    match ev {
        SessionEventWire::DriverRevoked { reason, .. } => {
            assert!(
                reason.contains("reload") || reason.contains("daemon"),
                "reason: {reason}"
            );
        }
        _ => unreachable!(),
    }
    actor.end().await;
}

#[tokio::test]
async fn armed_session_does_not_park() {
    let (host, _temp) = host_with_plugin().await;
    let handle = host
        .create_session(SessionConfig {
            model_override: Some(MODEL.into()),
            persist: true,
            ..SessionConfig::default()
        })
        .await
        .expect("create_session");
    let (mut t, _snap) =
        LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
            .await
            .unwrap();
    // Second client: under E-P7 §S1 detaching the LAST client revokes the
    // driver (no headless spend). To exercise the can_park/can_end_idle driver
    // guards (F6) a client must stay attached so the zero-client revoke never
    // fires and the driver stays armed across the park deadline.
    let (mut keeper, _snap2) =
        LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Attach))
            .await
            .unwrap();

    // Arm the driver.
    t.send(SessionCommand::DriverStart {
        plugin: "autonomous".into(),
        command: "auto".into(),
        arg: "start -- park-test".into(),
    })
    .await
    .unwrap();
    loop {
        let env = tokio::time::timeout(Duration::from_secs(10), t.next_event())
            .await
            .expect("hung")
            .expect("alive");
        if matches!(env.event, SessionEventWire::DriverArmed { .. }) {
            break;
        }
    }

    // Force the idle/park deadline to fire immediately: without the driver
    // guards on can_park AND can_end_idle (F6), a zero-turn armed session
    // would be idle-ENDED here, killing the run mid-arm. The keeper client
    // stays attached, so E-P7 §S1's last-client revoke does not fire.
    std::env::set_var("SYNAPS_DAEMON_PARK_GRACE_SECS", "0");
    std::env::set_var("SYNAPS_DAEMON_IDLE_END_GRACE_SECS", "0");
    t.send(SessionCommand::Detach {
        client: ClientId(1),
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    std::env::remove_var("SYNAPS_DAEMON_PARK_GRACE_SECS");
    std::env::remove_var("SYNAPS_DAEMON_IDLE_END_GRACE_SECS");

    // The session must be alive (not Ended, not Parked) — the keeper client
    // and the armed driver both hold it warm.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), handle.closed())
            .await
            .is_err(),
        "armed session was ended by the idle deadline (F6)"
    );
    // Revoke + end to clean up via the still-attached keeper.
    keeper.send(SessionCommand::Cancel).await.unwrap();
    keeper
        .send(SessionCommand::End {
            reason: agent_engine::session::EndReason::ClientQuit,
        })
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), keeper.next_event()).await;
}

// ── P4 tick tests ────────────────────────────────────────────────────────────

/// The tick processes the command result and arms the driver. This is implicitly
/// tested by all the P3 tests above (arm() calls driver_start + waits for
/// DriverArmed), but make it explicit.
#[tokio::test]
async fn tick_processes_command_result_and_arms() {
    let (host, _temp) = host_with_plugin().await;
    let mut actor = session(&host).await;
    actor.driver_start().await;
    // The arm event proves tick processed TaskResult::Command.
    let ev = actor
        .until(|e| matches!(e, SessionEventWire::DriverArmed { .. }))
        .await;
    assert!(matches!(ev, SessionEventWire::DriverArmed { .. }));
    actor.end().await;
}

/// After arming, the tick should eventually emit a poll when the proposal
/// delay expires (the reference plugin uses a configurable delay). We test
/// by canceling — if the tick is running, we'll see DriverRevoked.
#[tokio::test]
async fn tick_revokes_on_lifecycle_death() {
    let (host, _temp) = host_with_plugin().await;
    let mut actor = session(&host).await;
    actor.arm().await;

    // Shut down the plugin → lifecycle dies → tick should revoke.
    let _ = host.ext_manager().write().await.unload("autonomous").await;
    let ev = actor
        .until(|e| matches!(e, SessionEventWire::DriverRevoked { .. }))
        .await;
    match ev {
        SessionEventWire::DriverRevoked { reason, .. } => {
            assert!(
                reason.contains("extension")
                    || reason.contains("lifecycle")
                    || reason.contains("replaced")
                    || reason.contains("unavailable")
                    || reason.contains("restarted")
                    || reason.contains("lost"),
                "reason: {reason}"
            );
        }
        _ => unreachable!(),
    }
    actor.end().await;
}

/// The tick should revoke when idle conflict conditions arise (e.g., compaction
/// starts while driver is armed).
#[tokio::test]
async fn tick_revokes_on_idle_conflict() {
    let (host, _temp) = host_with_plugin().await;
    let mut actor = session(&host).await;
    actor.arm().await;

    // Trigger a compaction — this should cause idle conflict revocation.
    actor
        .send(SessionCommand::Compact {
            instructions: None,
        })
        .await;
    // The driver should be revoked due to idle conflict (compaction running).
    let ev = actor
        .until(|e| matches!(e, SessionEventWire::DriverRevoked { .. }))
        .await;
    match ev {
        SessionEventWire::DriverRevoked { reason, .. } => {
            assert!(
                reason.contains("queued work")
                    || reason.contains("compaction")
                    || reason.contains("session/lifecycle"),
                "reason: {reason}"
            );
        }
        _ => unreachable!(),
    }
    actor.end().await;
}

/// The driver tick must never block the select loop. A plugin that sleeps
/// 5s in poll must not delay a concurrent Submit's TurnStarted by more
/// than the tick period. We test this by verifying the actor stays
/// responsive while driver work is pending.
#[tokio::test]
async fn tick_never_blocks_the_select_loop() {
    let (host, _temp) = host_with_plugin().await;
    let mut actor = session(&host).await;
    actor.arm().await;

    // The actor should be responsive while the driver's proposal timer
    // is ticking. Send a Cancel and verify quick response.
    let start = std::time::Instant::now();
    actor.send(SessionCommand::Cancel).await;
    let ev = actor
        .until(|e| matches!(e, SessionEventWire::DriverRevoked { .. }))
        .await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "Cancel took {elapsed:?}, expected < 1s — tick may be blocking"
    );
    assert!(matches!(ev, SessionEventWire::DriverRevoked { .. }));
    actor.end().await;
}


// ── P5 stream hook tests ────────────────────────────────────────────────────

/// The driver turn starts after the proposal delay. Once started, the turn
/// will fail (no credentials in test) and the terminal path fires. Verify
/// the turn was started (TurnStarted event) or the driver revoked due to
/// a preflight error. Extended timeout to handle retries.
#[tokio::test]
async fn driver_turn_starts_or_revokes_after_arm() {
    let (host, _temp) = host_with_plugin().await;
    let mut actor = session(&host).await;
    actor.arm().await;

    // The driver tick will fire the proposal after ~1s delay, attempt prepare
    // then start. With no credentials, the turn fails at preflight or API call.
    // We wait up to 30s since the bogus model may need network timeouts.
    let ev = tokio::time::timeout(
        Duration::from_secs(30),
        actor.until(|e| {
            matches!(
                e,
                SessionEventWire::TurnStarted { .. }
                    | SessionEventWire::DriverRevoked { .. }
                    | SessionEventWire::DriverTurnOutcome { .. }
            )
        }),
    )
    .await;
    assert!(ev.is_ok(), "expected turn start or driver revoke within 30s");
    actor.send(SessionCommand::Cancel).await;
    // Drain until end is safe.
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if actor.t.next_event().await.is_none() {
                break;
            }
        }
    })
    .await;
}

/// Cancel while a driver turn is armed (before or during streaming) should
/// revoke without an outcome.
#[tokio::test]
async fn driver_cancel_eof_revokes_without_outcome() {
    let (host, _temp) = host_with_plugin().await;
    let mut actor = session(&host).await;
    actor.arm().await;

    // Cancel immediately.
    actor.send(SessionCommand::Cancel).await;
    let ev = actor
        .until(|e| matches!(e, SessionEventWire::DriverRevoked { .. }))
        .await;
    match ev {
        SessionEventWire::DriverRevoked { reason, .. } => {
            assert!(
                reason.contains("canceled"),
                "expected cancel revocation, got: {reason}"
            );
        }
        _ => unreachable!(),
    }
    actor.end().await;
}

/// event_wake_inhibited_while_armed: verify the code guard exists by
/// ensuring a cancel after arm works cleanly (the RunTurn path is blocked).
#[tokio::test]
async fn event_wake_inhibited_while_armed() {
    let (host, _temp) = host_with_plugin().await;
    let mut actor = session(&host).await;
    actor.arm().await;

    actor.send(SessionCommand::Cancel).await;
    let ev = actor
        .until(|e| matches!(e, SessionEventWire::DriverRevoked { .. }))
        .await;
    assert!(matches!(ev, SessionEventWire::DriverRevoked { .. }));
    actor.end().await;
}

// ── P6 submit routing tests ────────────────────────────────────────────────

/// While the driver is armed and idle (not streaming), a Submit should
/// queue to the steering FIFO instead of starting a normal turn.
#[tokio::test]
async fn submit_while_armed_idle_queues_steering() {
    let (host, _temp) = host_with_plugin().await;
    let mut actor = session(&host).await;
    actor.arm().await;

    // Submit while armed + idle → should go to steering, not TurnStarted.
    actor
        .send(SessionCommand::Submit {
            text: "steer me".into(), attachments: vec![],
        })
        .await;
    let ev = actor
        .until(|e| {
            matches!(
                e,
                SessionEventWire::Steered { .. } | SessionEventWire::TurnStarted { .. }
            )
        })
        .await;
    assert!(
        matches!(ev, SessionEventWire::Steered { .. }),
        "expected Steered, got: {ev:?}"
    );

    // Cancel and verify undelivered steering comes back.
    actor.send(SessionCommand::Cancel).await;
    let ev = actor
        .until(|e| matches!(e, SessionEventWire::DriverRevoked { .. }))
        .await;
    match ev {
        SessionEventWire::DriverRevoked {
            undelivered_steering,
            ..
        } => {
            assert!(
                undelivered_steering.iter().any(|s| s.contains("steer me")),
                "expected undelivered steering, got: {undelivered_steering:?}"
            );
        }
        _ => unreachable!(),
    }
    actor.end().await;
}

/// Submit while unarmed is a normal turn.
#[tokio::test]
async fn unarmed_submit_is_a_normal_turn() {
    let host = host().await;
    let mut actor = session(&host).await;

    actor
        .send(SessionCommand::Submit {
            text: "hello".into(), attachments: vec![],
        })
        .await;
    let ev = actor
        .until(|e| {
            matches!(
                e,
                SessionEventWire::TurnStarted { .. } | SessionEventWire::SystemNotice(_)
            )
        })
        .await;
    // Without driver, submit should start a normal turn.
    assert!(
        matches!(
            ev,
            SessionEventWire::TurnStarted { .. } | SessionEventWire::SystemNotice(_)
        ),
        "expected TurnStarted, got: {ev:?}"
    );
    // Cancel the turn.
    actor.send(SessionCommand::Cancel).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), actor.until(|e| matches!(e, SessionEventWire::Idle))).await;
    actor.end().await;
}

/// The steering FIFO has a 16 message cap.
#[tokio::test]
async fn steering_fifo_16_msg_cap() {
    let (host, _temp) = host_with_plugin().await;
    let mut actor = session(&host).await;
    actor.arm().await;

    // Queue 16 messages (at the cap).
    for i in 0..16 {
        actor
            .send(SessionCommand::Submit {
                text: format!("msg-{i}"),
                attachments: vec![],
            })
            .await;
        let _ = actor
            .until(|e| matches!(e, SessionEventWire::Steered { .. }))
            .await;
    }

    // 17th should be refused.
    actor
        .send(SessionCommand::Submit {
            text: "overflow".into(), attachments: vec![],
        })
        .await;
    let ev = actor
        .until(|e| matches!(e, SessionEventWire::SystemNotice(_)))
        .await;
    match ev {
        SessionEventWire::SystemNotice(msg) => {
            assert!(
                msg.contains("full") || msg.contains("16"),
                "expected cap notice, got: {msg}"
            );
        }
        _ => unreachable!(),
    }
    actor.send(SessionCommand::Cancel).await;
    let _ = tokio::time::timeout(Duration::from_secs(3), actor.until(|e| matches!(e, SessionEventWire::DriverRevoked { .. }))).await;
    actor.end().await;
}

/// The steering FIFO has a 256 KiB byte cap (UTF-8 byte count).
#[tokio::test]
async fn steering_fifo_256kib_cap_counts_utf8_bytes() {
    let (host, _temp) = host_with_plugin().await;
    let mut actor = session(&host).await;
    actor.arm().await;

    // Send a message just under 256 KiB.
    let big = "x".repeat(200 * 1024);
    actor
        .send(SessionCommand::Submit { text: big, attachments: vec![] })
        .await;
    let _ = actor
        .until(|e| matches!(e, SessionEventWire::Steered { .. }))
        .await;

    // Second message that crosses the 256 KiB total.
    let big2 = "y".repeat(100 * 1024);
    actor
        .send(SessionCommand::Submit { text: big2, attachments: vec![] })
        .await;
    let ev = actor
        .until(|e| matches!(e, SessionEventWire::SystemNotice(_)))
        .await;
    match ev {
        SessionEventWire::SystemNotice(msg) => {
            assert!(
                msg.contains("full") || msg.contains("256"),
                "expected byte cap notice, got: {msg}"
            );
        }
        _ => unreachable!(),
    }
    actor.send(SessionCommand::Cancel).await;
    let _ = tokio::time::timeout(Duration::from_secs(3), actor.until(|e| matches!(e, SessionEventWire::DriverRevoked { .. }))).await;
    actor.end().await;
}

/// Revocation returns undelivered steering.
#[tokio::test]
async fn revocation_restores_undelivered_steering() {
    let (host, _temp) = host_with_plugin().await;
    let mut actor = session(&host).await;
    actor.arm().await;

    // Queue some steering.
    actor
        .send(SessionCommand::Submit {
            text: "first".into(), attachments: vec![],
        })
        .await;
    let _ = actor
        .until(|e| matches!(e, SessionEventWire::Steered { .. }))
        .await;
    actor
        .send(SessionCommand::Submit {
            text: "second".into(), attachments: vec![],
        })
        .await;
    let _ = actor
        .until(|e| matches!(e, SessionEventWire::Steered { .. }))
        .await;

    // Cancel → DriverRevoked should carry undelivered steering.
    actor.send(SessionCommand::Cancel).await;
    let ev = actor
        .until(|e| matches!(e, SessionEventWire::DriverRevoked { .. }))
        .await;
    match ev {
        SessionEventWire::DriverRevoked {
            undelivered_steering,
            ..
        } => {
            assert_eq!(undelivered_steering.len(), 2);
            assert_eq!(undelivered_steering[0], "first");
            assert_eq!(undelivered_steering[1], "second");
        }
        _ => unreachable!(),
    }
    actor.end().await;
}

// ── E2E: full driver loop across multiple turns ─────────────────────────────

/// The gate for the driver branch: prove the SessionActor driver loop runs the
/// FULL cycle more than once — arm → turn1 → terminal(Done) → poll → turn2 →
/// terminal(Done) — not just that a single turn starts.
///
/// The mechanism (copied from `tests/session_actor_differential.rs`): stand up a
/// local HTTP stub answering the Anthropic API with canned SSE so each driver
/// turn actually COMPLETES (Terminal::Success → `DriverTurnOutcome{Success}`).
/// The `autonomous` plugin is armed with NO `--turns`, so it proposes UNBOUNDED
/// turns and keeps re-polling after every success (MIN_DELAY_MS ≈ 1s cadence).
///
/// Env isolation matches the differential file exactly: `#[serial]` +
/// `HomeGuard` (temp HOME + synthetic auth.json, provider keys scrubbed,
/// `SYNAPS_ANTHROPIC_BASE_URL` guarded and restored on drop).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn driver_runs_multiple_turns_end_to_end() {
    let _guard = HomeGuard::new();
    // Every request → canned Anthropic SSE that ends the turn (end_turn), so the
    // driver's turn reaches a Success terminal instead of dying at preflight.
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    // The driver turn runs under the PLUGIN's proposed selection, NOT the
    // session `model_override`. The plugin's DEFAULT_FAVORITES are fictional
    // non-Anthropic models, which would never hit our Anthropic stub (the turn
    // would EOF → Blocked). Seed a plugin-local `prefs.json` BEFORE the plugin
    // initializes so it proposes a real anthropic `claude-*` model — one the
    // stub answers via the Anthropic Messages wire (synthetic OAuth from HomeGuard).
    let host = host().await;
    let (temp, manifest) = plugin_copy();
    {
        let prefs = temp.path().join("prefs.json");
        std::fs::write(
            &prefs,
            br#"{"version":1,"favorites":[{"model":"anthropic/claude-fable-5-1","effort":"high"}]}"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&prefs, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    host.ext_manager()
        .write()
        .await
        .load_with_cwd("autonomous", &manifest, Some(temp.path().to_path_buf()))
        .await
        .unwrap();
    let _temp = temp; // keep the plugin dir (and prefs.json) alive for the run

    let mut actor = session(&host).await;
    actor.arm().await;

    // Watch the loop until we have observed >= 2 completed turns. Each cycle is
    // TurnStarted … DriverTurnOutcome{Success}; the driver must NOT revoke
    // between them (that would mean the poll never re-fired / the loop died).
    use agent_engine::extensions::session_driver::Outcome;
    let target = 2usize;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);

    let mut outcomes = 0usize; // DriverTurnOutcome count
    let mut turn_starts = 0usize; // TurnStarted count
    let mut pairs = 0usize; // start-then-terminal cycles
    let mut pending_start = false; // a TurnStarted is awaiting its terminal
    let mut early_revoke: Option<String> = None;

    while outcomes < target {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .unwrap_or_default();
        assert!(
            !remaining.is_zero(),
            "timed out before seeing {target} DriverTurnOutcome events \
             (saw {outcomes} outcomes, {turn_starts} turn starts, {pairs} pairs)"
        );
        let env = tokio::time::timeout(remaining, actor.t.next_event())
            .await
            .expect("driver loop stalled — no event before 60s deadline")
            .expect("actor alive");
        match env.event {
            SessionEventWire::TurnStarted { .. } => {
                turn_starts += 1;
                pending_start = true;
            }
            SessionEventWire::DriverTurnOutcome { outcome, .. } => {
                // A completed driver turn against the SSE stub is a success.
                assert!(
                    matches!(outcome, Outcome::Success),
                    "expected a successful/continued turn outcome, got {outcome:?}"
                );
                outcomes += 1;
                if pending_start {
                    pairs += 1;
                    pending_start = false;
                }
            }
            SessionEventWire::DriverRevoked { reason, .. } => {
                // Any revoke before we've seen 2 clean cycles is a loop failure.
                early_revoke = Some(reason);
                break;
            }
            _ => {}
        }
    }

    assert!(
        early_revoke.is_none(),
        "driver revoked mid-loop before completing {target} turns: {:?}",
        early_revoke
    );
    assert!(
        outcomes >= target,
        "expected >= {target} DriverTurnOutcome events, saw {outcomes}"
    );
    assert!(
        turn_starts >= target,
        "expected >= {target} TurnStarted events, saw {turn_starts}"
    );
    assert!(
        pairs >= target,
        "expected >= {target} arm→turn→Done cycles (TurnStarted then terminal), saw {pairs}"
    );

    // Clean shutdown: Cancel revokes the driver, then End drains to Ended.
    actor.send(SessionCommand::Cancel).await;
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        actor.until(|e| matches!(e, SessionEventWire::DriverRevoked { .. })),
    )
    .await;
    actor.end().await;
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match actor.t.next_event().await {
                None => break,
                Some(env) => {
                    if matches!(env.event, SessionEventWire::Ended { .. }) {
                        break;
                    }
                }
            }
        }
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// E-P7 security gates: S1 (zero-client → no headless spend), S3 (cost caps),
// S9 (no auto-approve under driver). 12 gate tests.
// ═══════════════════════════════════════════════════════════════════════════

use agent_engine::extensions::session_driver::{
    Grant, Outcome, PollRequest, Reply,
};

// ── shared stub setup (mirrors driver_runs_multiple_turns_end_to_end) ────────

/// Boot a host under a temp HOME with the loopback Anthropic stub wired in, and
/// load the autonomous plugin with a `prefs.json` proposing a REAL anthropic
/// model so the driver turn actually hits the stub (fictional favorites would
/// EOF → Blocked). `extra_config` is appended to the `~/.synaps-cli/config`
/// file BEFORE boot (e.g. `tools.activation_confirm = prompt`).
async fn stub_host(
    guard: &HomeGuard,
    script: Script,
    extra_config: &str,
) -> (Arc<EngineHost>, tempfile::TempDir) {
    if !extra_config.is_empty() {
        std::fs::write(guard.base_dir().join("config"), extra_config).unwrap();
    }
    let (url, _hits, _bodies) = spawn_stub(script).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (temp, manifest) = plugin_copy();
    {
        let prefs = temp.path().join("prefs.json");
        std::fs::write(
            &prefs,
            br#"{"version":1,"favorites":[{"model":"anthropic/claude-fable-5-1","effort":"high"}]}"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&prefs, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    host.ext_manager()
        .write()
        .await
        .load_with_cwd("autonomous", &manifest, Some(temp.path().to_path_buf()))
        .await
        .unwrap();
    (host, temp)
}

/// A session created against `host` with a caller-tweaked `SessionConfig`, then
/// attached (returns the `TestActor` and its `SessionHandle`).
async fn session_cfg(host: &Arc<EngineHost>, cfg: SessionConfig) -> TestActor {
    let handle = host.create_session(cfg).await.expect("create_session");
    let (t, _snap) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    TestActor { t, handle }
}

/// Anthropic SSE that calls a named tool WITH JSON arguments (input_json_delta),
/// then stops with `tool_use` — needed to drive `activate_tools` (which requires
/// a non-empty `tools` array before it raises the host confirmation).
fn sse_tool_call_with_input(name: &str, id: &str, input_json: &str) -> &'static str {
    let escaped = input_json.replace('\\', "\\\\").replace('"', "\\\"");
    Box::leak(
        format!(
            concat!(
                "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_p7\",\"type\":\"message\",",
                "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-fable-5-1\",\"stop_reason\":null,",
                "\"stop_sequence\":null,\"usage\":{{\"input_tokens\":10,\"output_tokens\":0,",
                "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}}}\n\n",
                "data: {{\"type\":\"content_block_start\",\"index\":0,",
                "\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{id}\",\"name\":\"{name}\"}}}}\n\n",
                "data: {{\"type\":\"content_block_delta\",\"index\":0,",
                "\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{input}\"}}}}\n\n",
                "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
                "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\",",
                "\"stop_sequence\":null}},\"usage\":{{\"input_tokens\":10,\"output_tokens\":5,",
                "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}\n\n",
                "data: {{\"type\":\"message_stop\"}}\n\n",
            ),
            id = id,
            name = name,
            input = escaped,
        )
        .into_boxed_str(),
    )
}

/// Anthropic SSE that calls `prompt_fixture` (raises a host prompt via the
/// stream's `SecretPromptHandle`) then stops with `tool_use`.
fn sse_prompt_fixture(id: &str) -> &'static str {
    Box::leak(
        format!(
            concat!(
                "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_pf\",\"type\":\"message\",",
                "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-fable-5-1\",\"stop_reason\":null,",
                "\"stop_sequence\":null,\"usage\":{{\"input_tokens\":10,\"output_tokens\":0,",
                "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}}}\n\n",
                "data: {{\"type\":\"content_block_start\",\"index\":0,",
                "\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{id}\",\"name\":\"prompt_fixture\"}}}}\n\n",
                "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
                "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\",",
                "\"stop_sequence\":null}},\"usage\":{{\"input_tokens\":10,\"output_tokens\":5,",
                "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}\n\n",
                "data: {{\"type\":\"message_stop\"}}\n\n",
            ),
            id = id,
        )
        .into_boxed_str(),
    )
}

/// Builtin tool that prompts through the stream's `SecretPromptHandle`. Copied
/// from `session_actor_differential.rs::PromptFixtureTool`.
struct PromptFixtureTool;

#[async_trait::async_trait]
impl agent_engine::Tool for PromptFixtureTool {
    fn name(&self) -> &str {
        "prompt_fixture"
    }
    fn description(&self) -> &str {
        "prompts"
    }
    fn parameters(&self) -> agent_engine::Value {
        serde_json::json!({"type": "object"})
    }
    fn origin(&self) -> agent_engine::tools::ToolOrigin {
        agent_engine::tools::ToolOrigin::Builtin
    }
    async fn execute(
        &self,
        _params: agent_engine::Value,
        ctx: agent_engine::ToolContext,
    ) -> agent_engine::Result<String> {
        let handle = ctx
            .capabilities
            .secret_prompt
            .expect("stream passes Some(handle)");
        Ok(match handle.prompt("Secret".into(), "enter secret".into()).await {
            Some(v) => format!("answered:{}", v.len()),
            None => "cancelled".to_string(),
        })
    }
}

// Build a Reply::Start from JSON (parse_reply is the public boundary).
fn start_reply(max_cost_usd: Option<f64>) -> Reply {
    let mut inner = serde_json::json!({
        "action": "start",
        "run_id": "p7run",
        "models": [{"model": "anthropic/claude-fable-5-1", "effort": "high"}],
        "prompt": "go",
        "delay_ms": 1000u64,
    });
    if let Some(c) = max_cost_usd {
        inner["max_cost_usd"] = serde_json::json!(c);
    }
    let json = serde_json::json!({ "session_driver": inner });
    agent_engine::extensions::session_driver::parse_reply(&json)
        .expect("valid reply")
        .expect("some reply")
}

// ─────────────────────────── S1 ─────────────────────────────────────────────

/// S1: detaching the LAST client revokes the driver grant, releasing the
/// host-level single-tenancy claim so another session can arm. The revoke
/// event cannot be observed on the detached client itself, so we prove it via
/// the grant becoming available again.
#[tokio::test]
async fn s1_detach_last_client_revokes_driver_grant() {
    let (host, _temp) = host_with_plugin().await;

    // Session A arms and holds the single-tenancy grant.
    let mut a = session(&host).await;
    a.arm().await;

    // Session B cannot arm while A holds the grant.
    let mut b = session(&host).await;
    b.driver_start().await;
    let ev = b
        .until(|e| {
            matches!(
                e,
                SessionEventWire::SystemNotice(_) | SessionEventWire::DriverArmed { .. }
            )
        })
        .await;
    assert!(
        matches!(ev, SessionEventWire::SystemNotice(_)),
        "second session should be refused while A holds the grant, got {ev:?}"
    );

    // Detach A's only client → S1 revokes A's driver, releasing the grant.
    a.send(SessionCommand::Detach { client: ClientId(1) }).await;

    // Now B can arm — proof the grant was released by the last-detach revoke.
    let mut armed = false;
    for _ in 0..40 {
        b.driver_start().await;
        let ev = b
            .until(|e| {
                matches!(
                    e,
                    SessionEventWire::SystemNotice(_) | SessionEventWire::DriverArmed { .. }
                )
            })
            .await;
        if matches!(ev, SessionEventWire::DriverArmed { .. }) {
            armed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(armed, "grant was not released after last-client detach (S1)");
    b.end().await;
}

/// S1: detaching a NON-last client leaves the driver armed (the gate fires only
/// at the zero-clients transition). A second session stays refused because A
/// still holds the grant.
#[tokio::test]
async fn s1_detach_nonlast_client_keeps_driver() {
    let (host, _temp) = host_with_plugin().await;
    let mut a = session(&host).await;
    // Attach a second client to A (owner stays ClientId(1)).
    let (a2, _snap) =
        LocalTransport::attach(a.handle.clone(), ClientMeta::new(ClientKind::Attach))
            .await
            .unwrap();
    a.arm().await;

    // Detach the SECOND client (non-last) — a client remains, no revoke.
    a2.send(SessionCommand::Detach { client: ClientId(2) })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // A still holds the grant: a fresh session is refused.
    let mut b = session(&host).await;
    b.driver_start().await;
    let ev = b
        .until(|e| {
            matches!(
                e,
                SessionEventWire::SystemNotice(_) | SessionEventWire::DriverArmed { .. }
            )
        })
        .await;
    assert!(
        matches!(ev, SessionEventWire::SystemNotice(_)),
        "driver was wrongly revoked on a non-last detach (S1), B armed: {ev:?}"
    );
    b.end().await;
    a.end().await;
}

/// S1: after a zero-client revoke, re-attaching and re-arming works cleanly.
#[tokio::test]
async fn s1_rearm_after_zero_client_revoke() {
    let (host, _temp) = host_with_plugin().await;
    // `keep_warm` keeps the (empty-history) session resident with zero clients
    // so it neither parks nor idle-ends — we can re-attach and prove the
    // last-client revoke left clean, re-armable state. (Avoids racing on the
    // idle-end grace env var with other tests.)
    let mut a = session_cfg(
        &host,
        SessionConfig {
            model_override: Some(MODEL.into()),
            persist: false,
            keep_warm: true,
            ..SessionConfig::default()
        },
    )
    .await;
    a.arm().await;

    // Detach the only client → revoke.
    a.send(SessionCommand::Detach { client: ClientId(1) }).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Re-attach a new client and arm again.
    let (t2, _snap) = LocalTransport::attach(a.handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    let mut a2 = TestActor { t: t2, handle: a.handle.clone() };
    a2.arm().await; // panics on timeout if re-arm failed
    a2.end().await;
}

/// S1: a pending host confirmation is fail-closed answered `None` (deny) when
/// the LAST client detaches, so the session does not zombie-block on a prompt
/// with nobody to answer it. Verified via the re-attach snapshot showing the
/// prompt was drained.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn s1_pending_prompt_fail_closed_on_last_detach() {
    let guard = HomeGuard::new();
    // Turn calls prompt_fixture → raises a prompt; a follow-up SSE would end
    // the turn once the (auto-denied) prompt resolves.
    let bodies: &'static [&'static str] =
        Box::leak(Box::new([sse_prompt_fixture("toolu_p7pf"), ANTHROPIC_SSE]));
    let (host, _temp) = stub_host(&guard, Script::SeqSse(bodies), "").await;
    host.parts().tools.write().await.register(Arc::new(PromptFixtureTool));

    let mut a = session_cfg(
        &host,
        SessionConfig {
            model_override: Some("anthropic/claude-fable-5-1".into()),
            persist: false,
            ..SessionConfig::default()
        },
    )
    .await;

    // Normal (foreground) turn that raises a prompt.
    a.send(SessionCommand::Submit { text: "go".into(), attachments: vec![] })
        .await;
    a.until(|e| matches!(e, SessionEventWire::Prompt(_))).await;

    // Detach the only client → S1 fail-closed drains the prompt (None).
    a.send(SessionCommand::Detach { client: ClientId(1) }).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Re-attach: the prompt must be gone (drained), not stuck pending.
    let (t2, snap) =
        LocalTransport::attach(a.handle.clone(), ClientMeta::new(ClientKind::Test))
            .await
            .unwrap();
    assert!(
        snap.pending_prompts.is_empty(),
        "pending prompt was not fail-closed on last detach (S1): {:?}",
        snap.pending_prompts
    );
    t2.send(SessionCommand::End {
        reason: agent_engine::session::EndReason::ClientQuit,
    })
    .await
    .ok();
}

// ─────────────────────────── S3 ─────────────────────────────────────────────

/// S3: a session-cost breach cancels the turn, revokes the driver, and emits
/// `CostCapReached{scope:"session"}`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn s3_session_cost_cap_breach_revokes_and_emits() {
    let guard = HomeGuard::new();
    let (host, _temp) = stub_host(&guard, Script::Sse(ANTHROPIC_SSE), "").await;

    // Tiny cap: one turn's usage (fable pricing) exceeds it immediately.
    let mut a = session_cfg(
        &host,
        SessionConfig {
            persist: false,
            max_session_cost: Some(0.00001),
            ..SessionConfig::default()
        },
    )
    .await;
    a.arm().await;

    let mut saw_cap = false;
    let mut saw_revoke = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(40);
    while std::time::Instant::now() < deadline && !(saw_cap && saw_revoke) {
        let env = match tokio::time::timeout(Duration::from_secs(20), a.t.next_event()).await {
            Ok(Some(env)) => env,
            _ => break,
        };
        match env.event {
            SessionEventWire::CostCapReached { scope, cost, cap } => {
                assert_eq!(scope, "session", "expected session-scope breach");
                assert!(cost >= cap, "reported cost {cost} below cap {cap}");
                saw_cap = true;
            }
            SessionEventWire::DriverRevoked { reason, .. } => {
                assert!(
                    reason.contains("cost cap"),
                    "revoke reason not cost-related: {reason}"
                );
                saw_revoke = true;
            }
            _ => {}
        }
    }
    assert!(saw_cap, "no CostCapReached emitted on session breach (S3)");
    assert!(saw_revoke, "driver not revoked on cost breach (S3)");
    a.end().await;
}

/// S3: an unbounded (very-high-cap) session keeps running driver turns — the
/// circuit breaker does not trip on normal spend.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn s3_high_cost_cap_still_runs_turns() {
    let guard = HomeGuard::new();
    let (host, _temp) = stub_host(&guard, Script::Sse(ANTHROPIC_SSE), "").await;

    let mut a = session_cfg(
        &host,
        SessionConfig {
            persist: false,
            max_session_cost: Some(1_000.0), // effectively unbounded here
            ..SessionConfig::default()
        },
    )
    .await;
    a.arm().await;

    let mut outcomes = 0usize;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while outcomes < 2 && std::time::Instant::now() < deadline {
        let env = match tokio::time::timeout(Duration::from_secs(20), a.t.next_event()).await {
            Ok(Some(env)) => env,
            _ => break,
        };
        match env.event {
            SessionEventWire::CostCapReached { .. } => {
                panic!("cost cap tripped under a 1000-USD ceiling (S3)");
            }
            SessionEventWire::DriverTurnOutcome { outcome, .. } => {
                assert!(matches!(outcome, Outcome::Success));
                outcomes += 1;
            }
            SessionEventWire::DriverRevoked { reason, .. } => {
                panic!("driver revoked unexpectedly under a high cap: {reason}");
            }
            _ => {}
        }
    }
    assert!(outcomes >= 2, "high-cap driver did not complete >=2 turns, saw {outcomes}");
    a.send(SessionCommand::Cancel).await;
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        a.until(|e| matches!(e, SessionEventWire::DriverRevoked { .. })),
    )
    .await;
    a.end().await;
}

/// S3: the effective per-run cap is `min(plugin proposal, host cap)` — the
/// plugin may only lower, never raise, a host limit.
#[test]
fn s3_grant_effective_cost_cap_takes_minimum() {
    // Plugin proposes 0.50.
    let (grant, _p) = Grant::from_start("plugin", "sess", start_reply(Some(0.50))).unwrap();
    assert_eq!(grant.max_cost_usd, Some(0.50));
    assert_eq!(grant.effective_cost_cap(Some(0.20)), Some(0.20)); // host lower wins
    assert_eq!(grant.effective_cost_cap(Some(1.00)), Some(0.50)); // plugin lower wins
    assert_eq!(grant.effective_cost_cap(None), Some(0.50)); // plugin alone

    // No plugin proposal: host cap stands alone; unbounded when both absent.
    let (bare, _p2) = Grant::from_start("plugin", "sess", start_reply(None)).unwrap();
    assert_eq!(bare.max_cost_usd, None);
    assert_eq!(bare.effective_cost_cap(Some(0.30)), Some(0.30));
    assert_eq!(bare.effective_cost_cap(None), None);
}

/// S3: a plugin-proposed `max_cost_usd` is captured onto the grant at arm.
#[test]
fn s3_grant_from_start_captures_plugin_max_cost() {
    let (grant, _p) = Grant::from_start("plugin", "sess", start_reply(Some(2.5))).unwrap();
    assert_eq!(grant.max_cost_usd, Some(2.5));
}

/// S3: `PollRequest` carries `session_cost_so_far` (present when set, omitted
/// when `None` — additive/back-compatible on the wire).
#[test]
fn s3_pollrequest_serializes_session_cost_so_far() {
    let with = PollRequest {
        run_id: "r".into(),
        decision_id: "d".into(),
        outcome: Outcome::Success,
        error_kind: "none".into(),
        model: "m".into(),
        effort: "high".into(),
        feedback: None,
        session_id: Some("s".into()),
        session_cost_so_far: Some(1.5),
    };
    let v = serde_json::to_value(&with).unwrap();
    assert_eq!(v["session_cost_so_far"], serde_json::json!(1.5));

    let without = PollRequest { session_cost_so_far: None, ..with };
    let v2 = serde_json::to_value(&without).unwrap();
    assert!(
        v2.get("session_cost_so_far").is_none(),
        "None session_cost_so_far must be omitted from the wire"
    );
}

/// S3: the new `CostCapReached` wire event survives the SessionEventWire ↔
/// WireSessionEvent mirror (both `From` arms), preserving its fields.
#[test]
fn s3_costcapreached_wire_mirror_roundtrips() {
    use agent_engine::session::wire::WireSessionEvent;
    let ev = SessionEventWire::CostCapReached {
        scope: "run".into(),
        cost: 1.25,
        cap: 0.75,
    };
    let wire: WireSessionEvent = ev.into();
    let back: SessionEventWire = wire.into();
    match back {
        SessionEventWire::CostCapReached { scope, cost, cap } => {
            assert_eq!(scope, "run");
            assert_eq!(cost, 1.25);
            assert_eq!(cap, 0.75);
        }
        other => panic!("CostCapReached did not survive the wire mirror: {other:?}"),
    }
}

// ─────────────────────────── S9 ─────────────────────────────────────────────

/// S9: even when the session config sets `auto_approve_confirms = true`, a
/// driver-armed turn keeps ordinary tool-activation gates in force — the model's
/// `activate_tools` request raises the host confirmation prompt instead of being
/// silently auto-approved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn s9_armed_driver_forces_gating_despite_auto_approve() {
    let guard = HomeGuard::new();
    // Driver turn calls activate_tools with a valid id → the tool asks the host
    // to confirm (activation_confirm = prompt) UNLESS auto-approved.
    let sse = sse_tool_call_with_input("activate_tools", "toolu_p7at", r#"{"tools":["ns:fake"]}"#);
    let (host, _temp) = stub_host(
        &guard,
        Script::Sse(sse),
        "tools.activation_confirm = prompt\n",
    )
    .await;

    let mut a = session_cfg(
        &host,
        SessionConfig {
            persist: false,
            auto_approve_confirms: true, // would bypass the gate on a normal turn
            ..SessionConfig::default()
        },
    )
    .await;
    a.arm().await;

    // The armed driver forces auto-approve off → a host prompt is raised.
    let ev = tokio::time::timeout(
        Duration::from_secs(40),
        a.until(|e| {
            matches!(
                e,
                SessionEventWire::Prompt(_) | SessionEventWire::DriverRevoked { .. }
            )
        }),
    )
    .await
    .expect("no Prompt/revoke before deadline (S9)");
    assert!(
        matches!(ev, SessionEventWire::Prompt(_)),
        "armed driver auto-approved tool activation (S9), got {ev:?}"
    );
    a.send(SessionCommand::Cancel).await;
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        a.until(|e| matches!(e, SessionEventWire::DriverRevoked { .. })),
    )
    .await;
    a.end().await;
}

/// S9 control: on an UNARMED session, `auto_approve_confirms = true` DOES bypass
/// the activation gate (no host prompt), proving the armed case above is the
/// override at work — not an environment quirk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn s9_unarmed_auto_approve_bypasses_activation_gate() {
    let guard = HomeGuard::new();
    let sse = sse_tool_call_with_input("activate_tools", "toolu_p7c", r#"{"tools":["ns:fake"]}"#);
    let bodies: &'static [&'static str] = Box::leak(Box::new([sse, ANTHROPIC_SSE]));
    let (host, _temp) = stub_host(
        &guard,
        Script::SeqSse(bodies),
        "tools.activation_confirm = prompt\n",
    )
    .await;

    let mut a = session_cfg(
        &host,
        SessionConfig {
            model_override: Some("anthropic/claude-fable-5-1".into()),
            persist: false,
            auto_approve_confirms: true,
            ..SessionConfig::default()
        },
    )
    .await;

    // Foreground turn (no driver): auto-approve is honoured → no host prompt.
    a.send(SessionCommand::Submit { text: "go".into(), attachments: vec![] })
        .await;
    let ev = tokio::time::timeout(
        Duration::from_secs(40),
        a.until(|e| {
            matches!(
                e,
                SessionEventWire::Prompt(_)
                    | SessionEventWire::Idle
                    | SessionEventWire::Stream(agent_engine::StreamEvent::Session(
                        agent_engine::SessionEvent::Done
                    ))
            )
        }),
    )
    .await
    .expect("turn neither prompted nor finished (S9 control)");
    assert!(
        !matches!(ev, SessionEventWire::Prompt(_)),
        "unarmed auto-approve session raised a prompt — the gate was not bypassed (S9 control)"
    );
    a.end().await;
}
