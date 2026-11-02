//! Shared test helpers for tool unit tests.

use super::{ToolCapabilities, ToolChannels, ToolContext, ToolLimits};

pub(crate) fn create_tool_context() -> ToolContext {
    let foreground = agent_core::prompt::QualifiedModelId::parse("anthropic/claude-sonnet-4-6")
        .expect("test foreground is qualified");
    ToolContext {
        channels: ToolChannels {
            tx_delta: None,
            tx_events: None,
        },
        capabilities: ToolCapabilities {
            session_allow_all: None,
            launch_cancel: None,
            memory_backend: None,
            telemetry: None,
            continuation: None,
            watcher_exit_path: None,
            tool_register_tx: None,
            session_manager: None,
            subagent_registry: None,
            event_queue: None,
            delegation_parent: None,
            codex_parent_plan: None,
            worker_context: None,
            secret_prompt: None,
            orchestration: Some(std::sync::Arc::new(
                crate::orchestration::OrchestrationRuntime::baseline(foreground, 8, 64)
                    .expect("test foreground is routable"),
            )),
            tool_activation: None,
            mcp_leases: None,
            extension_leases: None,
            memory_context: None,
            cwd: None,
            env: None,
            env_stripped: Vec::new(),
            env_warned: Default::default(),
            sandbox: None,
            session_id: None,
            tool_use_id: None,
            turn_correlation_id: None,
            spend: None,
        },
        limits: ToolLimits {
            max_tool_output: 30000,
            max_tool_buffer: 256 * 1024,
            bash_timeout: 30,
            bash_max_timeout: 300,
            subagent_timeout: 300,
        },
    }
}

/// A memory binding of `kind` whose paths all lie in `dir`. Building it
/// starts no service and writes nothing; an Axel binding is exclusive.
pub(crate) fn memory_binding(
    kind: agent_core::config::MemoryBackendKind,
    dir: &std::path::Path,
) -> crate::memory_backend::MemoryBinding {
    crate::memory_backend::MemoryBinding::from_config_with_cwd(
        &agent_core::config::MemoryBackendConfig {
            kind,
            executable: Some(dir.join("missing-service")),
            brain: Some(dir.join("brain.r8")),
            user_scope: false,
        },
        Some(dir.to_path_buf()),
    )
}

/// A tool context carrying `binding` as its memory backend.
pub(crate) fn context_with_memory(binding: crate::memory_backend::MemoryBinding) -> ToolContext {
    let mut ctx = create_tool_context();
    ctx.capabilities.memory_backend = Some(binding);
    ctx
}

/// One event the daemon log's filter let through.
#[derive(Debug, Clone)]
pub(crate) struct LoggedLine {
    pub target: String,
    pub level: tracing::Level,
    pub message: String,
    pub fields: std::collections::BTreeMap<String, String>,
}

/// The events that reach the daemon log, on this thread, while the guard
/// lives: the daemon's own filter (`agent_core::logging::with_crate_directives`
/// over what `RUST_LOG` unset gives, exactly what `init_logging` builds)
/// in front of a recorder. Use on a current-thread runtime, so every event
/// of the code under test is on this thread.
///
/// Also keeps a second, empty dispatcher alive with the guard. While
/// exactly one dispatcher is registered, tracing-core caches a callsite's
/// interest from whatever default the REGISTERING thread has (its
/// `has_just_one` shortcut): a parallel test that reached a shared callsite
/// ("wait ended") first, on a thread without a capture, cached `never`,
/// and this capture saw nothing. With two registered, every callsite is
/// judged against all live dispatchers, this one included.
#[derive(Clone, Default)]
pub(crate) struct DaemonLog(std::sync::Arc<std::sync::Mutex<Vec<LoggedLine>>>);

/// [`DaemonLog::capture`]'s guard: the thread default first, then the
/// dispatcher that kept the shortcut off.
pub(crate) struct CaptureGuard {
    _default: tracing::subscriber::DefaultGuard,
    _second: tracing::Dispatch,
}

impl DaemonLog {
    pub(crate) fn capture() -> (Self, CaptureGuard) {
        use tracing_subscriber::filter::{EnvFilter, LevelFilter};
        use tracing_subscriber::layer::SubscriberExt;
        let second = tracing::Dispatch::new(tracing_subscriber::registry());
        let unset = EnvFilter::builder()
            .with_default_directive(LevelFilter::ERROR.into())
            .parse_lossy("");
        let log = Self::default();
        let guard = tracing::subscriber::set_default(
            tracing_subscriber::registry()
                .with(agent_core::logging::with_crate_directives(unset))
                .with(log.clone()),
        );
        (
            log,
            CaptureGuard {
                _default: guard,
                _second: second,
            },
        )
    }

    pub(crate) fn lines(&self) -> Vec<LoggedLine> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The lines at `target` and `level` whose message is `message`.
    pub(crate) fn find(
        &self,
        target: &str,
        level: tracing::Level,
        message: &str,
    ) -> Vec<LoggedLine> {
        self.lines()
            .into_iter()
            .filter(|l| l.target == target && l.level == level && l.message == message)
            .collect()
    }

    /// Whether any line that got through has a message starting `prefix`.
    pub(crate) fn passed(&self, prefix: &str) -> bool {
        self.lines().iter().any(|l| l.message.starts_with(prefix))
    }
}

struct LineFields<'a>(&'a mut LoggedLine);

impl tracing::field::Visit for LineFields<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.record(field, value.to_owned());
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.record(field, format!("{value:?}"));
    }
}

impl LineFields<'_> {
    fn record(&mut self, field: &tracing::field::Field, value: String) {
        if field.name() == "message" {
            self.0.message = value;
        } else {
            self.0.fields.insert(field.name().to_owned(), value);
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for DaemonLog {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut line = LoggedLine {
            target: event.metadata().target().to_owned(),
            level: *event.metadata().level(),
            message: String::new(),
            fields: Default::default(),
        };
        event.record(&mut LineFields(&mut line));
        self.0.lock().unwrap_or_else(|p| p.into_inner()).push(line);
    }
}
