//! SubagentStatusTool — poll the current state of a running or completed reactive subagent.
//!
//! Returns a lightweight snapshot: lifecycle status, last 500 chars of output,
//! elapsed wall-clock seconds, and tool-use count. Non-blocking — always returns
//! immediately regardless of the subagent's progress.

use super::super::{Tool, ToolContext};
use crate::{Result, RuntimeError};
use serde_json::{json, Value};

pub struct SubagentStatusTool;

#[async_trait::async_trait]
impl Tool for SubagentStatusTool {
    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Builtin
    }

    fn name(&self) -> &str {
        "subagent_status"
    }

    fn description(&self) -> &str {
        "Poll the current state of a reactive subagent. Returns status \
         (running/completed/cancelled/timed_out/failed), the last 500 characters of output \
         produced so far, elapsed time in seconds, and the number of tool calls \
         made. If the status is 'failed', an 'error' field contains the failure reason. \
         Non-blocking — returns immediately. Use for an occasional progress peek only: \
         subagents are reactive and push a completion event that wakes you when they \
         finish, so never call this in a sleep/poll loop."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "handle_id": {
                    "type": "string",
                    "description": "Handle ID returned by subagent_start (e.g. \"sa_3\")."
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

impl SubagentStatusTool {
    /// [`Tool::execute`] with reaped workers' records below `root`.
    pub(super) async fn execute_in(
        &self,
        params: Value,
        ctx: ToolContext,
        root: std::path::PathBuf,
    ) -> Result<String> {
        let handle_id = params["handle_id"]
            .as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'handle_id' parameter".to_string()))?
            .to_string();

        // ── Registry lookup ────────────────────────────────────────────────────

        let registry = ctx.capabilities.subagent_registry.as_ref().ok_or_else(|| {
            RuntimeError::Tool("SubagentRegistry not available on this ToolContext".to_string())
        })?;

        // The registry guard lives only in this block: nothing below it holds
        // the guard across an await.
        let looked_up = {
            let reg = registry.lock().unwrap();
            match reg.get(&handle_id) {
                // Not live: reaped, or carried over a daemon reload. Its
                // terminal record still answers (and still resumes), read as
                // `subagent_collect` reads it.
                None => Err((
                    reg.archived_record(&handle_id),
                    reg.was_stopped_by_reload(&handle_id),
                )),
                // Clone all needed data under the lock, then drop lock before
                // char traversal.
                Some(handle) => Ok((
                    handle.partial_output(),
                    handle.agent_name.clone(),
                    handle.status(),
                    handle.elapsed_secs(),
                    handle.tool_log().len(),
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
                )),
            }
        };

        // ── Build response ─────────────────────────────────────────────────────
        let (
            full,
            agent_name,
            status,
            elapsed,
            tool_count,
            model,
            terminal,
            authorization,
            archive_error,
            rollover_notice,
            visibility,
            evicted_windows,
            context,
        ) = match looked_up {
            Ok(live) => live,
            Err((record, stopped_by_reload)) => {
                return archived_status(&handle_id, record, stopped_by_reload, root).await
            }
        };
        let status_str = status.as_str().to_string();

        let char_count = full.chars().count();
        let partial_output: String = if char_count > 500 {
            full.chars().skip(char_count - 500).collect()
        } else {
            full
        };

        let output_length = partial_output.chars().count();
        if let Some(orchestration) = &ctx.capabilities.orchestration {
            orchestration
                .poll(&handle_id, &format!("{}:{}", status_str, tool_count))
                .map_err(RuntimeError::Tool)?;
        }

        let mut resp = json!({
            "handle_id":      handle_id,
            "agent_name":     agent_name,
            "status":         status_str,
            "partial_output": partial_output,
            "elapsed_secs":   (elapsed * 10.0).round() / 10.0,
            "tool_count":     tool_count,
            "model":          model,
            "terminal_cause": terminal,
            "authorization":   authorization,
            "archiveError": archive_error,
            "output_length":  output_length
        });
        if let Some(reason) = status.failure_reason() {
            resp["error"] = json!(reason);
        }
        // Present only once the worker's context has rolled over
        // (upstream's host-side rollover, #332).
        if let Some(context) = context {
            resp["context"] = context;
        }
        if let Some(notice) = rollover_notice {
            resp["rolloverNotice"] = json!(notice);
        }
        if let Some(visibility) = visibility {
            visibility.add_to_tool_body(&mut resp);
        }
        // Count only, and only when the resumed run's store evicted any.
        if evicted_windows > 0 {
            resp["evictedWindows"] = json!(evicted_windows);
        }
        Ok(resp.to_string())
    }
}

/// `subagent_status` of a handle that is no longer live, from its terminal
/// `record` (below `root`): a v1 archive, or the v2 final of the run the
/// handle recorded, through `subagent_collect`'s reader
/// ([`super::collect::archived_result`]) on a blocking worker.
async fn archived_status(
    handle_id: &str,
    record: Option<crate::runtime::subagent::ArchivedRecord>,
    stopped_by_reload: bool,
    root: std::path::PathBuf,
) -> Result<String> {
    let not_found = || RuntimeError::Tool(format!("No subagent found with handle_id '{}'", handle_id));
    let record = record.ok_or_else(not_found)?;
    let path = record.path.clone();
    // A record that exists but is refused is an error that says so, never
    // "not found".
    let read =
        tokio::task::spawn_blocking(move || super::collect::archived_result(&record, &root)).await;
    let archive = read
        .map_err(|e| format!("its read did not finish: {e}"))
        .and_then(|read| read)
        .map_err(|e| {
            RuntimeError::Tool(format!(
                "Subagent '{handle_id}' is no longer live and its record ({}) could not be read: {e}",
                path.display()
            ))
        })?;
    let full = archive.output;
    let char_count = full.chars().count();
    let partial_output: String = if char_count > 500 {
        full.chars().skip(char_count - 500).collect()
    } else {
        full
    };
    let note = if stopped_by_reload {
        "Stopped by a daemon reload (not by the user); not running. subagent_resume continues it \
         from this archive; subagent_collect reads its result."
    } else {
        "Not running: finished and reaped. subagent_collect reads its result from this archive; \
         subagent_resume continues it."
    };
    let mut body = json!({
        "handle_id":      handle_id,
        "agent_name":     archive.agent_name,
        "status":         archive.status,
        // Same contract as the live path and subagent_collect: a failed
        // worker's reason travels with its status.
        "error":          archive.error,
        "partial_output": partial_output,
        "model":          archive.model,
        "archived":       true,
        "stopped_by_reload": stopped_by_reload,
        "archive_path":   path,
        "note":           note,
    });
    // Upstream #332: a v1 archive of a worker whose context rolled over
    // host-side says where its earlier windows are.
    if let Some(context) = archive.context {
        body["context"] = context;
    }
    // A v2 final states its counters, under the key subagent_collect uses.
    if let Some(windows) = archive.windows {
        body["rolloverWindows"] = json!(windows);
    }
    Ok(body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::runtime::subagent::ArchivedRecord;

    /// A v1 handle's record (no v2 run recorded).
    fn v1(path: &std::path::Path) -> Option<ArchivedRecord> {
        Some(ArchivedRecord {
            path: path.to_path_buf(),
            v2_final_run: None,
        })
    }

    async fn status_of(
        id: &str,
        record: Option<ArchivedRecord>,
        stopped_by_reload: bool,
        root: &std::path::Path,
    ) -> Result<Value> {
        archived_status(id, record, stopped_by_reload, root.to_path_buf())
            .await
            .map(|body| serde_json::from_str(&body).unwrap())
    }

    /// After a reload the old id still answers from its archive, and says
    /// the reload (not the user) stopped it. A v1 archive's answer keeps its
    /// keys and notes exactly.
    #[tokio::test]
    async fn archived_status_reports_a_reload_stopped_worker() {
        let dir = tempfile::tempdir().unwrap();
        let meta = crate::runtime::subagent::SubagentArchiveMeta {
            agent_name: "integrator".into(),
            model: "m".into(),
            system_prompt: "s".into(),
            timeout_secs: 60,
            cwd: None,
            thinking: None,
            watch_usage: None,
        };
        let path = super::super::archive::write_archive(
            dir.path(),
            "sa_288",
            &meta,
            "cancelled",
            Some("boom"),
            "partial words",
            &[],
        )
        .unwrap();
        let v = status_of("sa_288", v1(&path), true, dir.path()).await.unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "agent_name",
                "archive_path",
                "archived",
                "error",
                "handle_id",
                "model",
                "note",
                "partial_output",
                "status",
                "stopped_by_reload",
            ],
            "a v1 archive's status keeps its shape: {v}"
        );
        assert_eq!(v["handle_id"], "sa_288");
        assert_eq!(v["status"], "cancelled");
        assert_eq!(v["error"], "boom", "archived status keeps the archive's error");
        assert_eq!(v["agent_name"], "integrator");
        assert_eq!(v["model"], "m");
        assert_eq!(v["archived"], true);
        assert_eq!(v["stopped_by_reload"], true);
        assert_eq!(v["partial_output"], "partial words");
        assert_eq!(v["archive_path"], json!(path));
        assert_eq!(
            v["note"],
            "Stopped by a daemon reload (not by the user); not running. subagent_resume \
             continues it from this archive; subagent_collect reads its result."
        );

        let v = status_of("sa_288", v1(&path), false, dir.path()).await.unwrap();
        assert_eq!(
            v["note"],
            "Not running: finished and reaped. subagent_collect reads its result from this \
             archive; subagent_resume continues it."
        );

        let err = status_of("sa_9", None, false, dir.path()).await.unwrap_err().to_string();
        assert!(err.contains("No subagent found"), "{err}");
    }

    /// A reaped worker whose context rolled over still reports where its
    /// earlier windows are; one that never rolled has no `context` key.
    #[tokio::test]
    async fn archived_status_reports_a_rolled_worker_context() {
        let dir = tempfile::tempdir().unwrap();
        let meta = crate::runtime::subagent::SubagentArchiveMeta {
            agent_name: "spike".into(),
            model: "m".into(),
            system_prompt: "s".into(),
            timeout_secs: 60,
            cwd: None,
            thinking: None,
            watch_usage: None,
        };
        let history = vec![
            std::sync::Arc::new(json!({"role":"user","content":"brief"})),
            std::sync::Arc::new(json!({"role":"user","content":"note",
                "_synaps_worker_context": {"schema":"synaps-worker-context/1","window":2,
                "transcripts":[{"window":1,"path":"/a/sa_4.window-1.md","messages":8}]}})),
        ];
        let rolled = super::super::archive::write_archive(
            dir.path(), "sa_4", &meta, "completed", None, "done", &history,
        )
        .unwrap();
        // PARITY: ours' async reader (`status_of`, a v1 record below root).
        let v = status_of("sa_4", v1(&rolled), false, dir.path()).await.unwrap();
        assert_eq!(v["context"]["window"], 2);
        assert_eq!(v["context"]["transcripts"][0], "/a/sa_4.window-1.md");

        let plain = super::super::archive::write_archive(
            dir.path(), "sa_5", &meta, "completed", None, "done", &history[..1],
        )
        .unwrap();
        let v = status_of("sa_5", v1(&plain), false, dir.path()).await.unwrap();
        assert!(v.get("context").is_none());
    }

    /// A record that is gone or refused says so, never "not found".
    #[tokio::test]
    async fn an_unreadable_record_is_an_error_that_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("sa_7.json");
        let err = status_of("sa_7", v1(&gone), false, dir.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Subagent 'sa_7' is no longer live and its record")
                && err.contains("could not be read")
                && err.contains(&gone.display().to_string()),
            "{err}"
        );
        assert!(!err.contains("No subagent found"), "{err}");

        // A v2 final behind a handle that recorded no v2 run (a v1-era
        // handle, or a record an older image carried over a reload, which
        // named no run) is never read as one: the v1 reader refuses its
        // version, and the error says why.
        use super::super::test_fixtures as fx;
        let fin = fx::record(&fx::run(1), fx::offline_binding(), vec![], &[], None);
        let path = dir.path().join("sa_5.json");
        std::fs::write(&path, serde_json::to_vec(&fin).unwrap()).unwrap();
        let err = status_of("sa_5", v1(&path), false, dir.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("could not be read") && err.contains("unsupported subagent archive version 2"),
            "{err}"
        );
    }
}
