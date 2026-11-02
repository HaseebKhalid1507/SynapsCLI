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
        .with_env_filter(default_env_filter(
            std::env::var("RUST_LOG").ok().as_deref(),
        ))
        .with_writer(non_blocking)
        .with_target(false)
        .with_thread_ids(true)
        .with_ansi(false)
        .try_init()
    {
        crate::eprintln_lossy!("Failed to initialize logging: {}", e);
        tracing::warn!(error = %e, "failed to initialize logging");
    }

    Some(guard)
}

/// Target of the engine's metadata-only context telemetry. The file
/// formatter hides targets, so its lines are identified by their messages
/// (`context rollover`, `context window settled`, `context rollover
/// deferred`).
pub const CONTEXT_TELEMETRY_TARGET: &str = "synaps::context";

/// The log filter: `rust_log` (the value of `RUST_LOG`; `error` when unset
/// or invalid) plus the crate directives, and `synaps::context=info` unless
/// `rust_log` itself has a directive for that target. Public so a test can
/// check what reaches the log (`default_env_filter(None)` is the daemon's
/// filter with `RUST_LOG` unset).
pub fn default_env_filter(rust_log: Option<&str>) -> tracing_subscriber::EnvFilter {
    let raw = rust_log.unwrap_or_default();
    let mut filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing_subscriber::filter::LevelFilter::ERROR.into())
        .parse_lossy(raw);
    if !names_target(raw, CONTEXT_TELEMETRY_TARGET) {
        filter = filter.add_directive(
            format!("{CONTEXT_TELEMETRY_TARGET}=info")
                .parse()
                .expect("valid directive"),
        );
    }
    with_crate_directives(filter)
}

/// The log file's directives on top of `base`: debug for Synaps's own
/// crates. An event whose target is outside these crates (and outside
/// `synaps::context`, see [`default_env_filter`]) is kept only at ERROR
/// unless `RUST_LOG` says otherwise.
pub fn with_crate_directives(base: tracing_subscriber::EnvFilter) -> tracing_subscriber::EnvFilter {
    base.add_directive("synaps_cli=debug".parse().expect("valid directive"))
        .add_directive("agent_core=debug".parse().expect("valid directive"))
        .add_directive("agent_engine=debug".parse().expect("valid directive"))
        .add_directive("agent_tui=debug".parse().expect("valid directive"))
        .add_directive("tracing=info".parse().expect("valid directive"))
}

/// Whether a directive in `directives` names a target that covers `target`
/// (filter targets match by prefix). A bare level names no target.
fn names_target(directives: &str, target: &str) -> bool {
    directives.split(',').any(|d| {
        let d = d.trim();
        let name = d.split(['[', '=']).next().unwrap_or_default().trim();
        let bare_level = !d.contains(['[', '='])
            && name
                .parse::<tracing_subscriber::filter::LevelFilter>()
                .is_ok();
        !name.is_empty() && !bare_level && target.starts_with(name)
    })
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

    fn context_enabled(rust_log: Option<&str>) -> bool {
        use tracing_subscriber::layer::SubscriberExt as _;
        let subscriber = tracing_subscriber::registry().with(default_env_filter(rust_log));
        tracing::subscriber::with_default(
            subscriber,
            || tracing::enabled!(target: "synaps::context", tracing::Level::INFO),
        )
    }

    fn other_warn_enabled(rust_log: Option<&str>) -> bool {
        use tracing_subscriber::layer::SubscriberExt as _;
        let subscriber = tracing_subscriber::registry().with(default_env_filter(rust_log));
        tracing::subscriber::with_default(
            subscriber,
            || tracing::enabled!(target: "some_dependency", tracing::Level::WARN),
        )
    }

    /// The context telemetry reaches the log file by default; `RUST_LOG`
    /// still decides when it names the target.
    #[test]
    fn context_telemetry_is_on_by_default_and_rust_log_overrides_it() {
        assert_eq!(CONTEXT_TELEMETRY_TARGET, "synaps::context");
        assert!(context_enabled(None));
        assert!(context_enabled(Some("")));
        assert!(context_enabled(Some("warn")));
        assert!(context_enabled(Some("off")), "a bare level names no target");
        assert!(context_enabled(Some("hyper=debug")));
        assert!(!context_enabled(Some("synaps::context=off")));
        assert!(!context_enabled(Some("synaps::context=warn")));
        assert!(!context_enabled(Some("warn,synaps=off")));
        assert!(context_enabled(Some("synaps::context=debug")));
        // The rest of the default is unchanged: errors only elsewhere.
        assert!(!other_warn_enabled(None));
        assert!(other_warn_enabled(Some("warn")));
    }

    #[test]
    fn directive_targets_are_recognized() {
        let t = CONTEXT_TELEMETRY_TARGET;
        assert!(names_target("synaps::context=info", t));
        assert!(names_target("info, synaps::context[span]=debug", t));
        assert!(names_target("synaps", t), "a target alone means trace");
        assert!(!names_target("", t));
        assert!(!names_target("info", t));
        assert!(!names_target("synaps_cli=debug", t));
        assert!(!names_target("synaps::context_other=info", t));
    }
}
