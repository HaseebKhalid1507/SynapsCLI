//! Extension runtime trait and registry.

pub mod process;
pub mod restart;

pub use restart::RestartPolicy;

use self::process::{
    NotificationFrame, ProviderCompleteParams, ProviderCompleteResult, ProviderStreamEvent,
};
use crate::extensions::commands::CommandOutputEvent;
use crate::extensions::hooks::events::{HookEvent, HookResult};
use crate::extensions::info::PluginInfo;
use crate::extensions::tasks::TaskEvent;
use async_trait::async_trait;
use serde_json::Value;

/// Streamed event delivered to a `command.invoke` caller.
#[derive(Debug, Clone, PartialEq)]
pub enum InvokeCommandEvent {
    /// Command output event matching the caller's `request_id`.
    Output(CommandOutputEvent),
    /// Spontaneous plugin task event (any `request_id`).
    Task(TaskEvent),
}

/// Health state for an extension handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionHealth {
    /// Manifest validated, process spawned, but `initialize` not yet completed.
    Loaded,
    /// Manifest failed validation — the extension never started.
    FailedValidation,
    /// `initialize` rpc failed — the extension started but couldn't capability-handshake.
    FailedInitialize,
    /// Healthy and serving requests.
    Running,
    /// Process restarting after transport failure but within restart budget.
    Restarting,
    /// Running, but at least one recent operation failed (e.g. hook timeout).
    Degraded,
    /// Permanent failure — restart budget exhausted or unrecoverable error.
    Failed,
}

impl ExtensionHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loaded => "loaded",
            Self::FailedValidation => "failed_validation",
            Self::FailedInitialize => "failed_initialize",
            Self::Running => "running",
            Self::Restarting => "restarting",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
        }
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod health_tests {
    use super::ExtensionHealth;

    #[test]
    fn as_str_covers_all_variants() {
        assert_eq!(ExtensionHealth::Loaded.as_str(), "loaded");
        assert_eq!(
            ExtensionHealth::FailedValidation.as_str(),
            "failed_validation"
        );
        assert_eq!(
            ExtensionHealth::FailedInitialize.as_str(),
            "failed_initialize"
        );
        assert_eq!(ExtensionHealth::Running.as_str(), "running");
        assert_eq!(ExtensionHealth::Restarting.as_str(), "restarting");
        assert_eq!(ExtensionHealth::Degraded.as_str(), "degraded");
        assert_eq!(ExtensionHealth::Failed.as_str(), "failed");
    }
}

/// A point-in-time lifecycle observation, not a lease on future execution.
///
/// Generations are opaque and local to one handler instance. A caller must pin
/// both the handler identity and generation before dispatch, then require the
/// same generation with `Running` or `Degraded` health when accepting a reply
/// and before executing any deferred work. All other health states fail closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtensionLifecycle {
    pub generation: u64,
    pub health: ExtensionHealth,
}

/// Trait for extension runtimes that can handle hook events.
#[async_trait]
pub trait ExtensionHandler: Send + Sync {
    /// Unique identifier for this extension.
    fn id(&self) -> &str;

    /// Observe the actual serving runtime without waiting for locks or RPC I/O.
    ///
    /// `None` means unsupported or temporarily unavailable, never "healthy".
    /// Implementations must invalidate the generation on runtime/transport loss
    /// and before replacement, even if the handler itself remains the same Arc.
    /// Consumers retaining authority must fail closed on `None`, non-live health,
    /// or a changed generation, including while a previously valid reply waits
    /// to execute. Recheck at dispatch, reply acceptance, and each frontend tick;
    /// this snapshot cannot eliminate exit races after the final observation.
    fn lifecycle_snapshot(&self) -> Option<ExtensionLifecycle> {
        None
    }

    /// Why this extension, a required guard the manager has loaded, cannot
    /// answer a tool call now (its process exited, was killed, or was
    /// stopped), or `None` while it is live. Waits for the brief locks that
    /// publish a process, never for RPC I/O. Default: a handler with no
    /// process to check is never known live, so it is down (fail closed).
    async fn guard_down(&self) -> Option<String> {
        Some("its handler reports no process liveness".to_string())
    }

