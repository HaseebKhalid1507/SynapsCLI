//! At daemon start the log says when no guard is required.
//!
//! One test, alone in its own binary: the process's required-guards gate
//! only grows (a guard named by one host stays required), so the empty
//! config is started first and the one naming a guard second.

use std::sync::{Arc, Mutex};

use agent_engine::daemon::{Daemon, DaemonOpts};
use agent_engine::{EngineHost, HostOpts, McpDiscovery};

#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogCapture {
    fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync {
        let sink = self.0.clone();
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || LogWriter(sink.clone()))
            .with_ansi(false)
            .finish()
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

/// Boot a host over `config`, start a daemon on it, and return what the
/// daemon start logged at warn level.
async fn warnings_of_a_daemon_start(config: &str) -> String {
    let base = tempfile::tempdir().expect("tempdir");
    std::fs::write(base.path().join("config"), config).expect("write config");
    std::env::set_var("SYNAPS_BASE_DIR", base.path());
    std::env::remove_var("SYNAPS_PROFILE");
    let host = EngineHost::boot(HostOpts {
        profile: None,
        no_extensions: true,
        mcp_discovery: McpDiscovery::Inline,
    })
    .await
    .expect("host boot");

    let run = tempfile::tempdir().expect("tempdir");
    let log = LogCapture::default();
    let daemon = {
        let _guard = tracing::subscriber::set_default(log.subscriber());
        Daemon::start(
            host,
            DaemonOpts {
                runtime_dir: Some(run.path().to_path_buf()),
                ..Default::default()
            },
        )
        .await
        .expect("daemon start")
    };
    daemon.shutdown_token().cancel();
    daemon.wait().await;
    log.text()
}

const NOTICE: &str = "daemon: no required guards configured (required_guards is absent or empty in the config); no tool call waits on a guard being loaded";

#[tokio::test]
async fn a_daemon_started_with_no_required_guards_warns_and_one_with_a_guard_does_not() {
    for config in ["", "model = claude-sonnet-4-6\n", "required_guards =\n"] {
        let text = warnings_of_a_daemon_start(config).await;
        assert!(text.contains("WARN"), "{config:?}: {text}");
        assert!(text.contains(NOTICE), "{config:?}: {text}");
    }
    let text = warnings_of_a_daemon_start("required_guards = example-guard\n").await;
    assert!(!text.contains("no required guards configured"), "{text}");
}
