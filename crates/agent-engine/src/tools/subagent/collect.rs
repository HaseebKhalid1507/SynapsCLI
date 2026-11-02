//! SubagentCollectTool — check if a reactive subagent is done and return its result.
//!
//! Non-blocking — checks the registry once and returns immediately.
//! If the subagent is still running, returns status + partial output.
//! If done, returns the full result. The natural pair to `subagent_start` —
//! start async, check when you want the answer.
//!

use super::super::{Tool, ToolContext};
use crate::runtime::subagent::SubagentStatus;
use crate::{Result, RuntimeError};
use serde_json::{json, Value};

pub struct SubagentCollectTool;

/// A finished worker's result as its terminal record holds it.
#[derive(Debug)]
pub(super) struct Archived {
    /// The worker's agent name, from the record's own metadata (a v1
    /// archive's `meta` or a v2 final's `meta`); `subagent_status` shows it.
    pub agent_name: String,
    pub status: String,
    /// The human failure reason a v1 archive keeps; a v2 final has none.
    pub error: Option<String>,
    pub output: String,
    pub model: String,
    /// A v2 final's window counters (§8); `None` for a v1 archive.
    pub windows: Option<crate::runtime::subagent::WindowCounters>,
    /// A v1 archive's `context` object (upstream #332: where a worker's
    /// host-side rollover left its earlier windows); `None` for a worker
    /// that never rolled over that way, and for a v2 final (`windows`).
    pub context: Option<Value>,
}

/// The result kept in a reaped worker's terminal `record` (below `root`):
/// a v1 archive read as it always was, or, only for the run the handle
/// recorded as its publisher, that run's v2 final under the record-file
/// rules (no links, private, bounded), never through the v1 reader. Blocking
/// I/O: callers run it on a blocking worker. `Err` says why nothing was read
/// (`subagent_status` shows it; `subagent_collect` only needs that it failed).
pub(super) fn archived_result(
    record: &crate::runtime::subagent::ArchivedRecord,
    root: &std::path::Path,
) -> std::result::Result<Archived, String> {
    match &record.v2_final_run {
        None => super::archive::read_archive(&record.path)
            .map(|archive| Archived {
                context: crate::runtime::subagent::archived_context_summary(&archive.history),
                agent_name: archive.meta.agent_name,
                status: archive.status,
                error: archive.error,
                output: archive.output,
                model: archive.meta.model,
                windows: None,
            })
            .map_err(|e| e.to_string()),
        Some(run) => v2_final_result(&record.path, root, run),
    }
}

/// The v2 final of run `run` at `path`, checked like any final before it
/// is believed: located below `root`, its own handle's, that run's, valid.
fn v2_final_result(
    path: &std::path::Path,
    root: &std::path::Path,
    run: &str,
) -> std::result::Result<Archived, String> {
    use super::record::{Candidate, FinalRecord, RecordKind};
    let raw = path.to_str().ok_or("its path is not UTF-8")?;
    let location = super::record_file::locate(raw, root).map_err(|e| e.to_string())?;
    if location.kind() != RecordKind::Final {
        return Err("it is not a final record".into());
    }
    let record = match super::record_file::load(&location)
        .map_err(|e| e.to_string())?
        .final_record
    {
        Candidate::Present(FinalRecord::V2(record)) => record,
        Candidate::Present(FinalRecord::V1(_)) => {
            return Err("it is a v1 archive, not the v2 final of the run recorded".into())
        }
        Candidate::Absent => return Err("there is no final record".into()),
        Candidate::Unreadable(e) => return Err(e),
    };
    if record.handle_id != location.handle() {
        return Err("it is another handle's final".into());
    }
    if record.run_id.as_str() != run {
        return Err("it is the final of another run than the one recorded".into());
    }
    record
        .validate(RecordKind::Final)
        .map_err(|e| e.to_string())?;
    Ok(Archived {
        windows: Some(super::persist::window_counters(&record)),
        agent_name: record.meta.agent_name,
        status: record.status,
        error: None,
        output: record.output,
        model: record.meta.model,
        context: None,
    })
}

/// Test seam: a gate the blocking read of a reaped handle's record waits at.
#[cfg(test)]
pub(super) mod read_gate {
    use std::sync::mpsc::{Receiver, Sender};
    use std::sync::Mutex;
    use std::time::Duration;

    type Gate = (String, Option<tokio::sync::oneshot::Sender<()>>, Receiver<()>);
    static GATES: Mutex<Vec<Gate>> = Mutex::new(Vec::new());

    /// Gate the next read of `handle`'s record: `entered` fires once the read
    /// has started; it then waits until `open` is sent to (at most 5 s, then
    /// it panics: nothing opened it while it waited).
    pub fn gate(handle: &str) -> (tokio::sync::oneshot::Receiver<()>, Sender<()>) {
        let (entered_tx, entered) = tokio::sync::oneshot::channel();
        let (open, open_rx) = std::sync::mpsc::channel();
        GATES
            .lock()
            .unwrap()
            .push((handle.to_owned(), Some(entered_tx), open_rx));
        (entered, open)
    }

    pub(in super::super) fn wait(handle: &str) {
        let gate = {
            let mut gates = GATES.lock().unwrap_or_else(|p| p.into_inner());
            let at = gates.iter().position(|g| g.0 == handle);
            at.map(|i| gates.remove(i))
        };
        if let Some((_, entered, open)) = gate {
            if let Some(entered) = entered {
                let _ = entered.send(());
            }
            open.recv_timeout(Duration::from_secs(5))
                .expect("the gated read was not opened while it waited");
        }
    }
}

