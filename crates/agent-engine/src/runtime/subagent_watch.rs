//! Watch journal: what a running subagent has shown so far, for clients that
//! watch it in its own thread view.
//!
//! A worker's real message history only reaches [`SubagentState`] when its
//! turn ends, so a live view can't be built from it. Instead the worker's
//! event loop (`tools/subagent/drive.rs`) records every presentation event
//! here, and clients page through it with `SessionQuery::SubagentWatch`
//! (`since` = the `next_seq` of their previous page). The first page is the
//! whole run so far; later pages are just what is new.
//!
//! Bounded on purpose:
//! - consecutive text / thinking / tool-output deltas coalesce into one entry
//!   — but only into an entry no reader has seen yet (`sealed`), so a client
//!   paging by `seq` never misses an append;
//! - large tool inputs and results are capped per entry;
//! - past [`JOURNAL_MAX_BYTES`] the oldest entries drop and readers asking
//!   for them get `truncated: true`.
//!
//! Raw provider error text never enters the journal (same rule as
//! [`SubagentState::safe_fail`]): errors carry only their category label,
//! and retry notices are not recorded.
//!
//! [`SubagentState`]: super::subagent::SubagentState
//! [`SubagentState::safe_fail`]: super::subagent::SubagentState::safe_fail

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{AgentEvent, LlmEvent, SessionEvent, StreamEvent};

/// Total retained bytes per worker before the oldest entries drop.
pub const JOURNAL_MAX_BYTES: usize = 2 * 1024 * 1024;
/// Most bytes of events one page returns; the client pages on from `next_seq`.
pub const PAGE_MAX_BYTES: usize = 1024 * 1024;
/// Cap on one tool input (serialized) or one tool result.
pub const TOOL_PAYLOAD_MAX_BYTES: usize = 64 * 1024;

/// One presentation event of a worker, in the shape the TUI's transcript
/// applies (`stream_handler::apply_transcript_event`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WatchEvent {
    ResponseStart,
    ResponseReset,
    Thinking {
        text: String,
    },
    Text {
        text: String,
    },
    ToolUseStart {
        tool_id: String,
        tool_name: String,
    },
    ToolUse {
        tool_id: String,
        tool_name: String,
        input: Value,
    },
    ToolResultDelta {
        tool_id: String,
        delta: String,
    },
    ToolResult {
        tool_id: String,
        result: String,
    },
    /// A steering message the worker took in (from the user or its parent).
    Steered {
        text: String,
    },
    Done,
    /// Terminal failure: the category label only, never the raw message.
    Failed {
        category: String,
    },
}

impl WatchEvent {
    /// The journal entry for a worker stream event, if it is one the
    /// watch view shows.
    pub fn from_stream(event: &StreamEvent) -> Option<Self> {
        Some(match event {
            StreamEvent::Llm(LlmEvent::ResponseStart) => Self::ResponseStart,
            StreamEvent::Llm(LlmEvent::ResponseReset) => Self::ResponseReset,
            StreamEvent::Llm(LlmEvent::Thinking(text)) => Self::Thinking { text: text.clone() },
            StreamEvent::Llm(LlmEvent::Text(text)) => Self::Text { text: text.clone() },
            StreamEvent::Llm(LlmEvent::ToolUseStart { tool_name, tool_id }) => Self::ToolUseStart {
                tool_id: tool_id.clone(),
                tool_name: tool_name.clone(),
            },
            StreamEvent::Llm(LlmEvent::ToolUse {
                tool_name,
                tool_id,
                input,
            }) => Self::ToolUse {
                tool_id: tool_id.clone(),
                tool_name: tool_name.clone(),
                input: cap_input(input),
            },
            StreamEvent::Llm(LlmEvent::ToolResultDelta { tool_id, delta }) => {
                Self::ToolResultDelta {
                    tool_id: tool_id.clone(),
                    delta: delta.clone(),
                }
            }
            StreamEvent::Llm(LlmEvent::ToolResult { tool_id, result }) => Self::ToolResult {
                tool_id: tool_id.clone(),
                result: cap_text(result, TOOL_PAYLOAD_MAX_BYTES),
            },
            StreamEvent::Agent(AgentEvent::SteeringDelivered { message }) => Self::Steered {
                text: message.clone(),
            },
            StreamEvent::Session(SessionEvent::Done) => Self::Done,
            StreamEvent::Session(SessionEvent::Error(err)) => Self::Failed {
                category: err.category_label(),
            },
            // Input-JSON deltas are superseded by `ToolUse`; notices can echo
            // provider error bodies; the rest is bookkeeping.
            _ => return None,
        })
    }

