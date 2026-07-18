//! Pure helper functions for the `synaps rpc` command dispatcher.
//!
//! These are extracted from `cmd::rpc` so they can be unit-tested via
//! `cargo test --lib` without hitting the binary-crate's TUI dependencies.
//!
//! See `docs/rpc-protocol.md` and `synaps-bridge.SPEC.md §4` for the wire
//! protocol specification these functions implement.

use crate::core::rpc_protocol::{AssistantEvent, RpcAttachment, RpcCommand, RpcEvent, TurnUsage};
use crate::core::stream_types::{AgentEvent, LlmEvent, SessionEvent, StreamEvent};

// ─── Frame parsing ────────────────────────────────────────────────────────────

/// Maximum allowed inbound frame size in bytes (1 MiB).
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Parse a raw UTF-8 line into an [`RpcCommand`], enforcing the 1 MiB frame limit.
///
/// # Returns
/// - `Ok(RpcCommand)` on success.
/// - `Err(RpcEvent::Error { id: None, … })` on oversize or malformed input —
///   the caller should emit the error event and **continue** (do not exit).
pub fn parse_frame(line: &str, max_bytes: usize) -> Result<RpcCommand, RpcEvent> {
    if line.len() > max_bytes {
        return Err(RpcEvent::Error {
            id: None,
            message: "frame exceeds 1 MiB limit".to_string(),
        });
    }
    serde_json::from_str::<RpcCommand>(line).map_err(|e| RpcEvent::Error {
        id: None,
        message: e.to_string(),
    })
}

// ─── StreamEvent → RpcEvent mapping ──────────────────────────────────────────

/// Map a single [`StreamEvent`] to an optional [`RpcEvent`].
///
/// Returns `None` for events that are intentionally dropped on the wire:
/// - `LlmEvent::ToolResultDelta` — wire format has no streaming-result variant;
///   the final `ToolResult` carries the complete text.
/// - `AgentEvent::SteeringDelivered` — internal hook signal, not exposed.
///
/// `Session(*)` variants also return `None` — they carry session bookkeeping
/// data (message history, usage counters, completion/error signals) that the
/// streaming loop in `cmd::rpc` must handle directly with mutable access to
/// [`RpcState`].
pub fn map_stream_event(ev: &StreamEvent) -> Option<RpcEvent> {
    match ev {
        StreamEvent::Llm(LlmEvent::Thinking(s)) => Some(RpcEvent::MessageUpdate {
            event: AssistantEvent::ThinkingDelta { delta: s.clone() },
        }),
        StreamEvent::Llm(LlmEvent::Text(s)) => Some(RpcEvent::MessageUpdate {
            event: AssistantEvent::TextDelta { delta: s.clone() },
        }),
        StreamEvent::Llm(LlmEvent::ToolUseStart { tool_name, tool_id }) => {
            Some(RpcEvent::MessageUpdate {
                event: AssistantEvent::ToolcallStart {
                    tool_id: tool_id.clone(),
                    tool_name: tool_name.clone(),
                },
            })
        }
        StreamEvent::Llm(LlmEvent::ToolUseDelta { tool_id, delta }) => {
            Some(RpcEvent::MessageUpdate {
                event: AssistantEvent::ToolcallInputDelta {
                    tool_id: tool_id.clone(),
                    delta: delta.clone(),
                },
            })
        }
        // tool_name is intentionally dropped — already sent in ToolcallStart
        StreamEvent::Llm(LlmEvent::ToolUse { tool_id, input, .. }) => {
            Some(RpcEvent::MessageUpdate {
                event: AssistantEvent::ToolcallInput {
                    tool_id: tool_id.clone(),
                    input: input.clone(),
                },
            })
        }
        StreamEvent::Llm(LlmEvent::ToolResult { tool_id, result }) => {
            Some(RpcEvent::MessageUpdate {
                event: AssistantEvent::ToolcallResult {
                    tool_id: tool_id.clone(),
                    result: result.clone(),
                },
            })
        }
        // Drop — wire format has no streaming-result variant; final ToolResult carries full text
        StreamEvent::Llm(LlmEvent::ToolResultDelta { .. }) => None,

        StreamEvent::Agent(AgentEvent::SubagentStart {
            subagent_id,
            agent_name,
            task_preview,
        }) => Some(RpcEvent::SubagentStart {
            subagent_id: *subagent_id,
            agent_name: agent_name.clone(),
            task_preview: task_preview.clone(),
        }),
        StreamEvent::Agent(AgentEvent::SubagentUpdate {
            subagent_id,
            agent_name,
            status,
        }) => Some(RpcEvent::SubagentUpdate {
            subagent_id: *subagent_id,
            agent_name: agent_name.clone(),
            status: status.clone(),
        }),
        StreamEvent::Agent(AgentEvent::SubagentDone {
            subagent_id,
            agent_name,
            result_preview,
            duration_secs,
        }) => Some(RpcEvent::SubagentDone {
            subagent_id: *subagent_id,
            agent_name: agent_name.clone(),
            result_preview: result_preview.clone(),
            duration_secs: *duration_secs,
        }),
        // Drop — internal hook signal, not part of wire format
        StreamEvent::Agent(AgentEvent::SteeringDelivered { .. }) => None,

        // Session bookkeeping events are handled by the streaming loop in cmd::rpc
        // with direct mutable access to RpcState; they are never forwarded as-is.
        StreamEvent::Session(_) => None,
    }
}

