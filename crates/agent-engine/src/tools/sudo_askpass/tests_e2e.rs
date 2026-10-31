//! The bash tool end to end with the askpass broker: a fake sudo runs a
//! protocol helper as its askpass program (see `testing`), the broker
//! raises the prompt, and the answer goes back to sudo and nowhere else.

use super::testing::{run_bash_with_test_askpass, write_fixtures};
use super::{HELPER_NAME, PROMPT_TITLE};
use crate::tools::test_helpers::create_tool_context;
use crate::tools::{PromptKind, SecretPromptHandle, SecretPromptQueue, SecretPromptRequest};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

struct Rig {
    bin: tempfile::TempDir,
    helper: PathBuf,
}

fn rig() -> Rig {
    let bin = tempfile::tempdir().unwrap();
    let helper = write_fixtures(bin.path()).unwrap();
    Rig { bin, helper }
}

impl Rig {
    async fn run(&self, body: &str, ctx: crate::tools::ToolContext) -> crate::Result<String> {
        let command = format!(
            "export PATH=\"{}:$PATH\"\n{body}",
            self.bin.path().display()
        );
        run_bash_with_test_askpass(&command, Some(30), ctx, &self.helper).await
    }
}

fn prompts() -> (SecretPromptHandle, UnboundedReceiver<SecretPromptRequest>) {
    let (tx, rx) = unbounded_channel();
    (SecretPromptHandle::new(tx), rx)
}

/// Plays the user: answers each prompt in turn, returns what was shown.
fn user(
    mut rx: UnboundedReceiver<SecretPromptRequest>,
    answers: &[Option<&str>],
) -> tokio::task::JoinHandle<Vec<(String, String)>> {
    let mut answers: Vec<Option<String>> = answers.iter().map(|a| a.map(str::to_string)).collect();
    answers.reverse();
    tokio::spawn(async move {
        let mut shown = Vec::new();
        while let Some(req) = rx.recv().await {
            shown.push((req.title.clone(), req.prompt.clone()));
            let answer = answers.pop().expect("an unexpected extra prompt");
            let _ = req.response_tx.send(answer);
        }
        assert!(answers.is_empty(), "prompts never raised: {answers:?}");
        shown
    })
}

#[tokio::test]
async fn the_prompt_names_the_command_and_the_password_reaches_only_sudo() {
    let rig = rig();
    let (handle, rx) = prompts();
    let user = user(rx, &[Some("swordfish")]);
    let channel = crate::tools::output::delta_channel();
    let (delta_tx, mut delta_rx) = (channel.sender, channel.receiver);
    let mut ctx = create_tool_context();
    ctx.capabilities.secret_prompt = Some(handle);
    ctx.channels.tx_delta = Some(delta_tx);
    let out = rig
        .run(
            r#"sudo echo AUTH_OK
if read -r x; then echo "stdin gave: $x"; else echo stdin-eof; fi"#,
            ctx,
        )
        .await
        .unwrap();
    let shown = user.await.unwrap();
    let mut streamed = String::new();
    while let Some(delta) = delta_rx.try_drain() {
        streamed.push_str(&delta);
    }

    assert!(out.contains("AUTH_OK"), "sudo got the password: {out}");
    assert!(
        out.contains("stdin-eof"),
        "the command's stdin got nothing: {out}"
    );
    assert!(!out.contains("swordfish") && !streamed.contains("swordfish"));
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].0, PROMPT_TITLE);
    // The fake sudo is a script, so its argv starts `/bin/sh <path>`; the
    // real sudo shows as `sudo -A echo AUTH_OK`.
    let body = &shown[0].1;
    assert!(
        body.starts_with("sudo is asking for your password to run:\nsudo "),
        "{body}"
    );
    assert!(body.contains(" -A echo AUTH_OK\n"), "{body}");
    assert!(body.ends_with("\n[sudo] password for tester:"), "{body}");
}

#[tokio::test]
async fn a_password_the_command_echoes_is_redacted() {
    let rig = rig();
    let (handle, rx) = prompts();
    let user = user(rx, &[Some("swordfish")]);
    let mut ctx = create_tool_context();
    ctx.capabilities.secret_prompt = Some(handle);
    let out = rig
        .run("FAKE_SUDO_ECHO=1 sudo true; echo done", ctx)
        .await
        .unwrap();
    user.await.unwrap();
    assert!(out.contains("sudo got: [redacted]"), "{out}");
    assert!(!out.contains("swordfish"), "{out}");
}

