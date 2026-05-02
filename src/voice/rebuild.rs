//! Backend rebuild orchestration.
//!
//! Spawns the voice plugin's `scripts/setup.sh` with the requested feature
//! flags, streams stdout+stderr lines through an mpsc channel, and on exit
//! re-runs `discovery::read_build_info()` so callers can confirm the new
//! backend is live.
//!
//! Tests cover [`resolve_features_for_backend`] (pure helper) and the
//! "no setup.sh present" failure path. We deliberately do NOT exercise
//! an end-to-end real `cargo build` in tests — slow and platform-dependent.

use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::Sender;

use super::discovery;

/// One streamed line from the rebuild process.
#[derive(Debug, Clone)]
pub struct RebuildOutput {
    pub line: String,
    pub from_stderr: bool,
}

/// Lifecycle event published over the rebuild channel.
#[derive(Debug)]
pub enum RebuildEvent {
    /// A streamed line of stdout/stderr output.
    Output(RebuildOutput),
    /// The rebuild process exited normally (any exit code).
    /// `new_backend` is `Some(backend)` if `read_build_info()` succeeded
    /// against the post-rebuild binary.
    Done {
        exit_code: i32,
        new_backend: Option<String>,
    },
    /// The rebuild could not be started (e.g. setup.sh missing, spawn failed).
    Failed(String),
}

/// Translate a backend name into the `--features` argument value passed to
/// `setup.sh`. CPU is implicit (just `local-stt`); other backends are
/// appended as a comma-separated feature.
pub fn resolve_features_for_backend(backend: &str) -> String {
    match backend {
        "" | "cpu" => "local-stt".to_string(),
        other => format!("local-stt,{}", other),
    }
}

/// Resolve a possibly-`auto` backend selection to a concrete one by
/// probing the host. Pure-ish helper exposed for the slash command.
pub fn resolve_backend(selected: &str) -> String {
    match selected {
        "" | "auto" => discovery::detect_host_backend().to_string(),
        other => other.to_string(),
    }
}

/// Spawn `bash <plugin_dir>/scripts/setup.sh --features local-stt[,<backend>]`
/// and stream its output to `tx`. After exit, re-probe the rebuilt binary
/// via [`discovery::read_build_info`] and send `Done`.
///
/// `binary_after_rebuild` is the path to the sidecar binary that should
/// exist when the rebuild completes (used for the post-rebuild probe).
pub async fn rebuild_with_backend(
    plugin_dir: PathBuf,
    binary_after_rebuild: PathBuf,
    backend: String,
    tx: Sender<RebuildEvent>,
) {
    let setup = plugin_dir.join("scripts/setup.sh");
    if !setup.is_file() {
        let _ = tx
            .send(RebuildEvent::Failed(format!(
                "scripts/setup.sh not found at {} — is the local-voice plugin installed?",
                setup.display()
            )))
            .await;
        return;
    }

    let features = resolve_features_for_backend(&backend);
    let mut cmd = Command::new("bash");
    cmd.arg(&setup)
        .arg("--features")
        .arg(&features)
        .current_dir(&plugin_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx
                .send(RebuildEvent::Failed(format!(
                    "failed to spawn setup.sh: {}",
                    e
                )))
                .await;
            return;
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_out = tx.clone();
    let tx_err = tx.clone();

    let stdout_task = tokio::spawn(async move {
        if let Some(s) = stdout {
            let mut lines = BufReader::new(s).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx_out
                    .send(RebuildEvent::Output(RebuildOutput {
                        line,
                        from_stderr: false,
                    }))
                    .await;
            }
        }
    });
    let stderr_task = tokio::spawn(async move {
        if let Some(s) = stderr {
            let mut lines = BufReader::new(s).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx_err
                    .send(RebuildEvent::Output(RebuildOutput {
                        line,
                        from_stderr: true,
                    }))
                    .await;
            }
        }
    });

    let status = child.wait().await;
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    let exit_code = match status {
        Ok(s) => s.code().unwrap_or(-1),
        Err(e) => {
            let _ = tx
                .send(RebuildEvent::Failed(format!(
                    "failed to wait for setup.sh: {}",
                    e
                )))
                .await;
            return;
        }
    };

    let new_backend = if binary_after_rebuild.is_file() {
        discovery::read_build_info(&binary_after_rebuild).map(|i| i.backend)
    } else {
        None
    };

    let _ = tx
        .send(RebuildEvent::Done {
            exit_code,
            new_backend,
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[test]
    fn resolve_features_cpu_is_local_stt() {
        assert_eq!(resolve_features_for_backend("cpu"), "local-stt");
        assert_eq!(resolve_features_for_backend(""), "local-stt");
    }

    #[test]
    fn resolve_features_for_backend_appends_feature() {
        assert_eq!(resolve_features_for_backend("cuda"), "local-stt,cuda");
        assert_eq!(resolve_features_for_backend("metal"), "local-stt,metal");
        assert_eq!(resolve_features_for_backend("vulkan"), "local-stt,vulkan");
        assert_eq!(
            resolve_features_for_backend("openblas"),
            "local-stt,openblas"
        );
    }

    #[tokio::test]
    async fn rebuild_fails_cleanly_when_setup_sh_missing() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        rebuild_with_backend(
            dir.path().to_path_buf(),
            dir.path().join("bin/synaps-voice-plugin"),
            "cpu".to_string(),
            tx,
        )
        .await;
        let evt = rx.recv().await.expect("should emit Failed");
        match evt {
            RebuildEvent::Failed(msg) => assert!(
                msg.contains("setup.sh"),
                "unexpected failure message: {}",
                msg
            ),
            other => panic!("expected Failed, got {:?}", other),
        }
    }
}