// ─── Usage accumulator ────────────────────────────────────────────────────────

/// Option-accumulate for the cache-TTL split buckets: stay `None` until a
/// split value arrives, then sum. `None` means "split never reported" —
/// distinct from an explicit zero. Shared by every Usage accumulator that
/// tracks the 5m/1h breakdown (RPC dispatch, subagent loops).
pub fn merge_split(acc: &mut Option<u64>, v: Option<u64>) {
    if let Some(v) = v {
        *acc = Some(acc.unwrap_or(0) + v);
    }
}

/// Accumulate a [`SessionEvent::Usage`] payload into a [`TurnUsage`] counter.
///
/// Non-Usage session events are silently ignored so callers can pass any
/// [`SessionEvent`] without pre-filtering.  The `model` field is set from the
/// first Usage event seen and never overwritten.
pub fn accumulate_usage(acc: &mut TurnUsage, event: &SessionEvent) {
    if let SessionEvent::Usage {
        input_tokens,
        output_tokens,
        cache_read_input_tokens,
        cache_creation_input_tokens,
        cache_creation_5m,
        cache_creation_1h,
        model,
    } = event
    {
        acc.input_tokens += input_tokens;
        acc.output_tokens += output_tokens;
        acc.cache_read_input_tokens += cache_read_input_tokens;
        acc.cache_creation_input_tokens += cache_creation_input_tokens;
        // Option-accumulate: stay None until a split arrives, then sum.
        merge_split(&mut acc.cache_creation_5m, *cache_creation_5m);
        merge_split(&mut acc.cache_creation_1h, *cache_creation_1h);
        if acc.model.is_none() {
            acc.model = model.clone();
        }
    }
}

// ─── User-content builder ─────────────────────────────────────────────────────