#[async_trait::async_trait]
impl Tool for SubagentCollectTool {
    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Builtin
    }

    fn name(&self) -> &str {
        "subagent_collect"
    }

    fn description(&self) -> &str {
        "Retrieve a reactive subagent's result. Non-blocking — returns immediately \
         (status + partial output if still running). Standard flow: dispatch with \
         subagent_start, carry on (or end your turn) — the subagent's completion event \
         wakes you with a terminal status (completed/failed/timed_out/cancelled); then \
         call this ONCE with reconciled=true — that single call both collects the result and attests \
         reconciliation, clearing the completion gate. Calling without reconciled=true \
         is an inspection-only read: it leaves the completion gate blocked and forces \
         a second call. reconciled=true is idempotent and safe on repeats."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "handle_id": {
                    "type": "string",
                    "description": "Handle ID returned by subagent_start (e.g. \"sa_3\")."
                },
                "reconciled": {
                    "type": "boolean",
                    "default": false,
                    "description": "Set true on your terminal collect (after subagent_status reports done) to attest the result is reconciled with foreground work and unblock completion — one call collects AND reconciles. Leave false only for a deliberate inspection-only read; a later collect with reconciled=true still reconciles (idempotent)."
                }
            },
            "required": ["handle_id"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        self.execute_in(params, ctx, super::archive::archive_root())
            .await
    }
}

impl SubagentCollectTool {
    /// `subagent_collect` of a handle that was reaped: its result survives in
    /// its `archived_record` (a resume archive, or a v2 run's final), if any.
    async fn collect_reaped(
        handle_id: String,
        archived_record: Option<crate::runtime::subagent::ArchivedRecord>,
        params: &Value,
        ctx: &ToolContext,
        root: std::path::PathBuf,
    ) -> Result<String> {
        // Its lookup and read run on a blocking worker, never on the
        // executor.
        let archive_path = archived_record.as_ref().map(|r| r.path.clone());
        let archived = match archived_record {
            Some(record) => {
                #[cfg(test)]
                let gated = handle_id.clone();
                tokio::task::spawn_blocking(move || {
                    #[cfg(test)]
                    read_gate::wait(&gated);
                    archived_result(&record, &root)
                })
                .await
                .ok()
                .and_then(|read| read.ok())
            }
            None => None,
        };
        if let Some(orchestration) = &ctx.capabilities.orchestration {
            if orchestration.is_unreconciled(&handle_id) {
                orchestration
                    .terminal_and_collect(
                        &handle_id,
                        agent_core::orchestration::WorkerTerminal::Failed,
                    )
                    .map_err(RuntimeError::Tool)?;
                if params["reconciled"].as_bool().unwrap_or(false) {
                    orchestration
                        .reconcile(&handle_id)
                        .map_err(RuntimeError::Tool)?;
                }
                if archived.is_none() {
                    return Ok(json!({
                        "handle_id": handle_id,
                        "status": "expired",
                        "note": "Subagent output expired; orchestration lifecycle remains recoverable.",
                        "collected": false
                    })
                    .to_string());
                }
            }
        }
        if let (Some(archive), Some(path)) = (archived, archive_path) {
            let mut body = json!({
                "handle_id": handle_id,
                "status": archive.status,
                "error": archive.error,
                "output": archive.output,
                "model": archive.model,
                "collected": true,
                "archive_path": path,
                "note": "Handle was reaped; result read from its resume archive. \
                         subagent_resume can still continue this worker."
            });
            // A v2 final states its counters; its notices were not kept.
            if let Some(windows) = archive.windows {
                body["rolloverWindows"] = json!(windows);
            }
            // Upstream #332: a v1 archive of a worker whose context rolled
            // over host-side still says where its earlier windows are.
            if let Some(context) = archive.context {
                body["context"] = context;
            }
            return Ok(body.to_string());
        }
        Err(RuntimeError::Tool(format!(
            "No subagent found with handle_id '{}'. Finished handles are retained for {} minutes after completion and then garbage-collected.",
            handle_id,
            crate::runtime::subagent::FINISHED_HANDLE_TTL.as_secs() / 60
        )))
    }

    /// [`Tool::execute`] with reaped workers' records below `root`.
    pub(super) async fn execute_in(
        &self,
        params: Value,
        ctx: ToolContext,
        root: std::path::PathBuf,
    ) -> Result<String> {
        // A worker the daemon resumed after a reload: its id collects the
        // run that continues it, never the stopped run's partial result.
        let resumed = match (params["handle_id"].as_str(), &ctx.capabilities.subagent_registry) {
            (Some(id), Some(registry)) => registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .resumed_as(id)
                .map(|new| (id.to_string(), new)),
            _ => None,
        };
        match resumed {
            None => self.collect_handle(params, ctx, root).await,
            Some((old, None)) => Ok(json!({
                "handle_id": old,
                "status": "resuming",
                "note": "This worker was stopped by a daemon reload and is being resumed by the \
                         daemon; its new handle is announced shortly.",
                "collected": false
            })
            .to_string()),
            Some((old, Some(new))) => {
                let mut params = params;
                params["handle_id"] = json!(new);
                let reply = self.collect_handle(params, ctx, root).await?;
                Ok(match serde_json::from_str::<Value>(&reply) {
                    Ok(mut body) if body.is_object() => {
                        body["requested_handle_id"] = json!(old);
                        body["resumed_as"] = json!(new);
                        body["alias_note"] = json!(format!(
                            "{old} was stopped by a daemon reload and resumed by the daemon as {new}: \
                             this is {new}'s result."
                        ));
                        body.to_string()
                    }
                    _ => reply,
                })
            }
        }
    }

