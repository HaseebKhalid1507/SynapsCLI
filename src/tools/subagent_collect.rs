//! SubagentCollectTool — block until a reactive subagent finishes and return its full result.
//!
//! Polls the registry with a short sleep interval until the subagent's status
//! leaves `Running`, or until an optional additional timeout expires. This is
//! the natural pair to `subagent_start` — start async, collect when you need
//! the answer.
//!
//! ## Design note on blocking
//! We use `tokio::time::sleep` between polls rather than a true oneshot channel
//! because the registry is the authoritative state store. A completion-notify
//! channel will be added to `SubagentHandle` in a later step, at which point this
//! tool can switch to `tokio::select!` with zero poll overhead.

use serde_json::{json, Value};
use std::time::Duration;
use crate::{Result, RuntimeError};
use super::{Tool, ToolContext};
use crate::tools::subagent_handle::SubagentStatus;

/// How often to re-check the registry while waiting for completion.
const POLL_INTERVAL_MS: u64 = 500;

pub struct SubagentCollectTool;

#[async_trait::async_trait]
impl Tool for SubagentCollectTool {
    fn name(&self) -> &str { "subagent_collect" }

    fn description(&self) -> &str {
        "Block until a reactive subagent finishes and return its full output. \
         Optionally supply an additional timeout (seconds) to cap how long to \
         wait beyond the subagent's own timeout. Use after subagent_start when \
         you need the complete result before continuing."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "handle_id": {
                    "type": "string",
                    "description": "Handle ID returned by subagent_start (e.g. \"sa_3\")."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Additional seconds to wait beyond the subagent's own \
                                    timeout before giving up. Optional — defaults to waiting \
                                    indefinitely (bounded only by the subagent's own timeout)."
                }
            },
            "required": ["handle_id"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let handle_id = params["handle_id"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'handle_id' parameter".to_string()))?
            .to_string();

        let extra_timeout_secs = params["timeout"].as_u64();

        // ── Registry presence check ────────────────────────────────────────────

        let registry = ctx.subagent_registry.as_ref()
            .ok_or_else(|| RuntimeError::Tool(
                "SubagentRegistry not available on this ToolContext".to_string()
            ))?
            .clone();

        // Verify the handle exists before entering the wait loop.
        {
            let reg = registry.lock().unwrap();
            if reg.get(&handle_id).is_none() {
                return Err(RuntimeError::Tool(
                    format!("No subagent found with handle_id '{}'", handle_id)
                ));
            }
        }

        // ── Poll loop ──────────────────────────────────────────────────────────

        let deadline = extra_timeout_secs.map(|secs| {
            std::time::Instant::now() + Duration::from_secs(secs)
        });

        loop {
            // Check completion status — hold the lock only for the read.
            let (done, status_str, output) = {
                let reg = registry.lock().unwrap();
                let handle = reg.get(&handle_id).expect("handle vanished from registry");
                let done = handle.status() != SubagentStatus::Running;
                (done, handle.status().as_str().to_string(), handle.partial_output())
            };

            if done {
                return Ok(json!({
                    "handle_id": handle_id,
                    "status":    status_str,
                    "output":    output
                }).to_string());
            }

            // Check additional timeout — never fires if `deadline` is None.
            if let Some(dl) = deadline {
                if std::time::Instant::now() >= dl {
                    let reg = registry.lock().unwrap();
                    let handle = reg.get(&handle_id).expect("handle vanished from registry");
                    return Ok(json!({
                        "handle_id": handle_id,
                        "status":    "collect_timeout",
                        "output":    handle.partial_output()
                    }).to_string());
                }
            }

            // ── STUB note ──────────────────────────────────────────────────────
            // Once background spawn is wired, a completion-notify channel on
            // SubagentHandle will replace this sleep-poll with:
            //   tokio::select! {
            //       _ = handle.done_rx => { ... }
            //       _ = tokio::time::sleep(deadline_remaining) => { ... }
            //   }
            tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
        }
    }
}
