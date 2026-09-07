//! Offline steering regressions. The parent wires this file with `#[cfg(test)]`.
//!
//! Drive the real input adapter and the driver's in-memory seams, not the stream
//! handler: MessageHistory/Done handling can persist to the user's session store.
//! ACKs provisionally append history; later snapshot assignments represent the
//! runtime's replacing MessageHistory event after normal history repair. No provider or
//! preparation API is invoked. Only attachment fixtures touch disk, in TempDirs.

use super::*;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use serde_json::{json, Value};
use synaps_cli::extensions::hooks::events::{HookEvent, HookResult};
use synaps_cli::skills::{keybinds::KeybindRegistry, registry::CommandRegistry};

use super::super::input::{self, InputAction};

type SteeringRx = tokio::sync::mpsc::UnboundedReceiver<String>;

struct Handler;

#[async_trait::async_trait]
impl ExtensionHandler for Handler {
    fn id(&self) -> &str {
        "steering-fixture"
    }

    fn lifecycle_snapshot(&self) -> Option<synaps_cli::extensions::runtime::ExtensionLifecycle> {
        Some(synaps_cli::extensions::runtime::ExtensionLifecycle {
            generation: 7,
            health: ExtensionHealth::Running,
        })
    }

    async fn handle(&self, _: &HookEvent) -> HookResult {
        panic!("steering tests must not invoke extension hooks")
    }

    async fn shutdown(&self) {}
}

fn proposal() -> Proposal {
    Proposal {
        selection: Selection {
            model: "example/exact-model".into(),
            effort: "high".into(),
        },
        prompt: "Continue the explicitly authorized task".into(),
        delay: Duration::from_secs(300),
        notice: String::new(),
    }
}