    /// [`Self::execute_in`] for the id asked for.
    async fn collect_handle(
        &self,
        params: Value,
        ctx: ToolContext,
        root: std::path::PathBuf,
    ) -> Result<String> {
        let handle_id = params["handle_id"]
            .as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'handle_id' parameter".to_string()))?
            .to_string();

        let registry = ctx.capabilities.subagent_registry.as_ref().ok_or_else(|| {
            RuntimeError::Tool("SubagentRegistry not available on this ToolContext".to_string())
        })?;

        // A finished worker whose v1 archive write failed is retried first:
        // the request is taken under the guards, written on a blocking worker
        // with no guard held, and applied there (`archive::ArchiveRetry`).
        let retry = {
            let reg = registry.lock().unwrap();
            reg.get(&handle_id).and_then(|handle| {
                super::archive::ArchiveRetry::take(&handle.state, &handle_id)
                    .map(|retry| (retry, std::sync::Arc::clone(&handle.state)))
            })
        };
        if let Some((retry, state)) = retry {
            retry.run(state).await;
        }

        // The registry guard lives only in this block: nothing below it holds
        // the guard across an await.
        let looked_up = {
            let mut reg = registry.lock().unwrap();
            let archived_record = reg.archived_record(&handle_id);
            match reg.get_mut(&handle_id) {
                None => Err(archived_record),
                Some(handle) => {
                    let status = handle.status();
                    // Mark collected on any terminal read so the reaper knows
                    // it's safe to GC.
                    let already_collected = handle.is_collected();
                    if status != SubagentStatus::Running {
                        handle.mark_collected();
                    }
                    Ok((
                        status,
                        handle.partial_output(),
                        handle.elapsed_secs(),
                        handle.model.clone(),
                        handle.terminal_diagnostic(),
                        handle.authorization.clone(),
                        handle.archive_error(),
                        handle.rollover_notice(),
                        handle.visibility(),
                        handle.evicted_windows(),
                        handle
                            .state
                            .read()
                            .unwrap_or_else(|p| p.into_inner())
                            .context_summary(),
                        already_collected,
                        archived_record.map(|r| r.path),
                    ))
                }
            }
        };
        let (
            status,
            output,
            elapsed,
            model,
            terminal,
            authorization,
            archive_error,
            rollover_notice,
            visibility,
            evicted_windows,
            context,
            already_collected,
            archive_path,
        ) = match looked_up {
            Ok(live) => live,
            Err(archived_record) => {
                return Self::collect_reaped(handle_id, archived_record, &params, &ctx, root).await
            }
        };

        if status == SubagentStatus::Running {
            // Still going — return current state, don't block
            let char_count = output.chars().count();
            let output_so_far: String = if char_count > 500 {
                output.chars().skip(char_count - 500).collect()
            } else {
                output
            };
            let mut running = json!({
                "handle_id":    handle_id,
                "status":       "running",
                "elapsed_secs": (elapsed * 10.0).round() / 10.0,
                "output_so_far": output_so_far
            });
            if let Some(notice) = rollover_notice {
                running["rolloverNotice"] = json!(notice);
            }
            if let Some(visibility) = &visibility {
                visibility.add_to_tool_body(&mut running);
            }
            if evicted_windows > 0 {
                running["evictedWindows"] = json!(evicted_windows);
            }
            return Ok(running.to_string());
        }

        // Lifecycle (intentional caller attestation — never auto-reconcile):
        // - First terminal collect always drives terminal_and_collect.
        // - Any terminal collect with reconciled=true must reconcile, including
        //   a repeated collect after an earlier unreconciled read. Gating
        //   reconcile on !already_collected left workers stuck in Collected
        //   forever and blocked completion.
        // - terminal_and_collect stays first-collect-only so diagnostic
        //   re-reads remain read-many idempotent at the handle layer.
        if let Some(orchestration) = &ctx.capabilities.orchestration {
            if !already_collected {
                let terminal = match status {
                    SubagentStatus::Completed => {
                        agent_core::orchestration::WorkerTerminal::Completed
                    }
                    SubagentStatus::TimedOut => agent_core::orchestration::WorkerTerminal::TimedOut,
                    _ => agent_core::orchestration::WorkerTerminal::Failed,
                };
                orchestration
                    .terminal_and_collect(&handle_id, terminal)
                    .map_err(RuntimeError::Tool)?;
            }
            if params["reconciled"].as_bool().unwrap_or(false) {
                orchestration
                    .reconcile(&handle_id)
                    .map_err(RuntimeError::Tool)?;
            }
        }

        // Done — return full result. The registry retains this record, making
        // repeated collection diagnostically idempotent; `collected` signals
        // idempotency to the caller.
        let reconciled_now = params["reconciled"].as_bool().unwrap_or(false);
        let mut body = json!({
            "archiveError": archive_error,
            "handle_id": handle_id,
            "status":    status.as_str(),
            "output":    output,
            "model":     model,
            "terminal_cause": terminal,
            "authorization": authorization,
            "collected": already_collected,
        });
        // Present only once the worker's context has rolled over
        // (upstream's host-side rollover, #332).
        if let Some(context) = context {
            body["context"] = context;
        }
        if let Some(notice) = rollover_notice {
            body["rolloverNotice"] = json!(notice);
        }
        if let Some(visibility) = &visibility {
            visibility.add_to_tool_body(&mut body);
        }
        if evicted_windows > 0 {
            body["evictedWindows"] = json!(evicted_windows);
        }
        if !reconciled_now && ctx.capabilities.orchestration.is_some() {
            // Nudge stragglers: an unreconciled terminal read leaves the
            // completion gate blocked and forces a second collect. The
            // standard flow (doctrine + tool description) is a single
            // collect with reconciled=true after status turns terminal.
            body["note"] = json!(
                "UNRECONCILED read — the completion gate is still blocked. \
                 Call subagent_collect again with reconciled=true after \
                 integrating this result."
            );
        }
        if let Some(reason) = status.failure_reason() {
            body["error"] = json!(reason);
        }
        if let Some(path) = archive_path {
            // Full conversation kept: subagent_resume continues it.
            body["archive_path"] = json!(path);
        }
        Ok(body.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::subagent::{
        SubagentHandle, SubagentRegistry, SubagentState, SubagentStatus,
    };
    use crate::tools::test_helpers::create_tool_context;
    use crate::tools::Tool;
    use serde_json::json;
    use std::sync::{Arc, Mutex, RwLock};
    use tokio::sync::{mpsc, oneshot};

    fn make_finished_handle(id: &str, output: &str) -> SubagentHandle {
        let state = Arc::new(RwLock::new(SubagentState::new()));
        {
            let mut s = state.write().unwrap();
            s.status = SubagentStatus::Completed;
            s.partial_text = output.to_string();
            s.finished_at = Some(std::time::Instant::now());
        }
        let (steer_tx, _steer_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, _shutdown_rx) = oneshot::channel();
        let (_result_tx, result_rx) = oneshot::channel();
        SubagentHandle::new(
            id.to_string(),
            0,
            "test-agent".to_string(),
            "test task".to_string(),
            "claude-sonnet-4-6".to_string(),
            "system prompt".to_string(),
            300,
            state,
            Some(steer_tx),
            Some(shutdown_tx),
            Some(result_rx),
        )
    }

    #[tokio::test]
    async fn managed_terminal_waits_for_finalization_before_collection() {
        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        let h = make_finished_handle("sa_pending", "still draining");
        let state = Arc::clone(&h.state);
        {
            let mut s = state.write().unwrap();
            s.finished_at = None;
            s.archive_meta = Some(crate::runtime::subagent::SubagentArchiveMeta {
                agent_name: "worker".into(),
                model: "model".into(),
                system_prompt: "system".into(),
                timeout_secs: 1,
                cwd: None,
                thinking: None,
                watch_usage: None,
            });
        }
        registry.lock().unwrap().register(h);
        let response = SubagentCollectTool
            .execute(
                json!({"handle_id":"sa_pending", "reconciled":true}),
                make_ctx_with_registry(Arc::clone(&registry)),
            )
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap()["status"],
            "running"
        );
        let mut reg = registry.lock().unwrap();
        assert!(!reg.get("sa_pending").unwrap().is_collected());
        assert_eq!(reg.display_rows()[0].status, SubagentStatus::Running);
        reg.release_finished_resources("sa_pending");
        reg.cleanup_finished_with_ttl(std::time::Duration::ZERO);
        assert!(reg.get("sa_pending").is_some());
        assert!(state.read().unwrap().finished_at.is_none());
    }

    fn make_ctx_with_registry(registry: Arc<Mutex<SubagentRegistry>>) -> crate::tools::ToolContext {
        let mut ctx = create_tool_context();
        ctx.capabilities.subagent_registry = Some(registry);
        // This suite exercises the registry-level collected/idempotency
        // contract in isolation. The handle is registered directly (never
        // dispatched through orchestration), so the policy registry must not
        // participate — dispatch-path enforcement is covered by the
        // orchestration and lifecycle suites.
        ctx.capabilities.orchestration = None;
        ctx
    }

    #[tokio::test]
    async fn collect_after_reap_reads_the_archive() {
        let dir = tempfile::tempdir().unwrap();
        let meta = crate::runtime::subagent::SubagentArchiveMeta {
            agent_name: "test-agent".into(),
            model: "anthropic/claude-opus-5-5".into(),
            system_prompt: "s".into(),
            timeout_secs: 60,
            cwd: None,
            thinking: None,
            watch_usage: None,
        };
        let path = super::super::archive::write_archive(
            dir.path(),
            "sa_gone",
            &meta,
            "timed_out",
            Some("worker timed out"),
            "[TIMED OUT after 60s] latest progress",
            &[Arc::new(json!({"role": "user", "content": "task"}))],
        )
        .unwrap();
        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        {
            let mut reg = registry.lock().unwrap();
            let state = Arc::new(RwLock::new(SubagentState::new()));
            {
                let mut st = state.write().unwrap();
                st.status = SubagentStatus::TimedOut;
                st.finished_at = Some(std::time::Instant::now());
                st.archive_path = Some(path.clone());
            }
            let (steer_tx, _steer_rx) = mpsc::unbounded_channel();
            let (shutdown_tx, _shutdown_rx) = oneshot::channel();
            let (_result_tx, result_rx) = oneshot::channel();
            let mut h = SubagentHandle::new(
                "sa_gone".into(),
                0,
                "test-agent".into(),
                "task".into(),
                "anthropic/claude-opus-5-5".into(),
                "s".into(),
                60,
                state,
                Some(steer_tx),
                Some(shutdown_tx),
                Some(result_rx),
            );
            h.mark_collected();
            reg.register(h);
            reg.cleanup_finished_with_ttl(std::time::Duration::ZERO);
            assert!(reg.get("sa_gone").is_none());
        }
        let body: serde_json::Value = serde_json::from_str(
            &SubagentCollectTool
                .execute(
                    json!({"handle_id": "sa_gone"}),
                    make_ctx_with_registry(registry),
                )
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["status"], "timed_out");
        assert_eq!(body["error"], "worker timed out");
        assert_eq!(body["output"], "[TIMED OUT after 60s] latest progress");
        assert_eq!(body["archive_path"], json!(path));
    }

    /// A worker the daemon resumed after a reload: collecting its old id
    /// reads the run that continues it, never the stopped run's partial
    /// result, and says so; while its launch is under way, "resuming"
    /// (mutant: drop the alias in `execute_in`).
    #[tokio::test]
    async fn collect_of_a_daemon_resumed_worker_reads_its_new_run() {
        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        {
            let mut reg = registry.lock().unwrap();
            reg.register(make_finished_handle("sa_81", "the resumed run's answer"));
            reg.begin_daemon_resume("sa_80");
            reg.finish_daemon_resume("sa_80", Some("sa_81"));
            reg.begin_daemon_resume("sa_82");
        }
        let body: serde_json::Value = serde_json::from_str(
            &SubagentCollectTool
                .execute(json!({"handle_id": "sa_80"}), make_ctx_with_registry(registry.clone()))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["handle_id"], "sa_81", "{body}");
        assert_eq!(body["requested_handle_id"], "sa_80");
        assert_eq!(body["resumed_as"], "sa_81");
        assert_eq!(body["output"], "the resumed run's answer");
        assert!(registry.lock().unwrap().get("sa_81").unwrap().is_collected());

        let body: serde_json::Value = serde_json::from_str(
            &SubagentCollectTool
                .execute(json!({"handle_id": "sa_82"}), make_ctx_with_registry(registry.clone()))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["status"], "resuming", "{body}");
        assert_eq!(body["collected"], false);
    }

    // U5: collect marks collected on first read; second read shows collected=true
    #[tokio::test]
    async fn collect_marks_collected_and_is_idempotent() {
        let tool = SubagentCollectTool;

        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        let handle = make_finished_handle("sa_42", "result text");
        registry.lock().unwrap().register(handle);

        // First collect — should return collected=false (not yet collected before this call)
        let result1 = tool
            .execute(
                json!({"handle_id": "sa_42"}),
                make_ctx_with_registry(registry.clone()),
            )
            .await
            .unwrap();
        let body1: serde_json::Value = serde_json::from_str(&result1).unwrap();
        assert_eq!(body1["status"], "completed");
        assert_eq!(body1["output"], "result text");
        assert_eq!(
            body1["collected"], false,
            "first collect should report collected=false"
        );

        // Second collect — same handle still in registry (not yet reaped), collected=true
        let result2 = tool
            .execute(
                json!({"handle_id": "sa_42"}),
                make_ctx_with_registry(registry.clone()),
            )
            .await
            .unwrap();
        let body2: serde_json::Value = serde_json::from_str(&result2).unwrap();
        assert_eq!(body2["status"], "completed");
        assert_eq!(
            body2["collected"], true,
            "second collect should report collected=true"
        );

        // After collect, the registry should have the handle marked collected
        let reg = registry.lock().unwrap();
        let h = reg.get("sa_42").unwrap();
        assert!(
            h.is_collected(),
            "handle must be marked collected in registry"
        );
    }

    /// Dispatch a finished handle through orchestration so collect can drive
    /// the terminal → collected → reconciled lifecycle. Returns (registry, orch).
    fn make_orch_ctx(
        handle_id: &str,
        output: &str,
    ) -> (
        Arc<Mutex<SubagentRegistry>>,
        Arc<crate::orchestration::OrchestrationRuntime>,
        crate::tools::ToolContext,
    ) {
        use crate::orchestration::OrchestrationRuntime;
        use agent_core::orchestration::CompletionGate;

        let foreground = agent_core::prompt::QualifiedModelId::parse("anthropic/claude-sonnet-4-6")
            .expect("test foreground is qualified");
        let orch =
            Arc::new(OrchestrationRuntime::baseline(foreground, 8, 64).expect("baseline runtime"));
        orch.authorize(handle_id, "anthropic/claude-sonnet-4-6")
            .expect("authorize worker");
        // authorize() leaves the worker Running. Transition to Terminal+Collected
        // so the gate blocks (it only catches finished-but-unreconciled workers).
        orch.terminal_and_collect(
            handle_id,
            agent_core::orchestration::WorkerTerminal::Completed,
        )
        .expect("mark terminal + collected");
        assert!(
            matches!(orch.completion_gate(), CompletionGate::Blocked { .. }),
            "terminal worker must block completion until reconciled"
        );

        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        registry
            .lock()
            .unwrap()
            .register(make_finished_handle(handle_id, output));

        let mut ctx = create_tool_context();
        ctx.capabilities.subagent_registry = Some(registry.clone());
        ctx.capabilities.orchestration = Some(orch.clone());
        (registry, orch, ctx)
    }

    // (a) first collect without reconciled leaves gate blocked; second collect
    // with reconciled=true advances Collected → Reconciled and allows completion.
    #[tokio::test]
    async fn deferred_reconcile_on_repeat_collect_unblocks_completion() {
        use agent_core::orchestration::CompletionGate;

        let tool = SubagentCollectTool;
        let (registry, orch, ctx) = make_orch_ctx("sa_deferred", "result text");

        let result1 = tool
            .execute(
                json!({"handle_id": "sa_deferred", "reconciled": false}),
                ctx,
            )
            .await
            .unwrap();
        let body1: serde_json::Value = serde_json::from_str(&result1).unwrap();
        assert_eq!(body1["collected"], false);
        match orch.completion_gate() {
            CompletionGate::Blocked { workers } => {
                assert_eq!(workers, vec!["sa_deferred".to_string()]);
                assert!(
                    workers.iter().all(|id| id.starts_with("sa_")),
                    "blocked IDs must be subagent_collect handles, got {workers:?}"
                );
                assert!(
                    workers.iter().all(|id| !id.starts_with("worker-")),
                    "blocked IDs must never leak policy WorkerHandle, got {workers:?}"
                );
            }
            other => panic!(
                "first collect without reconcile must leave completion blocked, got {other:?}"
            ),
        }

        // Rebuild ctx so the same shared orch/registry participate on the
        // second call (create_tool_context would allocate a fresh baseline).
        let mut ctx2 = create_tool_context();
        ctx2.capabilities.subagent_registry = Some(registry.clone());
        ctx2.capabilities.orchestration = Some(orch.clone());

        let result2 = tool
            .execute(
                json!({"handle_id": "sa_deferred", "reconciled": true}),
                ctx2,
            )
            .await
            .unwrap();
        let body2: serde_json::Value = serde_json::from_str(&result2).unwrap();
        assert_eq!(body2["collected"], true);
        assert_eq!(
            orch.completion_gate(),
            CompletionGate::Allowed,
            "repeat collect with reconciled=true must unblock completion"
        );
    }

    // (b) first collect with reconciled=true allows completion immediately.
    #[tokio::test]
    async fn first_collect_with_reconcile_allows_completion() {
        use agent_core::orchestration::CompletionGate;

        let tool = SubagentCollectTool;
        let (_registry, orch, ctx) = make_orch_ctx("sa_first_true", "result text");

        let result = tool
            .execute(
                json!({"handle_id": "sa_first_true", "reconciled": true}),
                ctx,
            )
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(body["collected"], false);
        assert_eq!(
            orch.completion_gate(),
            CompletionGate::Allowed,
            "first collect with reconciled=true must allow completion"
        );
    }

    /// An unreconciled terminal read leaves the gate blocked, so the response
    /// must carry an explicit nudge to re-collect with reconciled=true —
    /// otherwise models stall on a gate they can no longer see. The standard
    /// single-call flow (reconciled=true up front) must NOT carry the note.
    #[tokio::test]
    async fn unreconciled_terminal_collect_carries_reconcile_nudge() {
        let tool = SubagentCollectTool;
        let (_registry, _orch, ctx) = make_orch_ctx("sa_nudge", "result text");

        let result = tool
            .execute(json!({"handle_id": "sa_nudge", "reconciled": false}), ctx)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&result).unwrap();
        let note = body["note"].as_str().expect("unreconciled read must nudge");
        assert!(note.contains("reconciled=true"), "{note}");
    }