    /// Handle a hook event. Returns the handler's decision.
    async fn handle(&self, event: &HookEvent) -> HookResult;

    /// [`Self::handle`] for a caller that must never start or restart the
    /// extension: a subagent worker's hook view
    /// ([`HookBus::worker_transforms`]). A worker runs on its own throwaway
    /// current-thread tokio runtime, so a child process or reader task
    /// spawned from it would die with the worker and leave the shared
    /// extension broken for the foreground and every other worker. An
    /// extension that is not running (crashed, stopped by a reload, deferred
    /// and not started yet) answers `Continue`; the foreground's next call
    /// starts it as before. Default: [`Self::handle`] (nothing to start).
    ///
    /// [`HookBus::worker_transforms`]: crate::extensions::hooks::HookBus::worker_transforms
    async fn handle_no_spawn(&self, event: &HookEvent) -> HookResult {
        self.handle(event).await
    }

    /// [`Self::handle`] (or [`Self::handle_no_spawn`] when `no_spawn`) for a
    /// guard that fails closed (manifest `on_failure: "block"`): when the
    /// extension cannot answer (no reply in time, dead or stopped child,
    /// failed start or respawn) the result is a `Block`, never `Continue`
    /// ([`HookFailurePolicy::unanswered`]). Default: a handler with no such
    /// failure mode answers as usual.
    ///
    /// [`HookFailurePolicy::unanswered`]: crate::extensions::hooks::events::HookFailurePolicy::unanswered
    async fn handle_fail_closed(&self, event: &HookEvent, no_spawn: bool) -> HookResult {
        if no_spawn {
            self.handle_no_spawn(event).await
        } else {
            self.handle(event).await
        }
    }

    /// One `before_tool_call` for a required guard (the config's
    /// `required_guards`), whatever its manifest's `on_failure` says. `Ok`
    /// is its answer; `Err` is why it gave none the bus may use, and the bus
    /// blocks the call naming the cause
    /// ([`no_answer_text`](crate::extensions::required_guards::no_answer_text)).
    /// The bus's own 5 s timeout and its check of the answer's kind apply on
    /// top. Default: [`Self::handle_fail_closed`]'s answer (a handler with no
    /// such failure mode answers as usual).
    async fn handle_required(
        &self,
        event: &HookEvent,
        no_spawn: bool,
    ) -> Result<HookResult, crate::extensions::required_guards::NoAnswer> {
        Ok(self.handle_fail_closed(event, no_spawn).await)
    }

    /// One `hook.handle` call whose answer is the raw JSON result, not a
    /// [`HookResult`] decision: how the host asks the `sandbox_policy`
    /// question (`params` is the hook envelope, built by the caller). No
    /// timeout here; the caller bounds the wait. A transport or JSON-RPC
    /// failure is `Err`. Default: this runtime cannot be asked.
    async fn hook_value(&self, params: Value) -> Result<Value, String> {
        let _ = params;
        Err("extension runtime does not support hook.handle".to_string())
    }

    /// Call an extension-provided tool on behalf of a conversation. The
    /// only way to call a tool: every caller says who the call is for.
    ///
    /// `session_id` is the owning conversation id (`None` = worker/subagent
    /// or no conversation); `cwd` is the folder the caller runs in (`None` =
    /// the process folder, which under the daemon is the daemon's, so a
    /// call made for a chat passes the chat's). Default: this runtime
    /// serves no tools.
    async fn call_tool_for(
        &self,
        name: &str,
        input: Value,
        session_id: Option<&str>,
        cwd: Option<&std::path::Path>,
    ) -> Result<Value, String> {
        let _ = (name, input, session_id, cwd);
        Err("extension runtime does not support tool.call".to_string())
    }