/// Each password request gets its own prompt and its own token: a wrong
/// answer, then a right one.
#[tokio::test]
async fn sequential_requests_are_each_asked() {
    let rig = rig();
    let (handle, rx) = prompts();
    let user = user(rx, &[Some("wrong"), Some("swordfish")]);
    let mut ctx = create_tool_context();
    ctx.capabilities.secret_prompt = Some(handle);
    let out = rig
        .run("sudo echo first; sudo echo second", ctx)
        .await
        .unwrap();
    assert_eq!(user.await.unwrap().len(), 2);
    assert!(out.contains("incorrect password attempt"), "{out}");
    assert!(!out.contains("first") && out.contains("second"), "{out}");
    assert!(
        !out.contains("wrong") && !out.contains("swordfish"),
        "{out}"
    );
}

#[tokio::test]
async fn a_cancelled_prompt_stops_the_command() {
    let rig = rig();
    let (handle, rx) = prompts();
    let user = user(rx, &[None]);
    let mut ctx = create_tool_context();
    ctx.capabilities.secret_prompt = Some(handle);
    let started = std::time::Instant::now();
    let err = rig
        .run("sudo echo nope; sleep 20; echo should-not-run", ctx)
        .await
        .unwrap_err()
        .to_string();
    user.await.unwrap();
    assert!(err.contains("waiting for password"), "{err}");
    assert!(!err.contains("should-not-run"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(15));
}

/// Headless (`synaps run`, no prompt capability): the helper is refused,
/// sudo reports its normal error and the command carries on.
#[tokio::test]
async fn without_a_prompt_capability_sudo_reports_its_own_error() {
    let rig = rig();
    let ctx = create_tool_context();
    assert!(ctx.capabilities.secret_prompt.is_none());
    let out = rig.run("sudo echo nope; echo after:$?", ctx).await.unwrap();
    assert!(out.contains("no interactive client"), "{out}");
    assert!(out.contains("sudo: no password was provided"), "{out}");
    assert!(out.contains("after:1") && !out.contains("nope\n"), "{out}");
}

/// The regression again with the broker active: text alone still asks
/// nothing, only sudo's askpass call does.
#[tokio::test]
async fn lookalike_output_raises_no_prompt_with_the_broker_active() {
    let rig = rig();
    let (handle, mut rx) = prompts();
    let mut ctx = create_tool_context();
    ctx.capabilities.secret_prompt = Some(handle);
    let out = rig
        .run(
            r#"printf '[sudo] password for x: ' >&2
printf 'Password: ' >&2
if read -r got; then echo "stdin gave: $got"; else echo stdin-eof; fi"#,
            ctx,
        )
        .await
        .unwrap();
    assert!(out.contains("stdin-eof"), "{out}");
    assert!(rx.try_recv().is_err(), "no prompt was raised");
}

/// The in-process TUI answers through its own `SecretPromptQueue`: a
/// masked `Secret` prompt, typed and submitted.
#[tokio::test]
async fn the_in_process_tui_queue_answers_it() {
    let rig = rig();
    let (tx, rx) = unbounded_channel();
    let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
    let ui = tokio::spawn(async move {
        let mut queue = SecretPromptQueue::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !queue.is_active() {
            assert!(
                std::time::Instant::now() < deadline,
                "no prompt reached the TUI queue"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
            queue.poll_requests(&rx);
        }
        let active = queue.active().unwrap();
        let seen = (active.kind, active.title.clone(), active.prompt.clone());
        for ch in "swordfish".chars() {
            queue.push_char(ch);
        }
        queue.submit();
        seen
    });
    let mut ctx = create_tool_context();
    ctx.capabilities.secret_prompt = Some(SecretPromptHandle::new(tx));
    let out = rig.run("sudo echo AUTH_OK", ctx).await.unwrap();
    let (kind, title, prompt) = ui.await.unwrap();
    assert_eq!(kind, PromptKind::Secret);
    assert_eq!(title, PROMPT_TITLE);
    assert!(prompt.contains("sudo -A echo AUTH_OK"), "{prompt}");
    assert!(out.contains("AUTH_OK"), "{out}");
}

/// The command sees `SUDO_ASKPASS` naming the helper link in a private
/// directory, which is gone once the call ends.
#[tokio::test]
async fn the_command_points_sudo_at_a_private_helper_link() {
    let rig = rig();
    let out = rig
        .run(
            r#"echo "link=$SUDO_ASKPASS"; stat -c 'mode=%a' "$(dirname "$SUDO_ASKPASS")""#,
            create_tool_context(),
        )
        .await
        .unwrap();
    let link = out
        .lines()
        .find_map(|l| l.strip_prefix("link="))
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("{out}"));
    assert_eq!(link.file_name().unwrap(), HELPER_NAME);
    assert!(out.contains("mode=700"), "{out}");
    assert!(!link.parent().unwrap().exists(), "removed after the call");
}