    #[tokio::test]
    async fn reconciled_collect_carries_no_nudge() {
        let tool = SubagentCollectTool;
        let (_registry, _orch, ctx) = make_orch_ctx("sa_no_nudge", "result text");

        let result = tool
            .execute(json!({"handle_id": "sa_no_nudge", "reconciled": true}), ctx)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(
            body.get("note").is_none(),
            "single-call collect+reconcile must not carry a nudge: {body}"
        );
    }

    #[tokio::test]
    async fn forced_expiry_remains_collectible_and_clears_gate() {
        use agent_core::orchestration::CompletionGate;

        let tool = SubagentCollectTool;
        let (registry, orch, ctx) = make_orch_ctx("sa_expired", &"result".repeat(10_000));
        crate::runtime::subagent::reap_finished_with_ttl(
            &registry,
            Some(orch.as_ref()),
            std::time::Duration::ZERO,
        );
        assert!(registry
            .lock()
            .unwrap()
            .get("sa_expired")
            .unwrap()
            .is_tombstone());

        let result = tool
            .execute(json!({"handle_id": "sa_expired", "reconciled": true}), ctx)
            .await
            .unwrap();
        assert_ne!(
            serde_json::from_str::<Value>(&result).unwrap()["status"],
            "expired"
        );
        assert_eq!(orch.completion_gate(), CompletionGate::Allowed);
    }

