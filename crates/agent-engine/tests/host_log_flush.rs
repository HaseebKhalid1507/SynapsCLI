//! `EngineHost::flush_logs` drains the non-blocking appender: a line logged
//! immediately before the flush is on disk when it returns. Own process —
//! the host installs the global tracing subscriber.

use agent_engine::{EngineHost, HostOpts};

#[tokio::test]
async fn line_logged_before_flush_reaches_the_file() {
    let home = std::env::temp_dir().join(format!("synaps-log-flush-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    agent_engine::config::set_base_dir_for_tests(home.clone());

    let h = EngineHost::boot(HostOpts {
        profile: None,
        no_extensions: true,
    })
    .await
    .expect("host boot");
    assert!(EngineHost::install(h).is_ok());

    let marker = format!("flush-marker-{}", std::process::id());
    // Subscriber filter admits the crate targets, not this test binary.
    tracing::info!(target: "agent_engine", "{marker}");
    EngineHost::flush_installed_logs();
    // Idempotent.
    EngineHost::flush_installed_logs();

    let log_dir = agent_engine::config::get_active_config_dir();
    let mut found = false;
    for entry in std::fs::read_dir(&log_dir).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("synaps.log") {
            let body = std::fs::read_to_string(entry.path()).unwrap_or_default();
            if body.contains(&marker) {
                found = true;
            }
        }
    }
    assert!(found, "marker must be flushed to {}", log_dir.display());
}