    /// The stream event a client replays to render this entry through the
    /// same transcript path as the main conversation. `Failed` has none:
    /// clients render its category themselves.
    pub fn to_stream(&self) -> Option<StreamEvent> {
        Some(match self {
            Self::ResponseStart => StreamEvent::Llm(LlmEvent::ResponseStart),
            Self::ResponseReset => StreamEvent::Llm(LlmEvent::ResponseReset),
            Self::Thinking { text } => StreamEvent::Llm(LlmEvent::Thinking(text.clone())),
            Self::Text { text } => StreamEvent::Llm(LlmEvent::Text(text.clone())),
            Self::ToolUseStart { tool_id, tool_name } => StreamEvent::Llm(LlmEvent::ToolUseStart {
                tool_name: tool_name.clone(),
                tool_id: tool_id.clone(),
            }),
            Self::ToolUse {
                tool_id,
                tool_name,
                input,
            } => StreamEvent::Llm(LlmEvent::ToolUse {
                tool_name: tool_name.clone(),
                tool_id: tool_id.clone(),
                input: input.clone(),
            }),
            Self::ToolResultDelta { tool_id, delta } => {
                StreamEvent::Llm(LlmEvent::ToolResultDelta {
                    tool_id: tool_id.clone(),
                    delta: delta.clone(),
                })
            }
            Self::ToolResult { tool_id, result } => StreamEvent::Llm(LlmEvent::ToolResult {
                tool_id: tool_id.clone(),
                result: result.clone(),
            }),
            Self::Steered { text } => StreamEvent::Agent(AgentEvent::SteeringDelivered {
                message: text.clone(),
            }),
            Self::Done => StreamEvent::Session(SessionEvent::Done),
            Self::Failed { .. } => return None,
        })
    }

    fn size(&self) -> usize {
        const OVERHEAD: usize = 32;
        OVERHEAD
            + match self {
                Self::ResponseStart | Self::ResponseReset | Self::Done => 0,
                Self::Thinking { text } | Self::Text { text } | Self::Steered { text } => {
                    text.len()
                }
                Self::ToolUseStart { tool_id, tool_name } => tool_id.len() + tool_name.len(),
                Self::ToolUse {
                    tool_id,
                    tool_name,
                    input,
                } => tool_id.len() + tool_name.len() + input.to_string().len(),
                Self::ToolResultDelta { tool_id, delta } => tool_id.len() + delta.len(),
                Self::ToolResult { tool_id, result } => tool_id.len() + result.len(),
                Self::Failed { category } => category.len(),
            }
    }

    /// Append `next` onto `self` when both are deltas of the same stream.
    /// Returns the bytes added, or `None` when they don't merge.
    fn absorb(&mut self, next: &WatchEvent) -> Option<usize> {
        match (self, next) {
            (Self::Text { text }, Self::Text { text: more })
            | (Self::Thinking { text }, Self::Thinking { text: more }) => {
                text.push_str(more);
                Some(more.len())
            }
            (
                Self::ToolResultDelta { tool_id, delta },
                Self::ToolResultDelta {
                    tool_id: next_id,
                    delta: more,
                },
            ) if tool_id == next_id && delta.len() < TOOL_PAYLOAD_MAX_BYTES => {
                delta.push_str(more);
                Some(more.len())
            }
            _ => None,
        }
    }
}

fn cap_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n… [{} more bytes not shown]",
        &text[..end],
        text.len() - end
    )
}

fn cap_input(input: &Value) -> Value {
    let raw = input.to_string();
    if raw.len() <= TOOL_PAYLOAD_MAX_BYTES {
        input.clone()
    } else {
        Value::String(cap_text(&raw, TOOL_PAYLOAD_MAX_BYTES))
    }
}

/// Bounded, append-only record of a worker's presentation events.
#[derive(Debug, Default)]
pub struct WatchJournal {
    /// `(seq, event)`, oldest first; `seq` is contiguous.
    entries: VecDeque<(u64, WatchEvent)>,
    /// `seq` the next new entry gets.
    next_seq: u64,
    /// Entries with `seq < sealed` were returned to a reader: never mutated.
    sealed: u64,
    bytes: usize,
}