/// Same App construction seam as the parent's tests; no commands, config
/// discovery, session saves, or provider requests are driven by this fixture.
/// Pin the cosmetic name rather than asserting on the constructor's default.
fn fixture(streaming: bool) -> (App, Runtime) {
    let runtime = Runtime::new_headless();
    let mut app = App::new_with_clock(
        synaps_cli::Session::new("example/exact-model", "high", None),
        super::super::clock::TuiClock::test(),
    );
    app.session.id = "steering-test-session".into();
    app.agent_name = "steering-test-agent".into();
    app.logo_build_t = None;
    app.streaming = streaming;
    app.session_driver.generation = 11;

    let p = proposal();
    let (grant, _) = Grant::from_start(
        "steering-fixture",
        &app.session.id,
        Reply::Start {
            run_id: "steering-test-run".into(),
            models: vec![p.selection.clone()],
            prompt: p.prompt.clone(),
            delay_ms: 300_000,
            // Far away; tests assert exact deadline identity, never elapsed time.
            max_duration_ms: Some(86_400_000),
            feedback_version: Some(1),
            time_checkpoint_version: None,
            context_mode: None,
            notice: String::new(),
        },
    )
    .unwrap();
    let cancel = CancellationToken::new();
    let workers = runtime.subagent_registry().clone();
    let worker_epoch = workers
        .lock()
        .unwrap()
        .set_spawn_cancellation(Some(cancel.clone()));
    let deadline = grant.deadline().unwrap();
    let deadline_cancel = cancel.clone();
    let deadline_workers = workers.clone();
    let deadline_task = Task(tokio::spawn(async move {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
        deadline_cancel.cancel();
        cancel_workers(&deadline_workers, worker_epoch);
    }));
    let mut active = Active {
        grant,
        handler: Arc::new(Handler),
        handler_generation: 7,
        cancel,
        workers,
        worker_epoch,
        _deadline_task: Some(deadline_task),
        proposal: None,
        selection: p.selection.clone(),
        awaiting_terminal: streaming,
        outcome: None,
        feedback: feedback::Tracker::default(),
        completed_feedback: "unknown",
        steering: VecDeque::new(),
    };
    schedule(&mut active, p).unwrap();
    app.session_driver.active = Some(active);
    (app, runtime)
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

fn input_event(app: &mut App, runtime: &Runtime, event: Event) -> InputAction {
    let streaming = app.streaming;
    input::handle_event(
        event,
        app,
        runtime,
        streaming,
        &Arc::new(CommandRegistry::new(&[], Vec::new())),
        &KeybindRegistry::new(),
        3,
    )
}

/// Enter performs the ordinary input clearing/history update. Only the driver's
/// submit seam is then called, deliberately excluding ordinary inference dispatch.
fn enter(app: &mut App, runtime: &Runtime, text: &str) -> String {
    app.set_input_text(text);
    let action = input_event(app, runtime, key(KeyCode::Enter, KeyModifiers::NONE));
    let submitted = match action {
        InputAction::Submit(text) if !app.streaming => text,
        InputAction::StreamingInput(text) if app.streaming => text,
        _ => panic!("expected a text submission"),
    };
    assert_eq!(submitted, text);
    assert!(app.input_is_empty());
    submitted
}

fn queued(app: &App) -> Vec<String> {
    app.session_driver
        .active
        .as_ref()
        .unwrap()
        .steering
        .iter()
        .cloned()
        .collect()
}

fn user(text: &str) -> agent_core::SharedMessage {
    Arc::new(json!({"role": "user", "content": text}))
}

fn values(history: &[agent_core::SharedMessage]) -> Vec<Value> {
    history.iter().map(|message| (**message).clone()).collect()
}

fn displayed_users(app: &App) -> Vec<String> {
    app.transcript
        .messages()
        .iter()
        .filter_map(|message| match &message.msg {
            ChatMessage::User(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn assert_no_send(rx: &mut SteeringRx) {
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
}

/// Snapshot authority and scheduled/pending work separately from mutable guidance.
struct Authority {
    generation: u64,
    session_id: String,
    run_id: String,
    handler: Arc<dyn ExtensionHandler>,
    handler_generation: u64,
    deadline: Option<Instant>,
    deadline_task: tokio::task::Id,
    pending_task: Option<tokio::task::Id>,
    parent: CancellationToken,
    child: CancellationToken,
    workers: Workers,
    worker_epoch: u64,
    selection: Selection,
    due: Option<Instant>,
    awaiting_terminal: bool,
    outcome: Option<(Outcome, String)>,
}

impl Authority {
    fn capture(app: &App) -> Self {
        let active = app.session_driver.active.as_ref().unwrap();
        Self {
            generation: app.session_driver.generation,
            session_id: active.grant.session_id.clone(),
            run_id: active.grant.run_id.clone(),
            handler: active.handler.clone(),
            handler_generation: active.handler_generation,
            deadline: active.grant.deadline(),
            deadline_task: active._deadline_task.as_ref().unwrap().0.id(),
            pending_task: app.session_driver.pending.as_ref().map(|p| p.task.0.id()),
            parent: active.cancel.clone(),
            child: active.cancel.child_token(),
            workers: active.workers.clone(),
            worker_epoch: active.worker_epoch,
            selection: active.selection.clone(),
            due: active.proposal.as_ref().map(|p| p.due),
            awaiting_terminal: active.awaiting_terminal,
            outcome: active.outcome.clone(),
        }
    }

    fn assert_preserved(&self, app: &App) {
        let active = app
            .session_driver
            .active
            .as_ref()
            .expect("grant was revoked");
        assert_eq!(app.session_driver.generation, self.generation);
        assert_eq!(active.grant.session_id, self.session_id);
        assert_eq!(active.grant.run_id, self.run_id);
        assert_eq!(active.grant.plugin_id, "steering-fixture");
        assert!(Arc::ptr_eq(&active.handler, &self.handler));
        assert_eq!(active.handler_generation, self.handler_generation);
        assert_eq!(
            active.grant.deadline(),
            self.deadline,
            "steering must not renew a grant"
        );
        assert_eq!(
            active._deadline_task.as_ref().unwrap().0.id(),
            self.deadline_task
        );
        assert!(!active._deadline_task.as_ref().unwrap().0.is_finished());
        assert_eq!(
            app.session_driver.pending.as_ref().map(|p| p.task.0.id()),
            self.pending_task,
            "steering must not replace/abort pending work"
        );
        if let Some(pending) = &app.session_driver.pending {
            assert_eq!(pending.generation, self.generation);
            assert_eq!(pending.session_id, self.session_id);
        }
        assert!(!self.parent.is_cancelled());
        assert!(!self.child.is_cancelled());
        assert!(!active.cancel.is_cancelled());
        assert!(Arc::ptr_eq(&active.workers, &self.workers));
        assert_eq!(active.worker_epoch, self.worker_epoch);
        assert_eq!(active.selection, self.selection);
        assert_eq!(active.proposal.as_ref().map(|p| p.due), self.due);
        if let Some(scheduled) = &active.proposal {
            assert_eq!(scheduled.proposal.selection, self.selection);
            assert_eq!(scheduled.proposal.prompt, proposal().prompt);
        }
        assert_eq!(active.awaiting_terminal, self.awaiting_terminal);
        assert_eq!(active.outcome, self.outcome);
        assert!(auto_wakes_allowed(app));
        assert!(app.queued_message.is_none());
    }

    /// Token clones do not expose pointer equality. Canceling the retained
    /// parent proves that the active token and its existing child still belong
    /// to the original cancellation tree, rather than a renewed grant.
    fn assert_same_parent(&self, app: &App) {
        self.parent.cancel();
        assert!(self.child.is_cancelled());
        assert!(app
            .session_driver
            .active
            .as_ref()
            .unwrap()
            .cancel
            .is_cancelled());
    }
}

fn feedback_text() -> StreamEvent {
    StreamEvent::Llm(synaps_cli::LlmEvent::Text("same local answer".into()))
}

fn prime_repeated_feedback(app: &mut App) {
    let active = app.session_driver.active.as_mut().unwrap();
    for expected in ["changed", "repeated"] {
        active
            .feedback
            .begin_turn(&active.selection.model, &active.selection.effort);
        active.feedback.observe(&feedback_text());
        assert_eq!(active.feedback.finish(), expected);
    }
    active.completed_feedback = "repeated";
    active
        .feedback
        .begin_turn(&active.selection.model, &active.selection.effort);
    active.feedback.observe(&feedback_text());
}

#[tokio::test]
async fn typing_paste_clear_and_history_are_drafts_not_revocations_idle_or_streaming() {
    for streaming in [false, true] {
        let (mut app, runtime) = fixture(streaming);
        app.input_history = vec![
            "older submitted text".into(),
            "latest submitted text".into(),
        ];
        let authority = Authority::capture(&app);
        let original_history = app.input_history.clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let events = [
            (key(KeyCode::Char('x'), KeyModifiers::NONE), "x"),
            (
                Event::Paste(" pasted\r\nsecond line".into()),
                "x pasted\nsecond line",
            ),
            (key(KeyCode::Char('u'), KeyModifiers::CONTROL), ""),
            (
                key(KeyCode::Up, KeyModifiers::NONE),
                "latest submitted text",
            ),
            (key(KeyCode::Up, KeyModifiers::NONE), "older submitted text"),
            (
                key(KeyCode::Down, KeyModifiers::NONE),
                "latest submitted text",
            ),
            (key(KeyCode::Down, KeyModifiers::NONE), ""),
        ];
        for (event, expected_draft) in events {
            assert!(matches!(
                input_event(&mut app, &runtime, event),
                InputAction::None
            ));
            assert_eq!(app.input_text(), expected_draft);
            authority.assert_preserved(&app);
            assert!(queued(&app).is_empty());
            assert!(history_with_steering(&app).is_empty());
            assert_eq!(app.input_history, original_history);
            assert_eq!(app.streaming, streaming);
            assert!(app.transcript.is_empty());
            assert_no_send(&mut rx);
        }
        // Only an explicit Enter can make recalled/drafted text steering.
        let text = enter(&mut app, &runtime, "actually submitted");
        assert!(submit_steering(&mut app, &text, Some(&tx)));
        assert_eq!(queued(&app), ["actually submitted"]);
        authority.assert_preserved(&app);
        if streaming {
            assert_eq!(rx.try_recv().unwrap(), text);
        }
        assert_no_send(&mut rx);
        authority.assert_same_parent(&app);
    }
}

fn live_acknowledgements(messages: [&str; 2]) {
    let (mut app, runtime) = fixture(true);
    // A prior identical historical message must not de-duplicate a new submission.
    let base = vec![
        user(messages[0]),
        Arc::new(json!({"role":"assistant", "content":"prior answer"})),
    ];
    app.api_messages = base.clone();
    let authority = Authority::capture(&app);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    for message in messages {
        let input = enter(&mut app, &runtime, message);
        assert!(submit_steering(&mut app, &input, Some(&tx)));
        assert!(app.queued_message.is_none());
    }
    assert_eq!(queued(&app), messages);
    assert_eq!(
        app.api_messages, base,
        "send is not delivery acknowledgement"
    );
    assert!(
        displayed_users(&app).is_empty(),
        "unacknowledged text is only a notice"
    );
    steering_delivered(&mut app, "unrelated event-bus payload");
    if messages[0] != messages[1] {
        steering_delivered(&mut app, messages[1]);
    }
    assert_eq!(
        queued(&app),
        messages,
        "only the FIFO head may be acknowledged"
    );
    assert_eq!(
        app.api_messages, base,
        "non-head ACKs must not append history"
    );

    for (index, message) in messages.into_iter().enumerate() {
        assert_eq!(rx.try_recv().unwrap(), message);
        steering_delivered(&mut app, message);
        assert_eq!(queued(&app), messages[index + 1..]);
        let mut acknowledged = base.clone();
        acknowledged.extend(messages[..=index].iter().map(|message| user(message)));
        assert_eq!(
            app.api_messages, acknowledged,
            "ACK must retain the human before a snapshot arrives"
        );
        let mut expected = base.clone();
        expected.extend(messages.iter().map(|message| user(message)));
        assert_eq!(history_with_steering(&app), expected);
        // The authoritative MessageHistory REPLACES the provisional vector; it
        // does not merge or append. Avoid the disk-saving frontend handler.
        app.api_messages = acknowledged.clone();
        assert_eq!(app.api_messages, acknowledged);
        assert_eq!(
            history_with_steering(&app),
            expected,
            "snapshot replacement must not duplicate steering"
        );
        authority.assert_preserved(&app);
    }
    assert_no_send(&mut rx);
    steering_delivered(&mut app, messages[1]);
    assert!(queued(&app).is_empty());
    app.streaming = false;
    observe_terminal(&mut app, &runtime, Some(Terminal::Success));
    commit_submission(&mut app, "next authorized prompt");
    let mut expected = base;
    expected.extend(messages.iter().map(|message| user(message)));
    expected.push(user("next authorized prompt"));
    assert_eq!(app.api_messages, expected);
    assert_eq!(history_with_steering(&app), expected);
    assert!(
        displayed_users(&app).is_empty(),
        "commit must not redisplay delivered steering"
    );
    assert!(app.queued_message.is_none());
}

#[tokio::test]
async fn two_distinct_live_submissions_are_fifo_and_acknowledged_once() {
    live_acknowledgements(["first correction", "second correction"]);
}

#[tokio::test]
async fn two_identical_live_submissions_are_two_entries_not_a_single_slot() {
    live_acknowledgements(["same correction", "same correction"]);
}

#[tokio::test]
async fn ack_then_cancel_before_authoritative_history_retains_human_exactly_once() {
    for messages in [
        ["delivered correction", "unsent correction"],
        ["same correction", "same correction"],
    ] {
        let (mut app, runtime) = fixture(true);
        let base = vec![user("original task")];
        app.api_messages = base.clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        for message in messages {
            assert!(submit_steering(&mut app, message, Some(&tx)));
            assert_eq!(rx.try_recv().unwrap(), message);
        }
        steering_delivered(&mut app, messages[0]);
        let mut acknowledged = base;
        acknowledged.push(user(messages[0]));
        assert_eq!(app.api_messages, acknowledged);
        assert_eq!(queued(&app), [messages[1]]);
        app.set_input_text("unfinished draft");
        app.streaming = false;
        // Deliberately omit MessageHistory between ACK and cancellation.
        observe_terminal(&mut app, &runtime, Some(Terminal::Canceled));
        assert!(app.session_driver.active.is_none());
        assert_eq!(app.api_messages, acknowledged);
        assert_eq!(history_with_steering(&app), acknowledged);
        assert_eq!(
            app.input_text(),
            format!("{}\n\nunfinished draft", messages[1])
        );
        assert!(!auto_wakes_allowed(&app));
        assert!(app.queued_message.is_none());
        assert_no_send(&mut rx);
    }
}

#[tokio::test]
async fn channel_close_missing_sender_and_send_without_ack_retain_each_submission_for_next_turn() {
    for channel in ["closed", "missing", "sent-without-ack"] {
        for messages in [["first", "second"], ["identical", "identical"]] {
            for provider_error in [false, true] {
                let (mut app, runtime) = fixture(true);
                app.api_messages.push(user("original task"));
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let mut rx = Some(rx);
                if channel == "closed" {
                    drop(rx.take());
                }
                for message in messages {
                    let input = enter(&mut app, &runtime, message);
                    let sender = (channel != "missing").then_some(&tx);
                    assert!(submit_steering(&mut app, &input, sender));
                    assert!(app.queued_message.is_none());
                }
                if channel == "sent-without-ack" {
                    for message in messages {
                        assert_eq!(rx.as_mut().unwrap().try_recv().unwrap(), message);
                    }
                    assert_no_send(rx.as_mut().unwrap());
                } else if let Some(rx) = rx.as_mut() {
                    assert_no_send(rx);
                }
                // Reading the wire alone is NOT an acknowledgement. Normal
                // terminal history repair still lacks both steering messages.
                assert_eq!(queued(&app), messages);
                app.set_input_text("unfinished draft must stay local");
                app.streaming = false;
                let terminal = if provider_error {
                    Terminal::Failure(Outcome::ProviderError, "auth".into())
                } else {
                    Terminal::Success
                };
                observe_terminal(&mut app, &runtime, Some(terminal));
                let active = app.session_driver.active.as_ref().unwrap();
                assert!(!active.awaiting_terminal);
                assert_eq!(
                    active.outcome,
                    Some(if provider_error {
                        (Outcome::ProviderError, "auth".into())
                    } else {
                        (Outcome::Success, "none".into())
                    })
                );
                assert_eq!(queued(&app), messages);
                let mut expected = vec![user("original task")];
                expected.extend(messages.iter().map(|message| user(message)));
                for _ in 0..3 {
                    assert_eq!(
                        history_with_steering(&app),
                        expected,
                        "preflight is read-only"
                    );
                    assert_eq!(queued(&app), messages);
                    assert_eq!(app.api_messages, [user("original task")]);
                }
                commit_submission(&mut app, "next prompt");
                expected.push(user("next prompt"));
                assert_eq!(app.api_messages, expected);
                assert_eq!(history_with_steering(&app), expected);
                assert_eq!(displayed_users(&app), messages);
                assert!(queued(&app).is_empty());
                commit_submission(&mut app, "later prompt");
                expected.push(user("later prompt"));
                assert_eq!(
                    app.api_messages, expected,
                    "drained steering must not replay"
                );
                assert_eq!(displayed_users(&app), messages);
                assert_eq!(app.input_text(), "unfinished draft must stay local");
                assert!(app.queued_message.is_none());
            }
        }
    }
}

#[tokio::test]
async fn history_uses_latest_acknowledged_messages_and_never_drains_the_queue() {
    let (mut app, _) = fixture(false);
    app.api_messages = vec![user("old context")];
    assert!(submit_steering(&mut app, "explicit guidance", None));
    let before_preparation = history_with_steering(&app);
    let latest = vec![
        user("checkpoint summary"),
        Arc::new(json!({"role":"assistant", "content":[{"type":"text", "text":"latest answer"}]})),
    ];
    app.api_messages = latest.clone();
    app.set_input_text("private draft, not submitted");
    let latest_preflight = history_with_steering(&app);
    assert_ne!(latest_preflight, before_preparation);
    assert!(Arc::ptr_eq(&latest_preflight[0], &latest[0]));
    assert!(Arc::ptr_eq(&latest_preflight[1], &latest[1]));
    assert_eq!(
        latest_preflight[2].as_ref(),
        &json!({"role":"user", "content":"explicit guidance"})
    );
    assert_eq!(latest_preflight.len(), 3);
    assert_eq!(app.api_messages, latest);
    assert_eq!(queued(&app), ["explicit guidance"]);
    assert_eq!(history_with_steering(&app), latest_preflight);
    commit_submission(&mut app, "authorized prompt");
    let mut expected = latest_preflight;
    expected.push(user("authorized prompt"));
    assert_eq!(app.api_messages, expected);
    assert_eq!(app.input_text(), "private draft, not submitted");
}

#[tokio::test]
async fn commit_preserves_draft_paste_and_staged_attachment_bytes_and_consumes_only_abort_context()
{
    let (mut app, _) = fixture(false);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    assert!(submit_steering(
        &mut app,
        "submitted before attachment staging",
        Some(&tx)
    ));
    assert_no_send(&mut rx); // An idle sender is never used for a new turn.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("draft-only.txt");
    std::fs::write(&path, "attachment bytes must remain local").unwrap();
    app.pending_attachments
        .add(
            agent_engine::attachments::load_attachment(&path)
                .await
                .unwrap(),
        )
        .unwrap();
    let attachments = app.pending_attachments.build_content("");
    let summaries = app.pending_attachments.summaries();
    app.set_input_text("unfinished multiline\ndraft");
    app.input_before_paste = Some("unfinished multiline".into());
    app.pasted_char_count = 6;
    app.abort_context = Some("retained abort context".into());
    let authority = Authority::capture(&app);
    assert_eq!(
        history_with_steering(&app),
        [user("submitted before attachment staging")]
    );
    commit_submission(&mut app, "authorized prompt");
    assert_eq!(
        app.api_messages,
        [
            user("submitted before attachment staging"),
            user("retained abort context\n\nauthorized prompt")
        ]
    );
    assert!(app.abort_context.is_none());
    assert!(queued(&app).is_empty());
    assert_eq!(app.pending_attachments.len(), 1);
    assert_eq!(app.pending_attachments.build_content(""), attachments);
    assert_eq!(app.pending_attachments.summaries(), summaries);
    assert_eq!(app.input_text(), "unfinished multiline\ndraft");
    assert_eq!(
        app.input_before_paste.as_deref(),
        Some("unfinished multiline")
    );
    assert_eq!(app.pasted_char_count, 6);
    assert_eq!(
        displayed_users(&app),
        ["submitted before attachment staging"]
    );
    authority.assert_preserved(&app);
    assert_no_send(&mut rx);
    commit_submission(&mut app, "second authorized prompt");
    assert_eq!(
        app.api_messages.last().unwrap().as_ref(),
        user("second authorized prompt").as_ref()
    );
    assert_eq!(app.pending_attachments.build_content(""), attachments);
    assert_eq!(app.input_text(), "unfinished multiline\ndraft");
}

#[tokio::test]
async fn attachment_rejection_retains_input_and_does_not_send_clear_feedback_or_revoke() {
    for streaming in [false, true] {
        let (mut app, _) = fixture(streaming);
        assert!(submit_steering(&mut app, "already queued", None));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending.txt");
        std::fs::write(&path, "not authorized for an automatic turn").unwrap();
        app.pending_attachments
            .add(
                agent_engine::attachments::load_attachment(&path)
                    .await
                    .unwrap(),
            )
            .unwrap();
        let attachments = app.pending_attachments.build_content("");
        prime_repeated_feedback(&mut app);
        let authority = Authority::capture(&app);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // Idle Enter would already have cleared the input; the streaming input
        // adapter rejects earlier. Exercise the helper's defensive gate in both.
        assert!(submit_steering(
            &mut app,
            "text accompanying attachment",
            Some(&tx)
        ));
        assert_eq!(app.input_text(), "text accompanying attachment");
        assert_eq!(queued(&app), ["already queued"]);
        assert_eq!(app.pending_attachments.build_content(""), attachments);
        assert_eq!(
            app.session_driver
                .active
                .as_ref()
                .unwrap()
                .completed_feedback,
            "repeated"
        );
        assert_eq!(
            app.session_driver
                .active
                .as_mut()
                .unwrap()
                .feedback
                .finish(),
            "repeated"
        );
        authority.assert_preserved(&app);
        assert_no_send(&mut rx);
        assert!(matches!(
            &app.transcript.messages().last().unwrap().msg,
            ChatMessage::Error(_)
        ));
    }
}

#[tokio::test]
async fn rejected_queue_count_limit_retains_entered_input_and_original_authority() {
    for streaming in [false, true] {
        let (mut app, runtime) = fixture(streaming);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let expected: Vec<_> = (0..16).map(|index| format!("guidance {index}")).collect();
        for message in &expected {
            let input = enter(&mut app, &runtime, message);
            assert!(submit_steering(&mut app, &input, Some(&tx)));
        }
        prime_repeated_feedback(&mut app);
        let authority = Authority::capture(&app);
        let input = enter(&mut app, &runtime, "seventeenth must be retained");
        assert!(submit_steering(&mut app, &input, Some(&tx)));
        assert_eq!(app.input_text(), input);
        assert_eq!(queued(&app), expected);
        assert!(app.api_messages.is_empty());
        assert_eq!(
            app.session_driver
                .active
                .as_ref()
                .unwrap()
                .completed_feedback,
            "repeated"
        );
        assert_eq!(
            app.session_driver
                .active
                .as_mut()
                .unwrap()
                .feedback
                .finish(),
            "repeated"
        );
        authority.assert_preserved(&app);
        if streaming {
            for message in &expected {
                assert_eq!(rx.try_recv().unwrap(), *message);
            }
        }
        assert_no_send(&mut rx);
        assert!(matches!(
            &app.transcript.messages().last().unwrap().msg,
            ChatMessage::Error(_)
        ));
        authority.assert_same_parent(&app);
    }
}

#[tokio::test]
async fn byte_limit_counts_utf8_bytes_and_rejected_input_is_not_lost_or_duplicated() {
    const MAX_BYTES: usize = 256 * 1024;
    for aggregate in [false, true] {
        let (mut app, runtime) = fixture(true);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let exact_limit = "é".repeat(MAX_BYTES / 2);
        assert_eq!(exact_limit.len(), MAX_BYTES);
        let rejected = if aggregate {
            assert!(submit_steering(&mut app, &exact_limit, Some(&tx)));
            assert_eq!(rx.try_recv().unwrap(), exact_limit);
            "é".to_owned()
        } else {
            format!("{exact_limit}é")
        };
        let before = queued(&app);
        prime_repeated_feedback(&mut app);
        let authority = Authority::capture(&app);
        let input = enter(&mut app, &runtime, &rejected);
        assert!(submit_steering(&mut app, &input, Some(&tx)));
        assert_eq!(app.input_text(), rejected);
        // A defensive retry while the same draft is present must not prepend it.
        assert!(submit_steering(&mut app, &input, Some(&tx)));
        assert_eq!(app.input_text(), rejected);
        assert_eq!(queued(&app), before);
        assert_eq!(
            app.session_driver
                .active
                .as_ref()
                .unwrap()
                .completed_feedback,
            "repeated"
        );
        assert_eq!(
            app.session_driver
                .active
                .as_mut()
                .unwrap()
                .feedback
                .finish(),
            "repeated"
        );
        assert!(app.api_messages.is_empty());
        authority.assert_preserved(&app);
        assert_no_send(&mut rx);
        authority.assert_same_parent(&app);
    }
}

#[tokio::test]
async fn accepted_steering_resets_feedback_but_keeps_pending_poll_and_preparation_alive() {
    for preparing in [false, true] {
        for streaming in [false, true] {
            let (mut app, runtime) = fixture(streaming);
            prime_repeated_feedback(&mut app);
            let (release, waiting) = tokio::sync::oneshot::channel::<()>();
            let (entered, started) = tokio::sync::oneshot::channel();
            app.session_driver
                .spawn(app.session.id.clone(), async move {
                    entered.send(()).unwrap();
                    waiting.await.expect("test controls pending completion");
                    if preparing {
                        TaskResult::Prepared {
                            proposal: proposal(),
                            // Do not construct/prepare a candidate or invoke a provider.
                            result: Box::new(Err(PrepareError::Selection(
                                "fixture rejection".into(),
                            ))),
                        }
                    } else {
                        TaskResult::Poll(Ok(Reply::Next {
                            run_id: "steering-test-run".into(),
                            selection: proposal().selection,
                            prompt: proposal().prompt,
                            delay_ms: 300_000,
                            notice: String::new(),
                        }))
                    }
                });
            tokio::time::timeout(Duration::from_secs(5), started)
                .await
                .unwrap()
                .unwrap();
            let authority = Authority::capture(&app);
            assert!(matches!(
                input_event(
                    &mut app,
                    &runtime,
                    Event::Paste("draft while pending".into())
                ),
                InputAction::None
            ));
            authority.assert_preserved(&app);
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let submitted = enter(&mut app, &runtime, "new human guidance");
            // Idle Enter does not reset paste metadata itself; steering must.
            app.input_before_paste = Some("pre-paste draft".into());
            app.pasted_char_count = 27;
            assert!(submit_steering(&mut app, &submitted, Some(&tx)));
            assert_eq!(queued(&app), ["new human guidance"]);
            assert!(app.input_before_paste.is_none());
            assert_eq!(app.pasted_char_count, 0);
            authority.assert_preserved(&app);
            if streaming {
                assert_eq!(rx.try_recv().unwrap(), submitted);
            }
            assert_no_send(&mut rx);
            let active = app.session_driver.active.as_mut().unwrap();
            assert_eq!(active.completed_feedback, "unknown");
            assert_eq!(
                poll_request(active, Outcome::Success, "none".into())
                    .feedback
                    .as_deref(),
                Some("unknown")
            );
            active.feedback.observe(&feedback_text());
            assert_eq!(
                active.feedback.finish(),
                "unknown",
                "mixed-purpose turn is not comparable"
            );
            for expected in ["changed", "repeated"] {
                active
                    .feedback
                    .begin_turn(&active.selection.model, &active.selection.effort);
                active.feedback.observe(&feedback_text());
                assert_eq!(
                    active.feedback.finish(),
                    expected,
                    "old signatures must be forgotten"
                );
            }

            // Handshake, not a sleep: a dropped/aborted Pending closes waiting.
            release.send(()).expect("steering aborted the pending task");
            let mut pending = app.session_driver.pending.take().unwrap();
            assert_eq!(pending.generation, authority.generation);
            assert_eq!(pending.task.0.id(), authority.pending_task.unwrap());
            let result = tokio::time::timeout(Duration::from_secs(5), &mut pending.task.0)
                .await
                .unwrap()
                .expect("pending work was canceled");
            match result {
                TaskResult::Prepared {
                    result,
                    proposal: p,
                } if preparing => {
                    assert_eq!(p.selection, authority.selection);
                    assert!(matches!(*result, Err(PrepareError::Selection(_))));
                }
                TaskResult::Poll(Ok(Reply::Next {
                    run_id, selection, ..
                })) if !preparing => {
                    assert_eq!(run_id, authority.run_id);
                    assert_eq!(selection, authority.selection);
                }
                _ => panic!("pending result changed after steering"),
            }
            authority.assert_same_parent(&app);
        }
    }
}

#[tokio::test]
async fn blocked_terminal_restores_only_unsent_fifo_before_draft_and_never_auto_wakes() {
    for eof in [false, true] {
        let (mut app, runtime) = fixture(true);
        let parent = app.session_driver.active.as_ref().unwrap().cancel.clone();
        let generation = app.session_driver.generation;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        for text in [
            "already acknowledged",
            "unsent first",
            "unsent duplicate",
            "unsent duplicate",
        ] {
            assert!(submit_steering(&mut app, text, Some(&tx)));
            assert_eq!(rx.try_recv().unwrap(), text);
        }
        // No MessageHistory arrives before this blocked/EOF terminal.
        steering_delivered(&mut app, "already acknowledged");
        app.set_input_text("unfinished draft");
        app.streaming = false;
        let terminal = if eof {
            capture_terminal(None, false)
        } else {
            Some(Terminal::Failure(Outcome::Blocked, "unknown".into()))
        };
        observe_terminal(&mut app, &runtime, terminal);
        let restored = "unsent first\n\nunsent duplicate\n\nunsent duplicate\n\nunfinished draft";
        assert_eq!(app.input_text(), restored);
        assert_eq!(app.api_messages, [user("already acknowledged")]);
        assert_eq!(history_with_steering(&app), app.api_messages);
        assert!(app.session_driver.active.is_none());
        assert!(app.session_driver.pending.is_none());
        assert_eq!(app.session_driver.generation, generation.wrapping_add(1));
        assert_eq!(app.session_driver.owner(), Some("steering-fixture"));
        assert!(parent.is_cancelled());
        assert!(!auto_wakes_allowed(&app));
        assert!(app.queued_message.is_none());
        assert_no_send(&mut rx);
        assert!(displayed_users(&app).is_empty());
        let notices = app.transcript.messages().len();
        // Late acknowledgements and duplicate terminal events cannot consume a
        // restored draft, resurrect authority, or append it to history.
        steering_delivered(&mut app, "unsent first");
        observe_terminal(&mut app, &runtime, Some(Terminal::Success));
        observe_terminal(&mut app, &runtime, capture_terminal(None, false));
        assert_eq!(app.input_text(), restored);
        assert_eq!(app.transcript.messages().len(), notices);
        assert_eq!(app.api_messages, [user("already acknowledged")]);
        assert!(matches!(
            input_event(
                &mut app,
                &runtime,
                key(KeyCode::Char('!'), KeyModifiers::NONE)
            ),
            InputAction::None
        ));
        assert_eq!(app.input_text(), format!("{restored}!"));
        assert!(
            !auto_wakes_allowed(&app),
            "editing is not explicit takeover"
        );
        assert_no_send(&mut rx);
    }
}

#[tokio::test]
async fn unarmed_helper_declines_and_blank_steering_does_not_reset_feedback() {
    let (mut app, _) = fixture(false);
    prime_repeated_feedback(&mut app);
    let authority = Authority::capture(&app);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.set_input_text("still a draft");
    assert!(submit_steering(&mut app, " \n\t", Some(&tx)));
    assert!(queued(&app).is_empty());
    assert_eq!(app.input_text(), "still a draft");
    assert_eq!(
        app.session_driver
            .active
            .as_ref()
            .unwrap()
            .completed_feedback,
        "repeated"
    );
    authority.assert_preserved(&app);
    revoke(&mut app, "fixture explicit stop");
    let before = values(&app.api_messages);
    let notices = app.transcript.messages().len();
    assert!(!submit_steering(
        &mut app,
        "ordinary user submission",
        Some(&tx)
    ));
    assert_eq!(values(&app.api_messages), before);
    assert_eq!(app.input_text(), "still a draft");
    assert_eq!(app.transcript.messages().len(), notices);
    assert!(!auto_wakes_allowed(&app));
    assert!(app.queued_message.is_none());
    assert_no_send(&mut rx);
}
