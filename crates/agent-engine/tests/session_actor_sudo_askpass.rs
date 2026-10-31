//! sudo's askpass request travels the session actor's secret-prompt
//! plumbing like any prompt: a `Prompt` envelope to the attached client,
//! whose `Answer` goes back to the bash tool call and on to sudo. The
//! password never shows up on the event stream or in the tool result.
//!
//! sudo is a fake (see `tools::sudo_askpass::testing`); the broker, its
//! token and the ancestry check are real.
#![cfg(all(target_os = "linux", feature = "command-exec"))]

mod session_actor_common;
use session_actor_common::*;

use std::sync::Arc;

use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, LocalTransport, SessionCommand, SessionEventWire,
};
use agent_engine::tools::sudo_askpass::testing::{run_bash_with_test_askpass, write_fixtures};
use serial_test::serial;

/// Answers the stub's `prompt_fixture` call by running the bash tool, with
/// the askpass broker, on a command that needs sudo.
struct SudoFixtureTool {
    bin: tempfile::TempDir,
    helper: std::path::PathBuf,
}

impl SudoFixtureTool {
    fn new() -> Self {
        let bin = tempfile::tempdir().unwrap();
        let helper = write_fixtures(bin.path()).unwrap();
        Self { bin, helper }
    }
}

#[async_trait::async_trait]
impl agent_engine::Tool for SudoFixtureTool {
    fn name(&self) -> &str {
        "prompt_fixture"
    }
    fn description(&self) -> &str {
        "runs sudo through the bash tool"
    }
    fn parameters(&self) -> agent_engine::Value {
        serde_json::json!({"type": "object"})
    }
    fn origin(&self) -> agent_engine::tools::ToolOrigin {
        agent_engine::tools::ToolOrigin::Builtin
    }
    async fn execute(
        &self,
        _params: agent_engine::Value,
        ctx: agent_engine::ToolContext,
    ) -> agent_engine::Result<String> {
        let command = format!(
            "export PATH=\"{}:$PATH\"; FAKE_SUDO_ECHO=1 sudo echo AUTH_OK",
            self.bin.path().display()
        );
        run_bash_with_test_askpass(&command, Some(60), ctx, &self.helper).await
    }
}

async fn sudo_host() -> Arc<agent_engine::EngineHost> {
    let host = host().await;
    host.parts()
        .tools
        .write()
        .await
        .register(Arc::new(SudoFixtureTool::new()));
    host
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn sudo_password_round_trips_through_the_attached_client() {
    let _h = Home::new();
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = sudo_host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut client, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();

    client.send(submit("go")).await.unwrap();
    let seen = until(&mut client, |e| matches!(e, SessionEventWire::Prompt(_))).await;
    let SessionEventWire::Prompt(prompt) = &seen.last().unwrap().event else {
        unreachable!()
    };
    assert_eq!(prompt.title, "sudo password required");
    assert_eq!(prompt.kind, agent_engine::tools::PromptKind::Secret);
    assert!(
        prompt
            .prompt
            .starts_with("sudo is asking for your password to run:\n"),
        "{prompt:?}"
    );
    assert!(prompt.prompt.contains(" -A echo AUTH_OK"), "{prompt:?}");

    client
        .send(SessionCommand::Answer {
            prompt_id: prompt.id,
            value: Some("swordfish".into()),
        })
        .await
        .unwrap();
    let seen = until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await;
    let results = tool_results(&last_conversation(&seen));
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(
        results[0].contains("AUTH_OK"),
        "sudo got the password: {results:?}"
    );
    assert!(results[0].contains("sudo got: [redacted]"), "{results:?}");
    assert!(
        !seen
            .iter()
            .any(|e| format!("{:?}", e.event).contains("swordfish")),
        "the password never appears on the event stream"
    );
    end(&mut client).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_cancelled_sudo_prompt_stops_the_command() {
    let _h = Home::new();
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = sudo_host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut client, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();

    client.send(submit("go")).await.unwrap();
    let seen = until(&mut client, |e| matches!(e, SessionEventWire::Prompt(_))).await;
    let id = prompt_id(seen.last().unwrap()).unwrap();
    client
        .send(SessionCommand::Answer {
            prompt_id: id,
            value: None,
        })
        .await
        .unwrap();
    let seen = until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await;
    let results = tool_results(&last_conversation(&seen));
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(
        results[0].contains("canceled while waiting for password"),
        "{results:?}"
    );
    assert!(!results[0].contains("AUTH_OK"), "{results:?}");
    end(&mut client).await;
}