/// One page of a worker's journal (`SessionQuery::SubagentWatch` reply).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WatchPage {
    pub handle_id: String,
    pub subagent_id: u64,
    pub agent_name: String,
    /// `running`, `completed`, `cancelled`, `timed_out` or `failed`.
    pub status: String,
    /// The worker accepts steering (it is running and has a steer channel).
    pub steerable: bool,
    /// Ask for `since = next_seq` next time.
    pub next_seq: u64,
    /// Entries before the requested `since` were dropped (journal cap).
    pub truncated: bool,
    /// More entries are ready right now (this page hit its byte budget).
    pub more: bool,
    pub events: Vec<WatchEvent>,
}

impl WatchJournal {
    /// Record a worker stream event (no-op for events the view ignores).
    pub fn record(&mut self, event: &StreamEvent) {
        if let Some(ev) = WatchEvent::from_stream(event) {
            self.push(ev);
        }
    }

    pub fn push(&mut self, ev: WatchEvent) {
        if let Some((seq, last)) = self.entries.back_mut() {
            if *seq >= self.sealed {
                if let Some(added) = last.absorb(&ev) {
                    self.bytes += added;
                    self.trim();
                    return;
                }
            }
        }
        self.bytes += ev.size();
        self.entries.push_back((self.next_seq, ev));
        self.next_seq += 1;
        self.trim();
    }

    fn trim(&mut self) {
        while self.bytes > JOURNAL_MAX_BYTES && self.entries.len() > 1 {
            if let Some((_, ev)) = self.entries.pop_front() {
                self.bytes = self.bytes.saturating_sub(ev.size());
            }
        }
    }

