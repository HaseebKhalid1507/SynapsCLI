//! Integration test: `SidecarManager` drives a modality-neutral v2 sidecar
//! fixture and surfaces a final InsertText event.

use std::path::PathBuf;
use std::time::Duration;

use synaps_cli::sidecar::manager::{SidecarLifecycleEvent, SidecarManager};
use synaps_cli::sidecar::protocol::{InsertTextMode, SIDECAR_PROTOCOL_VERSION};

fn locate_sidecar() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock_sidecar_v2.py")
}

#[tokio::test]
async fn manager_drives_sidecar_insert_text_end_to_end() {
    let bin = locate_sidecar();
    assert!(
        bin.is_file(),
        "mock sidecar fixture missing at {}",
        bin.display()
    );

    let mut manager = SidecarManager::spawn(
        &bin,
        &[],
        serde_json::json!({ "protocol_version": SIDECAR_PROTOCOL_VERSION }),
    )
    .await
    .expect("manager spawn should succeed");

    manager.press().await.expect("press should send");
    manager.release().await.expect("release should send");

    let mut got_active = false;
    let mut got_insert_text: Option<String> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        let timed = tokio::time::timeout(remaining, manager.next_event()).await;
        let Ok(Some(event)) = timed else { break };
        match event {
            SidecarLifecycleEvent::StateChanged { state, .. } if state == "active" => {
                got_active = true;
            }
            SidecarLifecycleEvent::InsertText {
                text,
                mode: InsertTextMode::Final,
            } => {
                got_insert_text = Some(text);
                break;
            }
            SidecarLifecycleEvent::Error(err) => panic!("unexpected sidecar error: {err}"),
            _ => {}
        }
    }

    assert!(got_active, "expected active state event");
    assert_eq!(
        got_insert_text.as_deref(),
        Some("hello from sidecar"),
        "expected final InsertText event"
    );

    manager.shutdown().await.expect("graceful shutdown");
}

