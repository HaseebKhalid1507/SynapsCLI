//! The event loop shared by reactive workers (`subagent_start`, `subagent_resume`).
//!
//! It used to be copied into both tools. One copy now also:
//! - keeps the worker's real message history (every `MessageHistory` event), so
//!   the finalizer can archive it and `subagent_resume` can continue it;
//! - tracks where the latest model response starts, so completion previews and
//!   timeout reports lead with the worker's latest word, not its first;
//! - on timeout, cancels the worker and drains its stream briefly, because the
//!   runtime only sends the final history when the turn ends (cancel included);
//! - treats `timeout_secs == 0` as "no time limit".

use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use futures::{Stream, StreamExt};

use crate::runtime::subagent::{SubagentResult, SubagentState, SubagentStatus};
use crate::{AgentEvent, LlmEvent, SessionEvent, StreamEvent};

/// After a timeout the worker is cancelled and its stream drained at most this
/// long, so the runtime's final `MessageHistory` reaches the archive.
const CANCEL_DRAIN: Duration = Duration::from_secs(30);

pub(super) type WorkerStream = Pin<Box<dyn Stream<Item = StreamEvent> + Send>>;

pub(super) struct WorkerDrive {
    pub state: Arc<RwLock<SubagentState>>,
    pub tx_events: Option<tokio::sync::mpsc::UnboundedSender<StreamEvent>>,
    pub subagent_id: u64,
    pub label: String,
    pub model: String,
    /// Wall-clock limit in seconds; 0 = none.
    pub timeout_secs: u64,
    /// The worker's cancellation token (cancelled on timeout).
    pub cancel: crate::CancellationToken,
    /// The text that started this run, for the legacy `conversation_state`.
    pub task: String,
}

#[derive(Default)]
struct Totals {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
    // TTL split: None only if no turn ever reported one; otherwise summed.
    cache_5m: Option<u64>,
    cache_1h: Option<u64>,
}

/// Per-response bookkeeping (a provider round can be reset and retried).
#[derive(Default)]
struct ResponseMarks {
    /// (partial_text len, tool_log len, tool_count) when the response started.
    baseline: (usize, usize, u32),
    /// `last_response_start` before this response, restored on reset.
    prev_last_start: usize,
    /// Set at ResponseStart; consumed by the response's first text.
    pending: bool,
}

enum Flow {
    Continue,
    Done,
    Failed(String),
}

