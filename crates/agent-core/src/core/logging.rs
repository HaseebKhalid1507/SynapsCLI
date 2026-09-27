use tracing_appender::non_blocking::WorkerGuard;

pub fn init_logging() -> Option<WorkerGuard> {
    let log_dir = crate::config::get_active_config_dir();
    if !log_dir.exists() {
        let _ = std::fs::create_dir_all(&log_dir);
    }

    let file_appender = tracing_appender::rolling::daily(log_dir, "synaps.log");
    // `tracing_appender::non_blocking()` defaults to a 128 000-slot bounded
    // channel whose slots are all touched at construction (~3.9 MiB of anon
    // per process, forever). 16 k lines is far beyond any burst we have seen
    // and the writer is lossy under backlog either way. Kill-switch:
    // `SYNAPS_LOG_BUFFER_LINES=128000` restores the old buffer.
    let (non_blocking, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .buffered_lines_limit(log_buffer_lines())
        .lossy(true)
        .finish(file_appender);

    if let Err(e) = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("synaps_cli=debug".parse().expect("valid directive"))
                .add_directive("agent_core=debug".parse().expect("valid directive"))
                .add_directive("agent_engine=debug".parse().expect("valid directive"))
                .add_directive("agent_tui=debug".parse().expect("valid directive"))
                .add_directive("tracing=info".parse().expect("valid directive")),
        )
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

/// Default bounded-channel capacity for the non-blocking log writer.
pub const DEFAULT_LOG_BUFFER_LINES: usize = 16_384;

/// Buffer size for the non-blocking log writer: `SYNAPS_LOG_BUFFER_LINES`
/// when set to a positive integer, otherwise [`DEFAULT_LOG_BUFFER_LINES`].
pub fn log_buffer_lines() -> usize {
    log_buffer_lines_from(std::env::var("SYNAPS_LOG_BUFFER_LINES").ok().as_deref())
}

fn log_buffer_lines_from(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_LOG_BUFFER_LINES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_buffer_lines_honours_env_and_falls_back() {
        assert_eq!(log_buffer_lines_from(None), DEFAULT_LOG_BUFFER_LINES);
        assert_eq!(log_buffer_lines_from(Some("128000")), 128_000);
        assert_eq!(log_buffer_lines_from(Some(" 42 ")), 42);
        assert_eq!(log_buffer_lines_from(Some("0")), DEFAULT_LOG_BUFFER_LINES);
        assert_eq!(
            log_buffer_lines_from(Some("nope")),
            DEFAULT_LOG_BUFFER_LINES
        );
    }
}
