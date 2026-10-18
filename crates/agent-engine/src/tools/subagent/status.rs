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
        let handle_id = params["handle_id"]
            .as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'handle_id' parameter".to_string()))?
            .to_string();

        // ── Registry lookup ────────────────────────────────────────────────────

        let registry = ctx.capabilities.subagent_registry.as_ref().ok_or_else(|| {
            RuntimeError::Tool("SubagentRegistry not available on this ToolContext".to_string())
        })?;

        let reg = registry.lock().unwrap();

        let Some(handle) = reg.get(&handle_id) else {
            // Not live: reaped, or carried over a daemon reload. Its archive
            // still answers (and still resumes).
            let archive_path = reg.archive_path(&handle_id);
            let stopped_by_reload = reg.was_stopped_by_reload(&handle_id);
            drop(reg);
            return archived_status(&handle_id, archive_path, stopped_by_reload);
        };

        // ── Build response ─────────────────────────────────────────────────────
        // Clone all needed data under the lock, then drop lock before char traversal
        let full: String = handle.partial_output();
        let agent_name = handle.agent_name.clone();
        let status = handle.status();
        let status_str = status.as_str().to_string();
        let elapsed = handle.elapsed_secs();
        let tool_count = handle.tool_log().len();
        let model = handle.model.clone();
        let terminal = handle.terminal_diagnostic();
        let authorization = handle.authorization.clone();
        let archive_error = handle.archive_error();
        let _ = handle;
        drop(reg);

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
        Ok(resp.to_string())
    }
}

/// `subagent_status` of a handle that is no longer live, from its archive.
fn archived_status(
    handle_id: &str,
    archive_path: Option<std::path::PathBuf>,
    stopped_by_reload: bool,
) -> Result<String> {
    let not_found = || RuntimeError::Tool(format!("No subagent found with handle_id '{}'", handle_id));
    let path = archive_path.ok_or_else(not_found)?;
    let archive = super::archive::read_archive(&path).map_err(|e| {
        RuntimeError::Tool(format!(
            "Subagent '{handle_id}' is no longer live and its archive ({}) could not be read: {e}",
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
    Ok(json!({
        "handle_id":      handle_id,
        "agent_name":     archive.meta.agent_name,
        "status":         archive.status,
        "partial_output": partial_output,
        "model":          archive.meta.model,
        "archived":       true,
        "stopped_by_reload": stopped_by_reload,
        "archive_path":   path,
        "note":           note,
    })
    .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// After a reload the old id still answers from its archive, and says
    /// the reload (not the user) stopped it.
    #[test]
    fn archived_status_reports_a_reload_stopped_worker() {
        let dir = tempfile::tempdir().unwrap();
        let meta = crate::runtime::subagent::SubagentArchiveMeta {
            agent_name: "integrator".into(),
            model: "m".into(),
            system_prompt: "s".into(),
            timeout_secs: 60,
        };
        let path = super::super::archive::write_archive(
            dir.path(),
            "sa_288",
            &meta,
            "cancelled",
            None,
            "partial words",
            &[],
        )
        .unwrap();
        let v: Value =
            serde_json::from_str(&archived_status("sa_288", Some(path.clone()), true).unwrap()).unwrap();
        assert_eq!(v["status"], "cancelled");
        assert_eq!(v["agent_name"], "integrator");
        assert_eq!(v["archived"], true);
        assert_eq!(v["stopped_by_reload"], true);
        assert_eq!(v["partial_output"], "partial words");
        assert!(v["note"].as_str().unwrap().contains("daemon reload"));

        let v: Value =
            serde_json::from_str(&archived_status("sa_288", Some(path), false).unwrap()).unwrap();
        assert!(!v["note"].as_str().unwrap().contains("daemon reload"));

        let err = archived_status("sa_9", None, false).unwrap_err().to_string();
        assert!(err.contains("No subagent found"), "{err}");
    }
}