#[cfg(unix)]
mod slow_startup {
    use super::*;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use synaps_cli::sidecar::manager::SidecarError;

    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "synaps-slow-sidecar-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&dir).unwrap();
            Self { dir }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.join(name)
        }

        fn args(&self, extra: &[&str]) -> Vec<String> {
            let mut args = vec![
                "--pid-file".into(),
                self.path("pid").to_string_lossy().into_owned(),
                "--hello-sent-file".into(),
                self.path("hello").to_string_lossy().into_owned(),
                "--init-received-file".into(),
                self.path("init").to_string_lossy().into_owned(),
            ];
            args.extend(extra.iter().map(|arg| (*arg).to_owned()));
            args
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn bin() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/slow_sidecar_v2.py")
    }

    async fn wait_for_file(path: &Path) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while !std::fs::read_to_string(path).is_ok_and(|text| !text.is_empty()) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fixture did not publish its marker");
    }

    async fn assert_child_exited(fixture: &Fixture) {
        let pid = std::fs::read_to_string(fixture.path("pid"))
            .expect("fixture must publish its PID")
            .parse::<u32>()
            .expect("valid child PID");
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let alive = Command::new("kill")
                    .args(["-0", &pid.to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .expect("kill must be available on Unix")
                    .success();
                if !alive {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("cancelled/failed startup leaked its child process");
    }

    #[tokio::test]
    async fn delayed_hello_blocks_init_until_readiness() {
        let fixture = Fixture::new();
        let bin = bin();
        let gate = fixture.path("gate");
        let args = fixture.args(&[
            "--hello-gate",
            gate.to_str().unwrap(),
            "--hello-delay-ms",
            "200",
        ]);
        let mut startup = Box::pin(SidecarManager::spawn(&bin, &args, serde_json::json!({})));
        let pid_file = fixture.path("pid");
        tokio::select! {
            _ = &mut startup => panic!("startup completed before Hello"),
            _ = wait_for_file(&pid_file) => {}
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut startup)
                .await
                .is_err()
        );
        assert!(!fixture.path("init").exists(), "Init preceded Hello");
        let released = tokio::time::Instant::now();
        std::fs::write(gate, "go").unwrap();
        let mut manager = tokio::time::timeout(Duration::from_secs(3), &mut startup)
            .await
            .expect("delayed Hello must unblock startup")
            .expect("valid Hello should succeed");
        assert!(released.elapsed() >= Duration::from_millis(200));
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(3), manager.next_event())
                .await
                .unwrap(),
            Some(SidecarLifecycleEvent::StateChanged { state, .. }) if state == "ready"
        ));
        manager.shutdown().await.unwrap();
        assert_child_exited(&fixture).await;
    }

    #[tokio::test]
    async fn hello_is_ready_without_post_init_status() {
        let fixture = Fixture::new();
        let mut manager = tokio::time::timeout(
            Duration::from_secs(3),
            SidecarManager::spawn(
                &bin(),
                &fixture.args(&["--no-init-status"]),
                serde_json::json!({}),
            ),
        )
        .await
        .expect("startup must not wait for an optional Init status")
        .expect("Hello alone establishes readiness");
        wait_for_file(&fixture.path("init")).await;
        manager.press().await.unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(3), manager.next_event())
                .await
                .unwrap(),
            Some(SidecarLifecycleEvent::StateChanged { state, .. }) if state == "active"
        ));
        manager.shutdown().await.unwrap();
        assert_child_exited(&fixture).await;
    }

    #[tokio::test]
    async fn stalled_init_write_times_out_and_cleans_up_child() {
        let fixture = Fixture::new();
        let result = tokio::time::timeout(
            Duration::from_secs(6),
            SidecarManager::spawn(
                &bin(),
                &fixture.args(&["--stall-stdin"]),
                serde_json::json!({ "padding": "x".repeat(8 * 1024 * 1024) }),
            ),
        )
        .await
        .expect("Init write must have its own finite deadline");
        assert!(matches!(
            result,
            Err(SidecarError::Io(ref error)) if error.kind() == std::io::ErrorKind::TimedOut
        ));
        assert!(
            fixture.path("hello").exists(),
            "Hello must precede blocked Init"
        );
        assert!(!fixture.path("init").exists());
        assert_child_exited(&fixture).await;
    }

    #[tokio::test]
    async fn cancelling_before_hello_cleans_up_child() {
        let fixture = Fixture::new();
        let bin = bin();
        let gate = fixture.path("never-opened");
        let args = fixture.args(&["--hello-gate", gate.to_str().unwrap()]);
        let mut startup = Box::pin(SidecarManager::spawn(&bin, &args, serde_json::json!({})));
        let pid_file = fixture.path("pid");
        tokio::select! {
            _ = &mut startup => panic!("startup completed without Hello"),
            _ = wait_for_file(&pid_file) => {}
        }
        drop(startup);
        assert!(!fixture.path("hello").exists());
        assert_child_exited(&fixture).await;
    }

    #[tokio::test]
    async fn cancelling_blocked_init_write_cleans_up_child() {
        let fixture = Fixture::new();
        let bin = bin();
        let args = fixture.args(&["--stall-stdin"]);
        let mut startup = Box::pin(SidecarManager::spawn(
            &bin,
            &args,
            serde_json::json!({ "padding": "x".repeat(8 * 1024 * 1024) }),
        ));
        let hello_file = fixture.path("hello");
        tokio::select! {
            _ = &mut startup => panic!("startup completed despite stalled reader"),
            _ = wait_for_file(&hello_file) => {}
        }
        // Poll through Hello into the partial Init write, without waiting for
        // the manager's two-second deadline. Dropping cancels that write.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut startup)
                .await
                .is_err()
        );
        drop(startup);
        assert!(!fixture.path("init").exists());
        assert_child_exited(&fixture).await;
    }
}