    /// [`Self::call_tool_for`], naming the run that calls: `caller` is the
    /// author of the calling run's memory binding (`None` = the call's
    /// context has no binding), sent as `tool.call` `params.caller` so an
    /// extension can tell a chat from each of its workers. What a registered
    /// extension tool calls. Default: [`Self::call_tool_for`], for a handler
    /// that has no wire to put it on.
    async fn call_tool_as(
        &self,
        name: &str,
        input: Value,
        session_id: Option<&str>,
        cwd: Option<&std::path::Path>,
        caller: Option<&agent_core::memory::author::Author>,
    ) -> Result<Value, String> {
        let _ = caller;
        self.call_tool_for(name, input, session_id, cwd).await
    }

    /// [`Self::call_tool_for`] for a call that belongs to no chat: on the
    /// wire `session_id` is `null` and `cwd` is the process folder.
    ///
    /// Tests only (the `testing` feature). Production has no such call:
    /// each one is made for a chat or a worker and says which folder. Should
    /// a genuine one appear, un-gate this and call it by name instead of
    /// passing `None, None`, so every no-chat call can be found by grep.
    #[cfg(any(test, feature = "testing"))]
    async fn call_tool_no_chat(&self, name: &str, input: Value) -> Result<Value, String> {
        self.call_tool_for(name, input, None, None).await
    }

    /// Complete a chat request through an extension-provided model provider.
    async fn provider_complete(
        &self,
        _params: ProviderCompleteParams,
    ) -> Result<ProviderCompleteResult, String> {
        Err("extension runtime does not support provider.complete".to_string())
    }

    /// Stream a chat request through an extension-provided model provider.
    ///
    /// The handler must forward `provider.stream.event` notifications to `sink`
    /// in order. The returned `ProviderCompleteResult` is the final aggregated
    /// response (so callers that don't need streaming can use it as the final
    /// state). Implementations that don't support streaming should return
    /// `Err("provider.stream is not supported by this extension")`.
    async fn provider_stream(
        &self,
        _params: ProviderCompleteParams,
        _sink: tokio::sync::mpsc::Sender<ProviderStreamEvent>,
    ) -> Result<ProviderCompleteResult, String> {
        Err("provider.stream is not supported by this extension".to_string())
    }

    /// Invoke a plugin-registered interactive slash command. The handler must
    /// forward `command.output` notifications matching `request_id` and any
    /// `task.*` notifications to `sink`. Returns the final response value.
    ///
    /// CP-11 fix-3: the sink is BOUNDED and metered
    /// ([`crate::extensions::invoke_output::InvokeEventSink`]). It must be
    /// paired with an EAGERLY CONCURRENT consumer — normally the collector
    /// half of [`crate::extensions::invoke_output::invoke_event_channel`],
    /// joined with this call by
    /// `ExtensionManager::invoke_command_collected` — which enforces
    /// invocation-local byte/event budgets at production time. A hostile
    /// `command.output` flood is therefore paced by awaited sends and
    /// bounded retention instead of parking aggregate bytes in an
    /// unbounded post-hoc queue (the pre-fix behavior). Handlers must
    /// treat a closed sink like the old closed-unbounded-sink case: stop
    /// forwarding, still complete the call.
    async fn invoke_command(
        &self,
        _command: &str,
        _args: Vec<String>,
        _request_id: &str,
        _sink: crate::extensions::invoke_output::InvokeEventSink,
    ) -> Result<Value, String> {
        Err("extension runtime does not support command.invoke".to_string())
    }

    /// Fetch optional plugin capability/build/model information.
    async fn get_info(&self) -> Result<PluginInfo, String> {
        Err("extension runtime does not support info.get".to_string())
    }

    /// Open a plugin-owned custom settings editor and return its initial render payload.
    async fn settings_editor_open(&self, _category: &str, _field: &str) -> Result<Value, String> {
        Err("extension runtime does not support settings.editor.open".to_string())
    }

    /// Forward a keypress to the active plugin-owned custom settings editor.
    async fn settings_editor_key(
        &self,
        _category: &str,
        _field: &str,
        _key: &str,
    ) -> Result<Value, String> {
        Err("extension runtime does not support settings.editor.key".to_string())
    }

    /// Ask the plugin to commit a custom editor value selected by the UI.
    async fn settings_editor_commit(
        &self,
        _category: &str,
        _field: &str,
        _value: Value,
    ) -> Result<Value, String> {
        Err("extension runtime does not support settings.editor.commit".to_string())
    }