impl WorkerDrive {
    pub async fn run(self, mut stream: WorkerStream) -> Result<SubagentResult, String> {
        let mut totals = Totals::default();
        let mut tool_count = 0u32;
        let mut marks = ResponseMarks::default();
        let mut deadline = (self.timeout_secs > 0)
            .then(|| Box::pin(tokio::time::sleep(Duration::from_secs(self.timeout_secs))));

        loop {
            tokio::select! {
                event = stream.next() => {
                    let Some(event) = event else { break };
                    match self.on_event(event, &mut tool_count, &mut marks, &mut totals) {
                        Flow::Continue => {}
                        Flow::Done => break,
                        Flow::Failed(reason) => return Err(reason),
                    }
                }
                _ = async {
                    match deadline.as_mut() {
                        Some(sleep) => sleep.as_mut().await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    return Ok(self.time_out(stream, tool_count, totals).await);
                }
            }
        }

        let text = self
            .state
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .partial_text
            .clone();
        Ok(self.result(text, tool_count, totals, false))
    }

    fn on_event(
        &self,
        event: StreamEvent,
        tool_count: &mut u32,
        marks: &mut ResponseMarks,
        totals: &mut Totals,
    ) -> Flow {
        match event {
            StreamEvent::Llm(LlmEvent::Thinking(_)) => {
                self.update("💭 thinking...".to_string(), *tool_count)
            }
            StreamEvent::Llm(LlmEvent::ResponseStart) => {
                let s = self.state.read().unwrap_or_else(|p| p.into_inner());
                marks.baseline = (s.partial_text.len(), s.tool_log.len(), *tool_count);
                marks.prev_last_start = s.last_response_start;
                marks.pending = true;
            }
            StreamEvent::Llm(LlmEvent::ResponseReset) => {
                let mut s = self.state.write().unwrap_or_else(|p| p.into_inner());
                s.partial_text.truncate(marks.baseline.0);
                s.tool_log.truncate(marks.baseline.1);
                s.last_response_start = marks.prev_last_start.min(s.partial_text.len());
                *tool_count = marks.baseline.2;
                s.tools = *tool_count;
                marks.pending = true;
            }
            StreamEvent::Llm(LlmEvent::Text(text)) => {
                let mut s = self.state.write().unwrap_or_else(|p| p.into_inner());
                if std::mem::take(&mut marks.pending) && !text.is_empty() {
                    // Separate this response from the previous one's narration.
                    if !s.partial_text.is_empty() && !s.partial_text.ends_with('\n') {
                        s.partial_text.push_str("\n\n");
                    }
                    s.last_response_start = s.partial_text.len();
                } else if text.is_empty() {
                    marks.pending |= s.partial_text.len() == marks.baseline.0;
                }
                s.partial_text.push_str(&text);
            }
            StreamEvent::Llm(LlmEvent::ToolUseStart { tool_name, .. }) => {
                *tool_count += 1;
                self.update(format!("⚙ {} (tool #{})", tool_name, tool_count), *tool_count);
            }
            StreamEvent::Llm(LlmEvent::ToolUse {
                tool_name, input, ..
            }) => {
                let input_preview: String = input.to_string().chars().take(200).collect();
                self.state
                    .write()
                    .unwrap_or_else(|p| p.into_inner())
                    .tool_log
                    .push(format!("[tool_use]: {} — {}", tool_name, input_preview));
                self.update(tool_detail(&tool_name, &input), *tool_count);
            }
            StreamEvent::Llm(LlmEvent::ToolResult { result, .. }) => {
                let preview: String = result.chars().take(300).collect();
                self.state
                    .write()
                    .unwrap_or_else(|p| p.into_inner())
                    .tool_log
                    .push(format!("[tool_result]: {}", preview));
            }
            StreamEvent::Session(SessionEvent::MessageHistory(history)) => {
                self.state
                    .write()
                    .unwrap_or_else(|p| p.into_inner())
                    .history = Some(history);
            }
            StreamEvent::Session(SessionEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                cache_creation_input_tokens,
                cache_creation_5m,
                cache_creation_1h,
                model: _,
            }) => {
                totals.input += input_tokens;
                totals.output += output_tokens;
                totals.cache_read += cache_read_input_tokens;
                totals.cache_creation += cache_creation_input_tokens;
                crate::core::rpc_dispatch::merge_split(&mut totals.cache_5m, cache_creation_5m);
                crate::core::rpc_dispatch::merge_split(&mut totals.cache_1h, cache_creation_1h);
            }
            StreamEvent::Session(SessionEvent::Error(e)) => {
                return Flow::Failed(format!("provider request failed [{}]", e.category_label()))
            }
            StreamEvent::Session(SessionEvent::Done) => return Flow::Done,
            _ => {}
        }
        Flow::Continue
    }

    /// Stop a worker that ran out of time: cancel it, collect its final history,
    /// and replace the output with a report that leads with its latest progress.
    async fn time_out(
        &self,
        mut stream: WorkerStream,
        tool_count: u32,
        totals: Totals,
    ) -> SubagentResult {
        self.state.write().unwrap_or_else(|p| p.into_inner()).status = SubagentStatus::TimedOut;
        self.cancel.cancel();
        let _ = tokio::time::timeout(CANCEL_DRAIN, async {
            while let Some(event) = stream.next().await {
                if let StreamEvent::Session(SessionEvent::MessageHistory(history)) = event {
                    self.state
                        .write()
                        .unwrap_or_else(|p| p.into_inner())
                        .history = Some(history);
                }
            }
        })
        .await;
        drop(stream);

        let text = {
            let mut s = self.state.write().unwrap_or_else(|p| p.into_inner());
            let report = super::archive::timeout_report(
                self.timeout_secs,
                tool_count,
                &s,
                s.history.is_some(),
            );
            s.conversation_state = vec![
                serde_json::json!({"role": "user", "content": self.task.clone()}),
                serde_json::json!({"role": "assistant", "content": s.partial_text.clone()}),
            ];
            s.partial_text = report.clone();
            s.last_response_start = 0;
            report
        };
        self.result(text, tool_count, totals, true)
    }

