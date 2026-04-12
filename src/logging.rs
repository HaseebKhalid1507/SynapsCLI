use tracing_appender::non_blocking::WorkerGuard;
use std::path::Path;

pub fn init_logging() -> Option<WorkerGuard> {
    if let Ok(home) = std::env::var("HOME") {
        let log_dir = Path::new(&home).join(".synaps-cli");
        if !log_dir.exists() {
            let _ = std::fs::create_dir_all(&log_dir);
        }

        let file_appender = tracing_appender::rolling::daily(log_dir, "synaps.log");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

        if let Err(e) = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("synaps_cli=debug".parse().unwrap())
                .add_directive("tracing=info".parse().unwrap()))
            .with_writer(non_blocking)
            .with_target(false)
            .with_thread_ids(true)
            .with_ansi(false)
            .try_init()
        {
            eprintln!("Failed to initialize logging: {}", e);
        }

        Some(guard)
    } else {
        None
    }
}