    #[tokio::test]
    async fn collected_handle_reaped_then_degraded_reconcile_clears_gate() {
        use agent_core::orchestration::CompletionGate;

        let tool = SubagentCollectTool;
        let (registry, orch, ctx) = make_orch_ctx("sa_collected_expired", "result text");
        tool.execute(
            json!({"handle_id": "sa_collected_expired", "reconciled": false}),
            ctx,
        )
        .await
        .unwrap();
        assert!(matches!(
            orch.completion_gate(),
            CompletionGate::Blocked { .. }
        ));

        crate::runtime::subagent::reap_finished_with_ttl(
            &registry,
            Some(orch.as_ref()),
            std::time::Duration::ZERO,
        );
        assert!(registry
            .lock()
            .unwrap()
            .get("sa_collected_expired")
            .is_none());

        let mut ctx2 = create_tool_context();
        ctx2.capabilities.subagent_registry = Some(registry);
        ctx2.capabilities.orchestration = Some(orch.clone());
        let response = tool
            .execute(
                json!({"handle_id": "sa_collected_expired", "reconciled": true}),
                ctx2,
            )
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap()["status"],
            "expired"
        );
        assert_eq!(orch.completion_gate(), CompletionGate::Allowed);
    }

    #[tokio::test]
    async fn missing_mapped_handle_has_degraded_collect_not_404() {
        use agent_core::orchestration::CompletionGate;

        let tool = SubagentCollectTool;
        let (registry, orch, ctx) = make_orch_ctx("sa_missing", "lost");
        registry.lock().unwrap().remove("sa_missing");
        let result = tool
            .execute(json!({"handle_id": "sa_missing", "reconciled": true}), ctx)
            .await
            .unwrap();
        let body: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(body["status"], "expired");
        assert_eq!(orch.completion_gate(), CompletionGate::Allowed);
    }

    // (c) repeated collect with reconciled=true remains read-many idempotent.
    #[tokio::test]
    async fn repeated_reconcile_true_is_idempotent() {
        use agent_core::orchestration::CompletionGate;

        let tool = SubagentCollectTool;
        let (registry, orch, ctx) = make_orch_ctx("sa_repeat_true", "result text");

        let result1 = tool
            .execute(
                json!({"handle_id": "sa_repeat_true", "reconciled": true}),
                ctx,
            )
            .await
            .unwrap();
        let body1: serde_json::Value = serde_json::from_str(&result1).unwrap();
        assert_eq!(body1["collected"], false);
        assert_eq!(orch.completion_gate(), CompletionGate::Allowed);

        let mut ctx2 = create_tool_context();
        ctx2.capabilities.subagent_registry = Some(registry.clone());
        ctx2.capabilities.orchestration = Some(orch.clone());

        let result2 = tool
            .execute(
                json!({"handle_id": "sa_repeat_true", "reconciled": true}),
                ctx2,
            )
            .await
            .unwrap();
        let body2: serde_json::Value = serde_json::from_str(&result2).unwrap();
        assert_eq!(body2["collected"], true);
        assert_eq!(
            orch.completion_gate(),
            CompletionGate::Allowed,
            "repeat reconciled collect must stay Allowed (idempotent)"
        );
        // Registry handle remains readable and marked collected.
        let reg = registry.lock().unwrap();
        assert!(reg.get("sa_repeat_true").unwrap().is_collected());
    }

    // ── The retry of a failed archive write (off the executor) ────────────

    /// A finished v1 worker whose archive write failed, its archive
    /// directory `dir` since repaired: history, metadata and the error kept.
    fn failed_archive_handle(id: &str, dir: &std::path::Path) -> SubagentHandle {
        let h = make_finished_handle(id, "out");
        {
            let mut s = h.state.write().unwrap();
            s.finalization_ready = true;
            s.archive_meta = Some(crate::runtime::subagent::SubagentArchiveMeta {
                agent_name: "worker".into(),
                model: "model".into(),
                system_prompt: "system".into(),
                timeout_secs: 1,
                cwd: None,
                thinking: None,
                watch_usage: None,
            });
            s.archive_dir = Some(dir.to_path_buf());
            s.history = Some(vec![Arc::new(json!({"role": "user", "content": "task"}))]);
            s.archive_error =
                Some("archive write failed: disk full; full history retained in memory".into());
        }
        h
    }

    /// The retry runs on a blocking worker with no registry guard held:
    /// while its write waits, the registry is free and this (single-
    /// threaded) executor keeps running; its outcome is then applied and the
    /// result collected.
    #[tokio::test]
    async fn a_failed_archive_retry_writes_off_the_executor_without_the_registry() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        let h = failed_archive_handle("sa_retry1", dir.path());
        let state = Arc::clone(&h.state);
        registry.lock().unwrap().register(h);
        let (reached, open) = super::super::archive::retry_gate::gate("sa_retry1");
        let ctx = make_ctx_with_registry(Arc::clone(&registry));
        let task = tokio::spawn(async move {
            SubagentCollectTool
                .execute(json!({"handle_id": "sa_retry1"}), ctx)
                .await
        });
        tokio::task::spawn_blocking(move || reached.recv().unwrap())
            .await
            .unwrap();
        assert!(registry.try_lock().is_ok(), "no registry guard during the write");
        assert_eq!(tokio::spawn(async { 7 }).await.unwrap(), 7, "the executor runs");
        assert!(state.read().unwrap().archive_retrying);
        open.send(()).unwrap();
        let body: Value = serde_json::from_str(&task.await.unwrap().unwrap()).unwrap();
        assert_eq!(body["status"], "completed");
        let s = state.read().unwrap();
        assert_eq!(s.archive_error, None);
        assert!(s.history.is_none());
        assert_eq!(s.archive_path, Some(dir.path().join("sa_retry1.json")));
        assert!(!s.archive_retrying);
    }

    /// A collect dropped while its retry writes leaves the retry to finish
    /// and apply its outcome.
    #[tokio::test]
    async fn a_dropped_collect_leaves_its_retry_to_finish() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        let h = failed_archive_handle("sa_retry2", dir.path());
        let state = Arc::clone(&h.state);
        registry.lock().unwrap().register(h);
        let (reached, open) = super::super::archive::retry_gate::gate("sa_retry2");
        let ctx = make_ctx_with_registry(Arc::clone(&registry));
        let task = tokio::spawn(async move {
            SubagentCollectTool
                .execute(json!({"handle_id": "sa_retry2"}), ctx)
                .await
        });
        tokio::task::spawn_blocking(move || reached.recv().unwrap())
            .await
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        open.send(()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while state.read().unwrap().archive_retrying {
            assert!(std::time::Instant::now() < deadline, "the retry never finished");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let s = state.read().unwrap();
        assert_eq!(s.archive_path, Some(dir.path().join("sa_retry2.json")));
        assert_eq!(s.archive_error, None);
    }

    /// One retry per worker at a time, applied only to the state it was
    /// taken from; a v2 run or a published archive is never retried.
    #[test]
    fn a_retry_applies_only_to_the_state_it_was_taken_from() {
        use super::super::archive::ArchiveRetry;
        let dir = tempfile::tempdir().unwrap();
        let h = failed_archive_handle("sa_retry3", dir.path());
        let retry = ArchiveRetry::take(&h.state, "sa_retry3").unwrap();
        assert!(ArchiveRetry::take(&h.state, "sa_retry3").is_none(), "one in flight");
        // The history it snapshotted was replaced meanwhile.
        h.state.write().unwrap().history =
            Some(vec![Arc::new(json!({"role": "user", "content": "other"}))]);
        retry.write_and_apply(&h.state);
        {
            let s = h.state.read().unwrap();
            assert_eq!(s.archive_path, None, "a stale outcome is dropped");
            assert!(s.archive_error.is_some());
            assert!(!s.archive_retrying);
        }
        // A published archive appeared meanwhile.
        let retry = ArchiveRetry::take(&h.state, "sa_retry3").unwrap();
        let elsewhere = dir.path().join("elsewhere.json");
        h.state.write().unwrap().archive_path = Some(elsewhere.clone());
        retry.write_and_apply(&h.state);
        assert_eq!(h.state.read().unwrap().archive_path, Some(elsewhere));

        let h = failed_archive_handle("sa_retry4", dir.path());
        h.state.write().unwrap().v2_records = true;
        assert!(ArchiveRetry::take(&h.state, "sa_retry4").is_none(), "v2: never as v1");
        let h = failed_archive_handle("sa_retry5", dir.path());
        h.state.write().unwrap().finalization_ready = false;
        assert!(ArchiveRetry::take(&h.state, "sa_retry5").is_none(), "not finalized");
    }

    /// A retry is of a failed archive only: a handle that already points at
    /// a published archive is never retried, whatever error it still shows.
    #[test]
    fn a_published_archive_is_never_taken_for_a_retry() {
        use super::super::archive::ArchiveRetry;
        let dir = tempfile::tempdir().unwrap();
        let h = failed_archive_handle("sa_retry6", dir.path());
        h.state.write().unwrap().archive_path = Some(dir.path().join("published.json"));
        assert!(ArchiveRetry::take(&h.state, "sa_retry6").is_none());
        assert!(!h.state.read().unwrap().archive_retrying);
    }

    /// Concurrent observation (transient, documented on `ArchiveRetry`): a
    /// collect that finds another caller's retry in flight does not wait for
    /// it. It reports the failed write as it stands (the error, no archive);
    /// the retrying collect, and every collect after it, report the archive.
    #[tokio::test]
    async fn a_collect_during_another_collects_retry_sees_the_failed_write_until_it_lands() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        let h = failed_archive_handle("sa_retry7", dir.path());
        let state = Arc::clone(&h.state);
        registry.lock().unwrap().register(h);
        let (reached, open) = super::super::archive::retry_gate::gate("sa_retry7");
        let ctx = make_ctx_with_registry(Arc::clone(&registry));
        let first = tokio::spawn(async move {
            SubagentCollectTool
                .execute(json!({"handle_id": "sa_retry7"}), ctx)
                .await
        });
        tokio::task::spawn_blocking(move || reached.recv().unwrap())
            .await
            .unwrap();
        let collect = || {
            let ctx = make_ctx_with_registry(Arc::clone(&registry));
            async move {
                let body = SubagentCollectTool
                    .execute(json!({"handle_id": "sa_retry7"}), ctx)
                    .await
                    .unwrap();
                serde_json::from_str::<Value>(&body).unwrap()
            }
        };
        let during = collect().await;
        assert!(state.read().unwrap().archive_retrying, "still writing");
        assert!(
            during["archiveError"].as_str().unwrap().contains("disk full"),
            "{during}"
        );
        assert!(during.get("archive_path").is_none(), "{during}");
        assert_eq!(during["status"], "completed");

        open.send(()).unwrap();
        let path = json!(dir.path().join("sa_retry7.json"));
        let retried: Value = serde_json::from_str(&first.await.unwrap().unwrap()).unwrap();
        assert_eq!(retried["archive_path"], path, "{retried}");
        assert!(retried["archiveError"].is_null(), "{retried}");
        let after = collect().await;
        assert_eq!(after["archive_path"], path, "{after}");
        assert!(after["archiveError"].is_null(), "{after}");
    }
}
