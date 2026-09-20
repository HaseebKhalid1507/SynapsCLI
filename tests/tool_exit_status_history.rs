//! F28: a tool that streams output and THEN fails must still tell the model
//! it failed. The streaming path prefers the delta-lane text (stdout/stderr
//! as it arrived) over the tool's summary for `tool_result.content`; the
//! exit status — and the T5 stripped-secret notice — only exist in the
//! summary. Before the fix, `echo before; exit 7` reached the model as a
//! clean `before\n`.
//!
//! Proof is the SECOND request body the stub receives: its `tool_result`
//! content must carry the exit status and the T5 notice.

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::sync::Arc;
use std::time::Duration;

use agent_engine::session::transport::{ClientTransport, LocalTransport};
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts};
use phase2::{spawn_stub, HomeGuard, Script};
use serial_test::serial;

/// SSE: bash tool_use — prints, references a stripped secret, then fails.
const SSE_BASH_FAIL: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_id1\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_fail\",\"name\":\"bash\",\"input\":{}}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",",
    "\"partial_json\":\"{\\\"command\\\":\\\"echo before; echo ${GH_TOKEN:?}; echo after\\\"}\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

const SSE_DONE: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_id2\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":1,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn streamed_then_failed_tool_reports_exit_status_and_notice_to_model() {
    let guard = HomeGuard::new();
    let (url, _hits, bodies) = spawn_stub(Script::SeqSse(&[SSE_BASH_FAIL, SSE_DONE])).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let host: Arc<EngineHost> =
        EngineHost::boot_and_install(HostOpts { profile: None, no_extensions: true })
            .await
            .expect("host boot");
    let handle = host
        .create_session(SessionConfig {
            cwd: Some(guard.home.path().to_path_buf()),
            env: Some(vec![("PATH".into(), std::env::var("PATH").unwrap_or_default())]),
            env_stripped: vec!["GH_TOKEN".into()],
            model_override: Some("claude-sonnet-4-5".into()),
            persist: false,
            ..Default::default()
        })
        .await
        .expect("create session");
    let (mut t, _snap) = LocalTransport::attach(handle, ClientMeta::new(ClientKind::Test))
        .await
        .expect("attach");
    t.send(SessionCommand::Submit { text: "go".into(), attachments: vec![] })
        .await
        .unwrap();
    loop {
        let e = tokio::time::timeout(Duration::from_secs(30), t.next_event())
            .await
            .expect("turn hung")
            .expect("alive");
        if matches!(e.event, SessionEventWire::Idle) {
            break;
        }
    }

    let bodies = bodies.lock().unwrap_or_else(|p| p.into_inner());
    assert!(bodies.len() >= 2, "expected the follow-up request, got {}", bodies.len());
    let second: serde_json::Value = serde_json::from_slice(&bodies[1]).expect("json body");
    let tool_result = second["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .find(|b| b["type"] == "tool_result")
        .cloned()
        .expect("tool_result in follow-up request");
    let content = match &tool_result["content"] {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    };
    assert!(
        content.contains("Command failed (exit"),
        "model must be told the command failed: {content:?}"
    );
    assert!(content.contains("before"), "streamed output must be kept: {content:?}");
    assert!(
        content.contains("note: $GH_TOKEN was stripped"),
        "T5 notice must reach the model: {content:?}"
    );

    t.send(SessionCommand::End { reason: EndReason::ClientQuit }).await.unwrap();
}