    fn result(&self, text: String, tool_count: u32, t: Totals, timed_out: bool) -> SubagentResult {
        SubagentResult {
            text,
            model: self.model.clone(),
            input_tokens: t.input,
            output_tokens: t.output,
            cache_read: t.cache_read,
            cache_creation: t.cache_creation,
            cache_creation_5m: t.cache_5m,
            cache_creation_1h: t.cache_1h,
            tool_count,
            timed_out,
        }
    }

    /// Publish the worker's progress: kept in its state (clients read it once
    /// the turn that started the worker has ended) and sent as a live update.
    fn update(&self, status: String, tools: u32) {
        self.state
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .note_progress(&status, tools);
        if let Some(tx) = &self.tx_events {
            let _ = tx.send(StreamEvent::Agent(AgentEvent::SubagentUpdate {
                subagent_id: self.subagent_id,
                agent_name: self.label.clone(),
                status,
            }));
        }
    }
}

/// One-line status for the TUI's subagent panel.
fn tool_detail(tool_name: &str, input: &serde_json::Value) -> String {
    let base = |key: &str, default: &str| {
        input[key]
            .as_str()
            .unwrap_or(default)
            .rsplit('/')
            .next()
            .unwrap_or(default)
            .to_string()
    };
    match tool_name {
        "bash" => {
            let preview: String = input["command"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(60)
                .collect();
            format!("$ {}", preview)
        }
        "read" => format!("reading {}", base("path", "?")),
        "write" => format!("writing {}", base("path", "?")),
        "edit" => format!("editing {}", base("path", "?")),
        "grep" => format!(
            "grep /{}/",
            input["pattern"]
                .as_str()
                .unwrap_or("?")
                .chars()
                .take(30)
                .collect::<String>()
        ),
        "find" => format!("find {}", input["pattern"].as_str().unwrap_or("?")),
        "ls" => format!("ls {}", base("path", ".")),
        other if other.starts_with("ext__") => {
            other.splitn(3, "__").last().unwrap_or(other).to_string()
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn drive(
        timeout_secs: u64,
    ) -> (
        WorkerDrive,
        Arc<RwLock<SubagentState>>,
        crate::CancellationToken,
    ) {
        let state = Arc::new(RwLock::new(SubagentState::new()));
        let cancel = crate::CancellationToken::new();
        (
            WorkerDrive {
                state: Arc::clone(&state),
                tx_events: None,
                subagent_id: 1,
                label: "inline".into(),
                model: "m".into(),
                timeout_secs,
                cancel: cancel.clone(),
                task: "the task".into(),
            },
            state,
            cancel,
        )
    }

    fn text(t: &str) -> StreamEvent {
        StreamEvent::Llm(LlmEvent::Text(t.into()))
    }

    fn history(n: usize) -> StreamEvent {
        StreamEvent::Session(SessionEvent::MessageHistory(
            (0..n)
                .map(|i| Arc::new(json!({"role": "user", "content": format!("m{i}")})))
                .collect(),
        ))
    }

    #[tokio::test]
    async fn completes_with_history_and_separated_responses() {
        let (d, state, _) = drive(60);
        let events = vec![
            StreamEvent::Llm(LlmEvent::ResponseStart),
            text("Reading the files."),
            StreamEvent::Llm(LlmEvent::ResponseStart),
            text("All done: "),
            text("plate renders."),
            history(4),
            StreamEvent::Session(SessionEvent::Done),
        ];
        let r = d
            .run(Box::pin(futures::stream::iter(events)))
            .await
            .unwrap();
        assert!(!r.timed_out);
        assert_eq!(r.text, "Reading the files.\n\nAll done: plate renders.");
        let s = state.read().unwrap();
        assert_eq!(s.last_response_text(), "All done: plate renders.");
        assert_eq!(s.history.as_ref().map(Vec::len), Some(4));
    }

    #[tokio::test]
    async fn progress_is_kept_in_state_for_background_clients() {
        let (d, state, _) = drive(60);
        let events = vec![
            StreamEvent::Llm(LlmEvent::ResponseStart),
            StreamEvent::Llm(LlmEvent::ToolUseStart {
                tool_name: "bash".into(),
                tool_id: "t1".into(),
            }),
            StreamEvent::Llm(LlmEvent::ToolUse {
                tool_name: "bash".into(),
                tool_id: "t1".into(),
                input: serde_json::json!({"command": "cargo test"}),
            }),
            StreamEvent::Llm(LlmEvent::ResponseStart),
            StreamEvent::Llm(LlmEvent::ToolUseStart {
                tool_name: "read".into(),
                tool_id: "t2".into(),
            }),
            StreamEvent::Llm(LlmEvent::ResponseReset),
            StreamEvent::Session(SessionEvent::Done),
        ];
        d.run(Box::pin(futures::stream::iter(events)))
            .await
            .unwrap();
        let s = state.read().unwrap();
        // The retried round's tool start is rolled back; the bash call stays.
        assert_eq!(s.tools, 1);
        assert_eq!(s.step, "⚙ read (tool #2)", "last published step is kept");
    }

    #[tokio::test]
    async fn reset_restores_the_previous_response_mark() {
        let (d, state, _) = drive(60);
        let events = vec![
            StreamEvent::Llm(LlmEvent::ResponseStart),
            text("First."),
            StreamEvent::Llm(LlmEvent::ResponseStart),
            text("Half a resp"),
            StreamEvent::Llm(LlmEvent::ResponseReset),
            text("Retried response."),
            StreamEvent::Session(SessionEvent::Done),
        ];
        let r = d
            .run(Box::pin(futures::stream::iter(events)))
            .await
            .unwrap();
        assert_eq!(r.text, "First.\n\nRetried response.");
        assert_eq!(
            state.read().unwrap().last_response_text(),
            "Retried response."
        );
    }

    #[tokio::test]
    async fn tool_only_rounds_keep_the_last_spoken_response() {
        let (d, state, _) = drive(60);
        let events = vec![
            StreamEvent::Llm(LlmEvent::ResponseStart),
            text("Rendering now."),
            StreamEvent::Llm(LlmEvent::ResponseStart),
            StreamEvent::Session(SessionEvent::Done),
        ];
        d.run(Box::pin(futures::stream::iter(events)))
            .await
            .unwrap();
        assert_eq!(state.read().unwrap().last_response_text(), "Rendering now.");
    }

    #[tokio::test]
    async fn error_keeps_history_sent_before_it() {
        let (d, state, _) = drive(60);
        let events = vec![
            history(3),
            StreamEvent::Session(SessionEvent::Error(
                agent_core::TurnError::interrupted_after_side_effect("c1"),
            )),
        ];
        let err = d
            .run(Box::pin(futures::stream::iter(events)))
            .await
            .unwrap_err();
        assert!(err.starts_with("provider request failed"));
        assert_eq!(
            state.read().unwrap().history.as_ref().map(Vec::len),
            Some(3)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_cancels_then_collects_the_final_history() {
        let (d, state, cancel) = drive(5);
        // A worker that talks, then hangs until cancelled, then (like the
        // runtime's cancel path) sends its history and ends.
        let stream = futures::stream::iter(vec![
            StreamEvent::Llm(LlmEvent::ResponseStart),
            text("Rendering pass 2."),
        ])
        .chain(futures::stream::once(async move {
            cancel.cancelled().await;
            history(7)
        }));
        let r = d.run(Box::pin(stream)).await.unwrap();
        assert!(r.timed_out);
        assert!(r.text.starts_with("[TIMED OUT after 5s"), "{}", r.text);
        assert!(r.text.contains("Rendering pass 2."));
        let s = state.read().unwrap();
        assert_eq!(s.status, SubagentStatus::TimedOut);
        assert_eq!(s.history.as_ref().map(Vec::len), Some(7));
        assert_eq!(s.partial_text, r.text);
    }

    #[tokio::test(start_paused = true)]
    async fn zero_timeout_never_times_out() {
        let (d, _, _) = drive(0);
        let stream = futures::stream::once(async {
            tokio::time::sleep(Duration::from_secs(365 * 24 * 3600)).await;
            text("still here")
        })
        .chain(futures::stream::iter(vec![StreamEvent::Session(
            SessionEvent::Done,
        )]));
        let r = d.run(Box::pin(stream)).await.unwrap();
        assert!(!r.timed_out);
        assert_eq!(r.text, "still here");
    }
}