    /// Events from `since` on (at most [`PAGE_MAX_BYTES`], always at least
    /// one). Seals everything returned so later appends start new entries.
    /// Fills the journal fields of a [`WatchPage`]; the caller adds identity.
    pub fn read(&mut self, since: u64) -> WatchPage {
        let first = self.entries.front().map_or(self.next_seq, |(s, _)| *s);
        let truncated = since < first;
        let mut page = WatchPage {
            truncated,
            // A cursor past the end (stale client) restarts at the end.
            next_seq: since.clamp(first, self.next_seq),
            ..WatchPage::default()
        };
        let mut budget = 0usize;
        for (seq, ev) in self.entries.iter().filter(|(s, _)| *s >= since) {
            let size = ev.size();
            if !page.events.is_empty() && budget + size > PAGE_MAX_BYTES {
                page.more = true;
                break;
            }
            budget += size;
            page.events.push(ev.clone());
            page.next_seq = seq + 1;
        }
        self.sealed = self.sealed.max(page.next_seq);
        page
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Retained payload bytes (the [`JOURNAL_MAX_BYTES`] measure).
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> WatchEvent {
        WatchEvent::Text { text: s.into() }
    }

    /// Every journal entry replays as the stream event it was recorded
    /// from, so a client renders it through the main transcript path.
    #[test]
    fn entries_replay_as_their_stream_events() {
        let all = vec![
            WatchEvent::ResponseStart,
            WatchEvent::ResponseReset,
            WatchEvent::Thinking { text: "hmm".into() },
            text("hi"),
            WatchEvent::ToolUseStart {
                tool_id: "t1".into(),
                tool_name: "bash".into(),
            },
            WatchEvent::ToolUse {
                tool_id: "t1".into(),
                tool_name: "bash".into(),
                input: serde_json::json!({ "command": "ls" }),
            },
            WatchEvent::ToolResultDelta {
                tool_id: "t1".into(),
                delta: "a".into(),
            },
            WatchEvent::ToolResult {
                tool_id: "t1".into(),
                result: "a\n".into(),
            },
            WatchEvent::Steered {
                text: "focus".into(),
            },
            WatchEvent::Done,
        ];
        for ev in all {
            let stream = ev.to_stream().expect("replayable");
            assert_eq!(WatchEvent::from_stream(&stream), Some(ev));
        }
        assert!(WatchEvent::Failed {
            category: "network".into()
        }
        .to_stream()
        .is_none());
    }

    #[test]
    fn deltas_coalesce_until_a_reader_sees_them() {
        let mut j = WatchJournal::default();
        j.push(text("hel"));
        j.push(text("lo"));
        assert_eq!(j.len(), 1, "unseen deltas merge");

        let p1 = j.read(0);
        assert_eq!(p1.events, vec![text("hello")]);
        assert_eq!(p1.next_seq, 1);

        // The entry the reader saw is sealed: new text starts a new entry,
        // so paging from next_seq gets exactly the new part.
        j.push(text(" world"));
        let p2 = j.read(p1.next_seq);
        assert_eq!(p2.events, vec![text(" world")]);
        assert!(!p2.truncated);

        // Nothing new: empty page, same cursor.
        let p3 = j.read(p2.next_seq);
        assert!(p3.events.is_empty());
        assert_eq!(p3.next_seq, p2.next_seq);
    }

    #[test]
    fn different_kinds_and_tools_do_not_merge() {
        let mut j = WatchJournal::default();
        j.push(WatchEvent::Thinking { text: "a".into() });
        j.push(text("b"));
        j.push(WatchEvent::ToolResultDelta {
            tool_id: "t1".into(),
            delta: "x".into(),
        });
        j.push(WatchEvent::ToolResultDelta {
            tool_id: "t2".into(),
            delta: "y".into(),
        });
        j.push(WatchEvent::ToolResultDelta {
            tool_id: "t2".into(),
            delta: "z".into(),
        });
        assert_eq!(j.len(), 4);
        let page = j.read(0);
        assert_eq!(
            page.events.last(),
            Some(&WatchEvent::ToolResultDelta {
                tool_id: "t2".into(),
                delta: "yz".into()
            })
        );
    }

    #[test]
    fn the_cap_drops_oldest_and_late_readers_see_truncated() {
        let mut j = WatchJournal::default();
        let chunk = "x".repeat(256 * 1024);
        for i in 0..12 {
            j.push(WatchEvent::ToolResult {
                tool_id: format!("t{i}"),
                result: chunk.clone(),
            });
        }
        assert!(j.bytes <= JOURNAL_MAX_BYTES, "bounded: {}", j.bytes);
        let page = j.read(0);
        assert!(page.truncated, "seq 0 was dropped");
        assert!(page.events.len() < 12);
    }

    #[test]
    fn pages_respect_the_byte_budget_and_resume() {
        let mut j = WatchJournal::default();
        let chunk = "y".repeat(400 * 1024);
        for i in 0..4 {
            j.push(WatchEvent::ToolResult {
                tool_id: format!("t{i}"),
                result: chunk.clone(),
            });
        }
        let p1 = j.read(0);
        assert!(
            p1.more && p1.events.len() == 2,
            "1 MiB budget: {}",
            p1.events.len()
        );
        let p2 = j.read(p1.next_seq);
        assert_eq!(p2.events.len(), 2);
        assert!(!p2.more);
    }

    #[test]
    fn stream_events_map_and_errors_keep_only_the_category() {
        let err = agent_core::TurnError::provider("secret upstream body", "auth_error", "turn-9");
        let ev = WatchEvent::from_stream(&StreamEvent::Session(SessionEvent::Error(err))).unwrap();
        match ev {
            WatchEvent::Failed { category } => assert!(!category.contains("secret")),
            other => panic!("unexpected {other:?}"),
        }
        assert!(
            WatchEvent::from_stream(&StreamEvent::Session(SessionEvent::Notice(
                "retry: upstream said …".into()
            )))
            .is_none()
        );
        let big = serde_json::json!({ "content": "z".repeat(TOOL_PAYLOAD_MAX_BYTES * 2) });
        let ev = WatchEvent::from_stream(&StreamEvent::Llm(LlmEvent::ToolUse {
            tool_name: "write".into(),
            tool_id: "t".into(),
            input: big,
        }))
        .unwrap();
        assert!(
            ev.size() < TOOL_PAYLOAD_MAX_BYTES + 1024,
            "tool input capped"
        );
    }

    #[test]
    fn pages_round_trip_through_json() {
        let mut j = WatchJournal::default();
        j.push(WatchEvent::ToolUse {
            tool_id: "t".into(),
            tool_name: "bash".into(),
            input: serde_json::json!({ "command": "ls" }),
        });
        j.push(WatchEvent::Failed {
            category: "canceled".into(),
        });
        let page = j.read(0);
        let back: WatchPage = serde_json::from_value(serde_json::to_value(&page).unwrap()).unwrap();
        assert_eq!(back, page);
    }
}