    /// Gracefully shut down the extension.
    async fn shutdown(&self);

    /// Subscribe to raw JSON-RPC notifications from this extension.
    /// Returns a monotonic subscription id (for unsubscribing) and a
    /// receiver that yields every notification frame. The default
    /// implementation returns a channel whose sender is immediately
    /// dropped, so the receiver yields `None` right away — effectively
    /// a no-op for handler impls that don't support notifications.
    async fn subscribe_notifications(
        &self,
    ) -> (usize, tokio::sync::mpsc::Receiver<NotificationFrame>) {
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        (0, rx)
    }

    /// Current health state of this handler.
    async fn health(&self) -> ExtensionHealth {
        ExtensionHealth::Running
    }

    /// Number of transport restarts observed by this handler.
    async fn restart_count(&self) -> usize {
        0
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    struct UnsupportedHandler;

    #[async_trait]
    impl ExtensionHandler for UnsupportedHandler {
        fn id(&self) -> &str {
            "unsupported-lifecycle"
        }

        async fn handle(&self, _event: &HookEvent) -> HookResult {
            unreachable!("snapshot must not dispatch a hook")
        }

        async fn shutdown(&self) {}
    }

    #[test]
    fn default_lifecycle_snapshot_is_unavailable_through_trait_object() {
        let handler: &dyn ExtensionHandler = &UnsupportedHandler;
        assert_eq!(handler.lifecycle_snapshot(), None);
    }

    /// A runtime that implements no tool call serves none, whoever the call
    /// is for.
    #[tokio::test]
    async fn default_call_tool_for_serves_no_tool() {
        let handler: &dyn ExtensionHandler = &UnsupportedHandler;
        let chat = std::env::temp_dir().join("chat-folder");
        for (session_id, cwd) in [(Some("chat-A"), Some(chat.as_path())), (None, None)] {
            let err = handler
                .call_tool_for("any", serde_json::json!({}), session_id, cwd)
                .await
                .unwrap_err();
            assert!(err.contains("does not support tool.call"), "{err}");
        }
    }

    /// Who each tool call said it was for.
    #[derive(Default)]
    struct Callers(std::sync::Mutex<Vec<(Option<String>, Option<std::path::PathBuf>)>>);

    #[async_trait]
    impl ExtensionHandler for Callers {
        fn id(&self) -> &str {
            "callers"
        }
        async fn handle(&self, _event: &HookEvent) -> HookResult {
            HookResult::default()
        }
        async fn shutdown(&self) {}
        async fn call_tool_for(
            &self,
            _name: &str,
            _input: Value,
            session_id: Option<&str>,
            cwd: Option<&std::path::Path>,
        ) -> Result<Value, String> {
            self.0.lock().unwrap().push((
                session_id.map(str::to_string),
                cwd.map(std::path::Path::to_path_buf),
            ));
            Ok(Value::Null)
        }
    }

    /// The one named way to call for no chat sends no conversation and no
    /// folder; every other call carries what its caller passed.
    #[tokio::test]
    async fn only_the_named_no_chat_call_omits_the_caller() {
        let handler = Callers::default();
        let chat = std::env::temp_dir().join("chat-folder");
        handler
            .call_tool_for("t", serde_json::json!({}), Some("chat-A"), Some(&chat))
            .await
            .unwrap();
        handler
            .call_tool_no_chat("t", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            *handler.0.lock().unwrap(),
            vec![(Some("chat-A".to_string()), Some(chat)), (None, None)]
        );
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Minimal handler for unit tests. Reports generation 0, Running health.
    pub struct DummyHandler;

    #[async_trait]
    impl ExtensionHandler for DummyHandler {
        fn id(&self) -> &str {
            "dummy"
        }

        fn lifecycle_snapshot(&self) -> Option<ExtensionLifecycle> {
            Some(ExtensionLifecycle {
                health: ExtensionHealth::Running,
                generation: 0,
            })
        }

        async fn handle(&self, _event: &HookEvent) -> HookResult {
            HookResult::default()
        }

        async fn shutdown(&self) {}
    }
}
