use tracing_appender::non_blocking::WorkerGuard;

pub fn live_debug_log_path() -> std::path::PathBuf {
    crate::config::get_active_config_dir().join("synaps-debug.log")
}

pub fn init_logging() -> Option<WorkerGuard> {
    let log_dir = crate::config::get_active_config_dir();
    if !log_dir.exists() {
        let _ = std::fs::create_dir_all(&log_dir);
    }

    let log_path = live_debug_log_path();
    let file_appender = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(file) => file,
        Err(e) => {
            eprintln!("Failed to open debug log {}: {}", log_path.display(), e);
            return None;
        }
    };
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    if let Err(e) = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env()
            .add_directive("synaps_cli=debug".parse().expect("valid directive"))
            .add_directive("tracing=info".parse().expect("valid directive")))
        .with_writer(non_blocking)
        .with_target(false)
        .with_thread_ids(true)
        .with_ansi(false)
        .try_init()
    {
        eprintln!("Failed to initialize logging: {}", e);
    }

    Some(guard)
}