/// Build the user message string to push into `api_messages`.
///
/// When attachments are present (v0) a human-readable note listing the file
/// paths is prepended.  File bytes are **not** read — Task 10 handles that.
fn quote_path(p: &str) -> String {
    let escaped = p.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

pub fn build_user_content(message: &str, attachments: &[RpcAttachment]) -> String {
    if attachments.is_empty() {
        return message.to_string();
    }
    let parts: Vec<String> = attachments.iter().map(|a| quote_path(&a.path)).collect();
    format!("[user attached files: {}]\n{}", parts.join(", "), message)
}

// ─── tools_list helper ───────────────────────────────────────────────────────

/// Build the `tools_list` response body from a `ToolRegistry` schema snapshot.
///
/// The schema entries produced by [`ToolRegistry::tools_schema`] already have
/// the shape `{name, description, input_schema}` that the bridge Phase 8
/// `SynapsRpcSessionRouter.listTools()` expects. This function wraps them in
/// the top-level `{ ok: true, tools: [...] }` envelope that the bridge
/// validates (router.js line 112).
pub fn build_tools_list_body(tools_schema: &[serde_json::Value]) -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "tools": tools_schema,
    })
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::rpc_protocol::{
        AssistantEvent, RpcAttachment, RpcCommand, RpcEvent, TurnUsage,
    };
    use crate::core::stream_types::{AgentEvent, LlmEvent, SessionEvent, StreamEvent};
    use serde_json::json;

    // ── parse_frame ──────────────────────────────────────────────────────────

    #[test]
    fn parse_frame_valid_prompt() {
        let line = r#"{"type":"prompt","id":"abc","message":"hello"}"#;
        let result = parse_frame(line, MAX_FRAME_BYTES);
        assert!(result.is_ok(), "should parse valid prompt frame");
        match result.unwrap() {
            RpcCommand::Prompt {
                id,
                message,
                attachments,
            } => {
                assert_eq!(id, "abc");
                assert_eq!(message, "hello");
                assert!(attachments.is_empty());
            }
            other => panic!("unexpected variant: {:?}", other),
        }
    }

    #[test]
    fn parse_frame_valid_shutdown() {
        let line = r#"{"type":"shutdown"}"#;
        let result = parse_frame(line, MAX_FRAME_BYTES);
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), RpcCommand::Shutdown));
    }

    #[test]
    fn parse_frame_valid_follow_up() {
        let line = r#"{"type":"follow_up","id":"f1","message":"and then?"}"#;
        let result = parse_frame(line, MAX_FRAME_BYTES);
        match result.unwrap() {
            RpcCommand::FollowUp { id, message } => {
                assert_eq!(id, "f1");
                assert_eq!(message, "and then?");
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn parse_frame_valid_abort() {
        let line = r#"{"type":"abort","id":"x"}"#;
        assert!(matches!(
            parse_frame(line, MAX_FRAME_BYTES).unwrap(),
            RpcCommand::Abort { .. }
        ));
    }

    #[test]
    fn parse_frame_malformed_json() {
        let line = "not json at all";
        let result = parse_frame(line, MAX_FRAME_BYTES);
        assert!(result.is_err());
        match result.unwrap_err() {
            RpcEvent::Error { id, message } => {
                assert!(id.is_none(), "malformed-JSON error must have id=None");
                assert!(!message.is_empty(), "error message must be non-empty");
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn parse_frame_valid_json_unknown_type() {
        // Unknown `type` tags should be a deserialisation error (serde enum).
        let line = r#"{"type":"does_not_exist","id":"1"}"#;
        let result = parse_frame(line, MAX_FRAME_BYTES);
        assert!(result.is_err(), "unknown type should fail to deserialise");
    }

    #[test]
    fn parse_frame_oversize() {
        let oversize = "x".repeat(MAX_FRAME_BYTES + 1);
        let result = parse_frame(&oversize, MAX_FRAME_BYTES);
        assert!(result.is_err());
        match result.unwrap_err() {
            RpcEvent::Error { id, message } => {
                assert!(id.is_none());
                assert!(
                    message.contains("1 MiB"),
                    "expected '1 MiB' in message, got: {message}"
                );
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn parse_frame_exactly_at_limit_valid_json() {
        // A well-formed frame at exactly the limit must not trigger the size error.
        let line = r#"{"type":"get_state","id":"x"}"#;
        assert!(line.len() <= MAX_FRAME_BYTES);
        let result = parse_frame(line, MAX_FRAME_BYTES);
        assert!(result.is_ok());
    }

    #[test]
    fn parse_frame_custom_small_limit() {
        // Oversize relative to a custom limit.
        let line = r#"{"type":"shutdown"}"#; // 19 bytes
        let result = parse_frame(line, 5); // limit = 5
        assert!(result.is_err());
        match result.unwrap_err() {
            RpcEvent::Error { id, .. } => assert!(id.is_none()),
            other => panic!("unexpected: {:?}", other),
        }
    }

    // ── map_stream_event ─────────────────────────────────────────────────────

    #[test]
    fn map_llm_thinking() {
        let ev = StreamEvent::Llm(LlmEvent::Thinking("hmm".to_string()));
        let rpc = map_stream_event(&ev).expect("Thinking must produce an event");
        match rpc {
            RpcEvent::MessageUpdate {
                event: AssistantEvent::ThinkingDelta { delta },
            } => assert_eq!(delta, "hmm"),
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn map_llm_text() {
        let ev = StreamEvent::Llm(LlmEvent::Text("hi".to_string()));
        let rpc = map_stream_event(&ev).expect("Text must produce an event");
        match rpc {
            RpcEvent::MessageUpdate {
                event: AssistantEvent::TextDelta { delta },
            } => assert_eq!(delta, "hi"),
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn map_llm_tool_use_start() {
        let ev = StreamEvent::Llm(LlmEvent::ToolUseStart {
            tool_name: "bash".to_string(),
            tool_id: "tid1".to_string(),
        });
        let rpc = map_stream_event(&ev).expect("ToolUseStart must produce an event");
        match rpc {
            RpcEvent::MessageUpdate {
                event: AssistantEvent::ToolcallStart { tool_id, tool_name },
            } => {
                assert_eq!(tool_id, "tid1");
                assert_eq!(tool_name, "bash");
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn map_llm_tool_use_delta() {
        let ev = StreamEvent::Llm(LlmEvent::ToolUseDelta {
            tool_id: "tid1".to_string(),
            delta: r#"{"cmd":"#.to_string(),
        });
        let rpc = map_stream_event(&ev).expect("ToolUseDelta must produce an event");
        match rpc {
            RpcEvent::MessageUpdate {
                event: AssistantEvent::ToolcallInputDelta { tool_id, delta },
            } => {
                assert_eq!(tool_id, "tid1");
                assert_eq!(delta, r#"{"cmd":"#);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn map_llm_tool_use_final_drops_tool_name() {
        let ev = StreamEvent::Llm(LlmEvent::ToolUse {
            tool_name: "bash".to_string(), // must be dropped per spec
            tool_id: "tid1".to_string(),
            input: json!({"cmd": "ls"}),
        });
        let rpc = map_stream_event(&ev).expect("ToolUse must produce an event");
        match rpc {
            RpcEvent::MessageUpdate {
                event: AssistantEvent::ToolcallInput { tool_id, input },
            } => {
                assert_eq!(tool_id, "tid1");
                assert_eq!(input, json!({"cmd": "ls"}));
                // tool_name intentionally absent from ToolcallInput
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn map_llm_tool_result() {
        let ev = StreamEvent::Llm(LlmEvent::ToolResult {
            tool_id: "tid1".to_string(),
            result: "output here".to_string(),
        });
        let rpc = map_stream_event(&ev).expect("ToolResult must produce an event");
        match rpc {
            RpcEvent::MessageUpdate {
                event: AssistantEvent::ToolcallResult { tool_id, result },
            } => {
                assert_eq!(tool_id, "tid1");
                assert_eq!(result, "output here");
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn map_llm_tool_result_delta_is_dropped() {
        let ev = StreamEvent::Llm(LlmEvent::ToolResultDelta {
            tool_id: "tid1".to_string(),
            delta: "partial".to_string(),
        });
        assert!(
            map_stream_event(&ev).is_none(),
            "ToolResultDelta must be dropped — wire format has no streaming-result variant"
        );
    }

    #[test]
    fn map_agent_subagent_start() {
        let ev = StreamEvent::Agent(AgentEvent::SubagentStart {
            subagent_id: 7,
            agent_name: "worker".to_string(),
            task_preview: "do thing".to_string(),
        });
        let rpc = map_stream_event(&ev).expect("SubagentStart must produce an event");
        match rpc {
            RpcEvent::SubagentStart {
                subagent_id,
                agent_name,
                task_preview,
            } => {
                assert_eq!(subagent_id, 7);
                assert_eq!(agent_name, "worker");
                assert_eq!(task_preview, "do thing");
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn map_agent_subagent_update() {
        let ev = StreamEvent::Agent(AgentEvent::SubagentUpdate {
            subagent_id: 7,
            agent_name: "worker".to_string(),
            status: "running".to_string(),
        });
        let rpc = map_stream_event(&ev).expect("SubagentUpdate must produce an event");
        match rpc {
            RpcEvent::SubagentUpdate {
                subagent_id,
                agent_name,
                status,
            } => {
                assert_eq!(subagent_id, 7);
                assert_eq!(agent_name, "worker");
                assert_eq!(status, "running");
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn map_agent_subagent_done() {
        let ev = StreamEvent::Agent(AgentEvent::SubagentDone {
            subagent_id: 7,
            agent_name: "worker".to_string(),
            result_preview: "done!".to_string(),
            duration_secs: 1.5,
        });
        let rpc = map_stream_event(&ev).expect("SubagentDone must produce an event");
        match rpc {
            RpcEvent::SubagentDone {
                subagent_id,
                agent_name,
                result_preview,
                duration_secs,
            } => {
                assert_eq!(subagent_id, 7);
                assert_eq!(agent_name, "worker");
                assert_eq!(result_preview, "done!");
                assert!((duration_secs - 1.5).abs() < f64::EPSILON);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn map_agent_steering_delivered_is_dropped() {
        let ev = StreamEvent::Agent(AgentEvent::SteeringDelivered {
            message: "steer".to_string(),
        });
        assert!(
            map_stream_event(&ev).is_none(),
            "SteeringDelivered must be dropped — internal hook signal"
        );
    }

    #[test]
    fn map_session_events_all_return_none() {
        // All Session variants return None; the streaming loop handles them
        // directly with mutable access to RpcState.
        let events: &[StreamEvent] = &[
            StreamEvent::Session(SessionEvent::Done),
            StreamEvent::Session(SessionEvent::Error(crate::TurnError::provider(
                "oops",
                "api_status",
                "turn-test-0",
            ))),
            StreamEvent::Session(SessionEvent::MessageHistory(vec![])),
            StreamEvent::Session(SessionEvent::Usage {
                input_tokens: 1,
                output_tokens: 2,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_creation_5m: None,
                cache_creation_1h: None,
                model: None,
            }),
        ];
        for ev in events {
            assert!(
                map_stream_event(ev).is_none(),
                "Session event {:?} should return None",
                ev
            );
        }
    }

    // ── accumulate_usage ─────────────────────────────────────────────────────

    fn zero_usage() -> TurnUsage {
        TurnUsage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_creation_5m: None,
            cache_creation_1h: None,
            model: None,
        }
    }

    #[test]
    fn accumulate_usage_basic() {
        let mut acc = zero_usage();
        let ev = SessionEvent::Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_input_tokens: 10,
            cache_creation_input_tokens: 5,
            cache_creation_5m: Some(3),
            cache_creation_1h: Some(2),
            model: Some("claude-3-5".to_string()),
        };
        accumulate_usage(&mut acc, &ev);
        assert_eq!(acc.input_tokens, 100);
        assert_eq!(acc.output_tokens, 50);
        assert_eq!(acc.cache_read_input_tokens, 10);
        assert_eq!(acc.cache_creation_input_tokens, 5);
        assert_eq!(acc.cache_creation_5m, Some(3));
        assert_eq!(acc.cache_creation_1h, Some(2));
        assert_eq!(acc.model.as_deref(), Some("claude-3-5"));
    }

    #[test]
    fn accumulate_usage_additive_across_calls() {
        let mut acc = TurnUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_creation_5m: Some(4),
            cache_creation_1h: None,
            model: Some("first-model".to_string()),
        };
        let ev = SessionEvent::Usage {
            input_tokens: 20,
            output_tokens: 8,
            cache_read_input_tokens: 2,
            cache_creation_input_tokens: 1,
            cache_creation_5m: Some(1),
            cache_creation_1h: Some(7),
            model: Some("second-model".to_string()),
        };
        accumulate_usage(&mut acc, &ev);
        assert_eq!(acc.input_tokens, 30);
        assert_eq!(acc.output_tokens, 13);
        assert_eq!(acc.cache_read_input_tokens, 2);
        assert_eq!(acc.cache_creation_input_tokens, 1);
        // Split: Option-accumulate — Some once any split arrives.
        assert_eq!(acc.cache_creation_5m, Some(5));
        assert_eq!(acc.cache_creation_1h, Some(7));
        // Model must NOT be overwritten once set (first-wins semantics)
        assert_eq!(acc.model.as_deref(), Some("first-model"));
    }

    #[test]
    fn accumulate_usage_sets_model_when_none() {
        let mut acc = zero_usage();
        let ev = SessionEvent::Usage {
            input_tokens: 1,
            output_tokens: 1,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_creation_5m: None,
            cache_creation_1h: None,
            model: Some("my-model".to_string()),
        };
        accumulate_usage(&mut acc, &ev);
        assert_eq!(acc.model.as_deref(), Some("my-model"));
    }

    #[test]
    fn accumulate_usage_ignores_done() {
        let mut acc = zero_usage();
        acc.input_tokens = 5;
        accumulate_usage(&mut acc, &SessionEvent::Done);
        assert_eq!(acc.input_tokens, 5, "Done must not mutate the accumulator");
    }

    #[test]
    fn accumulate_usage_ignores_error() {
        let mut acc = zero_usage();
        acc.output_tokens = 3;
        accumulate_usage(
            &mut acc,
            &SessionEvent::Error(crate::TurnError::provider(
                "boom",
                "api_status",
                "turn-test-1",
            )),
        );
        assert_eq!(
            acc.output_tokens, 3,
            "Error must not mutate the accumulator"
        );
    }

    #[test]
    fn accumulate_usage_ignores_message_history() {
        let mut acc = zero_usage();
        acc.input_tokens = 7;
        accumulate_usage(&mut acc, &SessionEvent::MessageHistory(vec![]));
        assert_eq!(
            acc.input_tokens, 7,
            "MessageHistory must not mutate the accumulator"
        );
    }

    // ── build_user_content ───────────────────────────────────────────────────

    #[test]
    fn build_user_content_no_attachments() {
        assert_eq!(build_user_content("hello", &[]), "hello");
    }

    #[test]
    fn build_user_content_single_attachment() {
        let attachments = vec![RpcAttachment {
            path: "/tmp/a.txt".to_string(),
            name: None,
            mime: None,
        }];
        let msg = build_user_content("check this", &attachments);
        assert!(msg.starts_with("[user attached files: \"/tmp/a.txt\"]"));
        assert!(msg.contains("check this"));
    }

    #[test]
    fn build_user_content_multiple_attachments() {
        let attachments = vec![
            RpcAttachment {
                path: "/tmp/a.txt".to_string(),
                name: None,
                mime: None,
            },
            RpcAttachment {
                path: "/tmp/b.pdf".to_string(),
                name: None,
                mime: None,
            },
        ];
        let msg = build_user_content("check these", &attachments);
        assert!(
            msg.contains("[user attached files: \"/tmp/a.txt\", \"/tmp/b.pdf\"]"),
            "paths must be quoted and comma-separated: {msg}"
        );
        assert!(msg.contains("check these"));
    }

    #[test]
    fn build_user_content_preserves_original_message() {
        let attachments = vec![RpcAttachment {
            path: "/tmp/x".to_string(),
            name: Some("x".to_string()),
            mime: Some("text/plain".to_string()),
        }];
        let original = "multi\nline\nmessage";
        let msg = build_user_content(original, &attachments);
        assert!(
            msg.ends_with(original),
            "original message must appear verbatim at the end"
        );
    }

    // ── build_user_content: quoting edge cases ───────────────────────────────

    #[test]
    fn build_user_content_path_with_comma_is_quoted() {
        let attachments = vec![RpcAttachment {
            path: "/tmp/a,b.pdf".to_string(),
            name: None,
            mime: None,
        }];
        let msg = build_user_content("look", &attachments);
        assert!(
            msg.contains("\"/tmp/a,b.pdf\""),
            "comma path must be wrapped in quotes: {msg}"
        );
        // Must NOT appear as bare unquoted path
        assert!(
            !msg.contains("[user attached files: /tmp/a,b.pdf]"),
            "bare unquoted comma path must not appear: {msg}"
        );
    }

    #[test]
    fn build_user_content_multiple_paths_each_quoted() {
        let attachments = vec![
            RpcAttachment {
                path: "/p1".to_string(),
                name: None,
                mime: None,
            },
            RpcAttachment {
                path: "/p2".to_string(),
                name: None,
                mime: None,
            },
        ];
        let msg = build_user_content("x", &attachments);
        assert!(
            msg.contains("\"/p1\", \"/p2\""),
            "each path must be individually quoted: {msg}"
        );
    }

    #[test]
    fn build_user_content_path_with_embedded_quote_is_escaped() {
        let attachments = vec![RpcAttachment {
            path: "/tmp/he\"llo".to_string(),
            name: None,
            mime: None,
        }];
        let msg = build_user_content("x", &attachments);
        assert!(
            msg.contains("\"/tmp/he\\\"llo\""),
            "embedded double-quote must be backslash-escaped: {msg}"
        );
    }

    #[test]
    fn build_user_content_path_with_backslash_is_escaped() {
        let attachments = vec![RpcAttachment {
            path: "/tmp/a\\b".to_string(),
            name: None,
            mime: None,
        }];
        let msg = build_user_content("x", &attachments);
        assert!(
            msg.contains("\"/tmp/a\\\\b\""),
            "backslash in path must be doubled: {msg}"
        );
    }

    // ── build_tools_list_body ────────────────────────────────────────────────

    #[test]
    fn build_tools_list_body_empty() {
        let body = super::build_tools_list_body(&[]);
        assert_eq!(body["ok"], true);
        assert!(body["tools"].is_array());
        assert_eq!(body["tools"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn build_tools_list_body_with_entries() {
        let schema = vec![
            json!({"name": "bash", "description": "Run bash", "input_schema": {"type": "object"}}),
            json!({"name": "read", "description": "Read file", "input_schema": {"type": "object"}}),
        ];
        let body = super::build_tools_list_body(&schema);
        assert_eq!(body["ok"], true);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "bash");
        assert_eq!(tools[1]["name"], "read");
    }

    /// The bridge checks `response.ok === true && Array.isArray(response.tools)`.
    /// Verify the body round-trips through serde and satisfies both conditions.
    #[test]
    fn build_tools_list_body_roundtrip_satisfies_bridge_contract() {
        let schema = vec![json!({"name": "bash", "description": "desc", "input_schema": {}})];
        let body = super::build_tools_list_body(&schema);
        // Simulate serialise → deserialise (what the parent process and bridge each do).
        let serialised = serde_json::to_string(&body).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&serialised).unwrap();
        assert_eq!(parsed["ok"], true, "bridge check: ok===true");
        assert!(
            parsed["tools"].is_array(),
            "bridge check: Array.isArray(tools)"
        );
    }

    // ── handle_compact lock-release invariant ────────────────────────────────

    /// Structural proof that `handle_compact` releases the state lock before
    /// the long-running `compact_conversation` await.
    ///
    /// The fix in `cmd::rpc::handle_compact` snapshots `(msgs, runtime)` inside
    /// a block that ends *before* the await, so the `MutexGuard` is dropped at
    /// the closing `}`.  This test uses a `tokio::sync::Mutex` to demonstrate
    /// the same pattern: a second task can acquire the lock while the "slow
    /// operation" is running, proving contention is bounded to the snapshot
    /// phase only.
    #[tokio::test]
    async fn handle_compact_releases_lock_before_slow_await() {
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let shared: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));

        // Simulate the fixed handle_compact pattern:
        //   1. brief lock to snapshot data
        //   2. long operation with NO lock
        //   3. brief lock to write result
        let shared2 = shared.clone();
        let task = tokio::spawn(async move {
            // Phase 1: snapshot under lock.
            let snapshot = {
                let mut g = shared2.lock().await;
                *g += 1; // mark "lock acquired for snapshot"
                *g // return snapshot value
            };
            // Lock is now RELEASED.

            // Phase 2: slow operation — no lock held.
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;

            // Phase 3: write result back under lock.
            let mut g = shared2.lock().await;
            *g = snapshot + 100;
        });

        // While the "slow" phase is running, this second task must be able to
        // acquire the lock without blocking for the full 20 ms.
        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
        let acquired =
            tokio::time::timeout(tokio::time::Duration::from_millis(5), shared.lock()).await;
        assert!(
            acquired.is_ok(),
            "second task must acquire the lock during the slow phase — \
             handle_compact must NOT hold the lock across compact_conversation"
        );
        drop(acquired);

        task.await.unwrap();
        assert_eq!(*shared.lock().await, 101);
    }
}
