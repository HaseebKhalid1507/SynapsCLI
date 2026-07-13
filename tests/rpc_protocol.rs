//! Golden round-trip tests for `core::rpc_protocol`.
//!
//! Organised into three modules:
//!   - `rpc_command`   — all 11 `RpcCommand` variants
//!   - `rpc_event`     — all 8  `RpcEvent` variants
//!   - `assistant_event` — all 6 `AssistantEvent` variants
//!
//! Each module contains:
//!   * Individual round-trip tests (serialize → deserialize → `assert_eq!`)
//!   * At least one golden-string test per category (exact byte-sequence check)

use synaps_cli::core::rpc_protocol::{
    AssistantEvent, RpcAttachment, RpcCommand, RpcEvent, TurnUsage,
    RPC_PROTOCOL_VERSION,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn round_trip_cmd(cmd: &RpcCommand) -> RpcCommand {
    let s = serde_json::to_string(cmd).expect("serialize RpcCommand");
    serde_json::from_str(&s).expect("deserialize RpcCommand")
}

fn round_trip_event(ev: &RpcEvent) -> RpcEvent {
    let s = serde_json::to_string(ev).expect("serialize RpcEvent");
    serde_json::from_str(&s).expect("deserialize RpcEvent")
}

fn round_trip_assistant(ev: &AssistantEvent) -> AssistantEvent {
    let s = serde_json::to_string(ev).expect("serialize AssistantEvent");
    serde_json::from_str(&s).expect("deserialize AssistantEvent")
}

// ---------------------------------------------------------------------------
// Protocol version constant
// ---------------------------------------------------------------------------

#[test]
fn protocol_version_is_one() {
    assert_eq!(RPC_PROTOCOL_VERSION, 1);
}

// ---------------------------------------------------------------------------
// RpcCommand
// ---------------------------------------------------------------------------

mod rpc_command {
    use super::*;

    // --- round-trip tests ---------------------------------------------------

    #[test]
    fn prompt_round_trip() {
        let cmd = RpcCommand::Prompt {
            id: "abc".into(),
            message: "hello".into(),
            attachments: vec![RpcAttachment {
                path: "/tmp/file.txt".into(),
                name: Some("file.txt".into()),
                mime: Some("text/plain".into()),
            }],
        };
        assert_eq!(round_trip_cmd(&cmd), cmd);
    }

    #[test]
    fn prompt_attachments_default_empty() {
        // When `attachments` key is absent the field must default to `[]`.
        let json = r#"{"type":"prompt","id":"y","message":"hi"}"#;
        let cmd: RpcCommand = serde_json::from_str(json).expect("parse");
        match cmd {
            RpcCommand::Prompt { attachments, .. } => {
                assert!(attachments.is_empty(), "expected empty attachments vec");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn follow_up_round_trip() {
        let cmd = RpcCommand::FollowUp {
            id: "1".into(),
            message: "continue".into(),
        };
        assert_eq!(round_trip_cmd(&cmd), cmd);
    }

    #[test]
    fn compact_round_trip() {
        let cmd = RpcCommand::Compact { id: "2".into() };
        assert_eq!(round_trip_cmd(&cmd), cmd);
    }

    #[test]
    fn new_session_round_trip() {
        let cmd = RpcCommand::NewSession { id: "3".into() };
        assert_eq!(round_trip_cmd(&cmd), cmd);
    }

    #[test]
    fn get_messages_round_trip() {
        let cmd = RpcCommand::GetMessages { id: "4".into() };
        assert_eq!(round_trip_cmd(&cmd), cmd);
    }

    #[test]
    fn set_model_round_trip() {
        let cmd = RpcCommand::SetModel {
            id: "5".into(),
            model: "claude-opus-4-5".into(),
        };
        assert_eq!(round_trip_cmd(&cmd), cmd);
    }

    #[test]
    fn get_available_models_round_trip() {
        let cmd = RpcCommand::GetAvailableModels { id: "6".into() };
        assert_eq!(round_trip_cmd(&cmd), cmd);
    }

    #[test]
    fn abort_round_trip() {
        let cmd = RpcCommand::Abort { id: "7".into() };
        assert_eq!(round_trip_cmd(&cmd), cmd);
    }

    #[test]
    fn get_session_stats_round_trip() {
        let cmd = RpcCommand::GetSessionStats { id: "8".into() };
        assert_eq!(round_trip_cmd(&cmd), cmd);
    }

    #[test]
    fn get_state_round_trip() {
        let cmd = RpcCommand::GetState { id: "9".into() };
        assert_eq!(round_trip_cmd(&cmd), cmd);
    }

    #[test]
    fn shutdown_round_trip() {
        let cmd = RpcCommand::Shutdown;
        assert_eq!(round_trip_cmd(&cmd), cmd);
    }

    // --- golden-string tests ------------------------------------------------

    #[test]
    fn golden_prompt_exact_json() {
        let cmd = RpcCommand::Prompt {
            id: "x".into(),
            message: "hi".into(),
            attachments: vec![],
        };
        let got = serde_json::to_string(&cmd).unwrap();
        assert_eq!(
            got,
            r#"{"type":"prompt","id":"x","message":"hi","attachments":[]}"#,
            "RpcCommand::Prompt golden JSON mismatch"
        );
    }

    #[test]
    fn golden_shutdown_exact_json() {
        let cmd = RpcCommand::Shutdown;
        let got = serde_json::to_string(&cmd).unwrap();
        assert_eq!(
            got,
            r#"{"type":"shutdown"}"#,
            "RpcCommand::Shutdown golden JSON mismatch"
        );
    }

    #[test]
    fn golden_set_model_exact_json() {
        let cmd = RpcCommand::SetModel {
            id: "m1".into(),
            model: "claude-sonnet-4-5".into(),
        };
        let got = serde_json::to_string(&cmd).unwrap();
        assert_eq!(
            got,
            r#"{"type":"set_model","id":"m1","model":"claude-sonnet-4-5"}"#,
            "RpcCommand::SetModel golden JSON mismatch"
        );
    }
}

// ---------------------------------------------------------------------------
// RpcEvent
// ---------------------------------------------------------------------------

mod rpc_event {
    use super::*;

    // --- round-trip tests ---------------------------------------------------

    #[test]
    fn message_update_round_trip() {
        let ev = RpcEvent::MessageUpdate {
            event: AssistantEvent::TextDelta { delta: "hello".into() },
        };
        assert_eq!(round_trip_event(&ev), ev);
    }

    #[test]
    fn subagent_start_round_trip() {
        let ev = RpcEvent::SubagentStart {
            subagent_id: 1,
            agent_name: "planner".into(),
            task_preview: "Plan the project".into(),
        };
        assert_eq!(round_trip_event(&ev), ev);
    }

    #[test]
    fn subagent_update_round_trip() {
        let ev = RpcEvent::SubagentUpdate {
            subagent_id: 2,
            agent_name: "coder".into(),
            status: "running".into(),
        };
        assert_eq!(round_trip_event(&ev), ev);
    }

    #[test]
    fn subagent_done_round_trip() {
        let ev = RpcEvent::SubagentDone {
            subagent_id: 3,
            agent_name: "tester".into(),
            result_preview: "All tests passed".into(),
            duration_secs: 1.5,
        };
        assert_eq!(round_trip_event(&ev), ev);
    }

    #[test]
    fn agent_end_round_trip() {
        let ev = RpcEvent::AgentEnd {
            usage: TurnUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_input_tokens: 10,
                cache_creation_input_tokens: 5,
                cache_creation_5m: Some(3),
                cache_creation_1h: Some(2),
                model: Some("claude-3-5-haiku-20241022".into()),
            },
        };
        assert_eq!(round_trip_event(&ev), ev);
    }

    #[test]
    fn response_round_trip() {
        let ev = RpcEvent::Response {
            id: "req-1".into(),
            command: "get_messages".into(),
            body: serde_json::json!({"messages": ["a", "b"]}),
        };
        assert_eq!(round_trip_event(&ev), ev);
    }

    #[test]
    fn error_round_trip_with_id() {
        let ev = RpcEvent::Error {
            id: Some("e1".into()),
            message: "something failed".into(),
        };
        assert_eq!(round_trip_event(&ev), ev);
    }

    #[test]
    fn error_round_trip_no_id() {
        let ev = RpcEvent::Error {
            id: None,
            message: "oversized frame".into(),
        };
        assert_eq!(round_trip_event(&ev), ev);
    }

    #[test]
    fn ready_round_trip() {
        let ev = RpcEvent::Ready {
            session_id: "sess-abc".into(),
            model: "claude-opus-4-5".into(),
            protocol_version: RPC_PROTOCOL_VERSION,
        };
        assert_eq!(round_trip_event(&ev), ev);
    }

    // --- golden-string tests ------------------------------------------------

    #[test]
    fn golden_ready_exact_json() {
        let ev = RpcEvent::Ready {
            session_id: "s1".into(),
            model: "claude-3-opus".into(),
            protocol_version: 1,
        };
        let got = serde_json::to_string(&ev).unwrap();
        assert_eq!(
            got,
            r#"{"type":"ready","session_id":"s1","model":"claude-3-opus","protocol_version":1}"#,
            "RpcEvent::Ready golden JSON mismatch"
        );
    }

    #[test]
    fn golden_error_no_id_exact_json() {
        let ev = RpcEvent::Error {
            id: None,
            message: "bad".into(),
        };
        let got = serde_json::to_string(&ev).unwrap();
        assert_eq!(
            got,
            r#"{"type":"error","message":"bad"}"#,
            "RpcEvent::Error (no id) golden JSON mismatch"
        );
    }

    /// The `body` field must be **flattened** — its keys appear at the top
    /// level of the JSON object, not nested under a `"body"` key.
    #[test]
    fn golden_response_body_flattened() {
        let ev = RpcEvent::Response {
            id: "x".into(),
            command: "get_messages".into(),
            body: serde_json::json!({"messages": []}),
        };
        let got = serde_json::to_string(&ev).unwrap();
        // "body" key must NOT appear; "messages" must be top-level.
        assert!(
            !got.contains(r#""body""#),
            "body key must not appear in serialized output; got: {got}"
        );
        assert!(
            got.contains(r#""messages""#),
            "messages key must appear at top level; got: {got}"
        );
        // Full golden check.
        assert_eq!(
            got,
            r#"{"type":"response","id":"x","command":"get_messages","messages":[]}"#,
            "RpcEvent::Response golden JSON mismatch"
        );
    }

    #[test]
    fn golden_agent_end_exact_json() {
        let ev = RpcEvent::AgentEnd {
            usage: TurnUsage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_creation_5m: None,
                cache_creation_1h: None,
                model: None,
            },
        };
        let got = serde_json::to_string(&ev).unwrap();
        assert_eq!(
            got,
            r#"{"type":"agent_end","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}"#,
            "RpcEvent::AgentEnd golden JSON mismatch"
        );
    }
}

// ---------------------------------------------------------------------------
// AssistantEvent
// ---------------------------------------------------------------------------

mod assistant_event {
    use super::*;

    // --- round-trip tests ---------------------------------------------------

    #[test]
    fn text_delta_round_trip() {
        let ev = AssistantEvent::TextDelta { delta: "chunk".into() };
        assert_eq!(round_trip_assistant(&ev), ev);
    }

    #[test]
    fn thinking_delta_round_trip() {
        let ev = AssistantEvent::ThinkingDelta { delta: "hmm".into() };
        assert_eq!(round_trip_assistant(&ev), ev);
    }

    #[test]
    fn toolcall_start_round_trip() {
        let ev = AssistantEvent::ToolcallStart {
            tool_id: "t1".into(),
            tool_name: "bash".into(),
        };
        assert_eq!(round_trip_assistant(&ev), ev);
    }

    #[test]
    fn toolcall_input_delta_round_trip() {
        let ev = AssistantEvent::ToolcallInputDelta {
            tool_id: "t2".into(),
            delta: r#"{"cmd"#.into(),
        };
        assert_eq!(round_trip_assistant(&ev), ev);
    }

    #[test]
    fn toolcall_input_round_trip() {
        let ev = AssistantEvent::ToolcallInput {
            tool_id: "t3".into(),
            input: serde_json::json!({"command": "ls"}),
        };
        assert_eq!(round_trip_assistant(&ev), ev);
    }

    #[test]
    fn toolcall_result_round_trip() {
        let ev = AssistantEvent::ToolcallResult {
            tool_id: "t4".into(),
            result: "file.txt\n".into(),
        };
        assert_eq!(round_trip_assistant(&ev), ev);
    }

    // --- golden-string tests ------------------------------------------------

    #[test]
    fn golden_text_delta_exact_json() {
        let ev = AssistantEvent::TextDelta { delta: "hi".into() };
        let got = serde_json::to_string(&ev).unwrap();
        assert_eq!(
            got,
            r#"{"type":"text_delta","delta":"hi"}"#,
            "AssistantEvent::TextDelta golden JSON mismatch"
        );
    }

    #[test]
    fn golden_toolcall_start_exact_json() {
        let ev = AssistantEvent::ToolcallStart {
            tool_id: "id1".into(),
            tool_name: "bash".into(),
        };
        let got = serde_json::to_string(&ev).unwrap();
        assert_eq!(
            got,
            r#"{"type":"toolcall_start","tool_id":"id1","tool_name":"bash"}"#,
            "AssistantEvent::ToolcallStart golden JSON mismatch"
        );
    }
}
