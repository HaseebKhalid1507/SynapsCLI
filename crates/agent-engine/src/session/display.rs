//! Daemon-side display projection of `api_messages` (PLAN-phase4 §2.3).
//!
//! EXACTLY the filter the TUI's `rebuild_display_messages` applied at
//! 741b6b60 (helpers.rs:170-224): skip `<context-summary>` messages, skip
//! `<event …>…</event>` payloads, project user text and assistant
//! thinking/text/tool_use blocks. Both `LocalTransport` and Digest socket
//! clients build their transcript from this, so they cannot drift.
//!
//! No TUI types here — this crate never depends on agent-tui.

use agent_core::core::extension_context::{self, user_content};
use agent_core::core::ui_types::{Answer, AnswerStatus, MARK_KEY};
use serde::{Deserialize, Serialize};

use crate::ui::fold::{fold_answer, fold_history, Fold, Outcome};
use crate::SharedMessage;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DisplayItem {
    User { text: String },
    Thinking { text: String },
    Text { text: String },
    ToolUse { tool_id: String, tool_name: String, input: String },
}

/// The last `items.len()` display items of a history; `omitted` = how many
/// display items were dropped from the front to fit the cap.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DisplayTail {
    pub items: Vec<DisplayItem>,
    #[serde(default)]
    pub omitted: usize,
    /// When each item's message happened (RFC 3339 UTC, the message's
    /// `_synaps_at`), parallel to `items`. Empty when no item has a time
    /// (legacy history, older daemons); `None` for an item without one.
    /// Clients use [`DisplayTail::time`], never index it directly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub times: Vec<Option<String>>,
    /// The daemon's fold of each item (agent-ui §2.5, v3.2): what history's
    /// host marks prove about a `ui` call or a host answer. Parallel to
    /// `items` and empty when no item has one, like `times`; older readers
    /// ignore it. Clients use [`DisplayTail::ui_item`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ui: Vec<Option<UiItem>>,
}

impl DisplayTail {
    /// The recorded time of `items[i]`, if any.
    pub fn time(&self, i: usize) -> Option<&str> {
        self.times.get(i)?.as_deref()
    }

    /// The fold result of `items[i]`, if any.
    pub fn ui_item(&self, i: usize) -> Option<&UiItem> {
        self.ui.get(i)?.as_ref()
    }
}

/// One item's fold result (agent-ui §2.5): `{"answer":{…}}` on a `User` item
/// the daemon folded as a host answer (its values are in the item's text),
/// `{"call":{…}}` on a `ToolUse{ui}` item. Never a new [`DisplayItem`]
/// variant: that enum has no `#[serde(other)]`, so older clients would
/// fail on one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiItem {
    Answer {
        card: String,
        answer: String,
        rev: u32,
        status: AnswerStatus,
        late: bool,
    },
    Call {
        outcome: UiOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        card: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answer: Option<String>,
        /// Only on `answered`: a `wait` result's values, which the Digest
        /// otherwise lacks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        values: Option<Answer>,
    },
}

/// What history proves about a `ui` call ([`Outcome`], on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiOutcome {
    Shown,
    Answered,
    Skipped,
    Pending,
    Closed,
    Unconfirmed,
    NotRecorded,
    NoCard,
}

impl UiItem {
    fn call(outcome: &Outcome) -> Self {
        let (o, card, answer, values) = match outcome {
            Outcome::NotRecorded => (UiOutcome::NotRecorded, None, None, None),
            Outcome::NoCard => (UiOutcome::NoCard, None, None, None),
            Outcome::Shown { card } => (UiOutcome::Shown, Some(card), None, None),
            Outcome::Answered {
                card,
                answer_id,
                answer,
            } => (
                UiOutcome::Answered,
                Some(card),
                Some(answer_id),
                Some(answer.clone()),
            ),
            Outcome::Skipped { card, answer_id } => {
                (UiOutcome::Skipped, Some(card), Some(answer_id), None)
            }
            Outcome::Pending { card } => (UiOutcome::Pending, Some(card), None, None),
            Outcome::Closed { card } => (UiOutcome::Closed, Some(card), None, None),
            Outcome::Unconfirmed { card } => (UiOutcome::Unconfirmed, Some(card), None, None),
        };
        UiItem::Call {
            outcome: o,
            card: card.cloned(),
            answer: answer.cloned(),
            values,
        }
    }
}

/// The fold of `msgs`, when anything in them can fold: a host mark or a
/// `ui` call. One cheap scan otherwise.
fn fold_of(msgs: &[SharedMessage]) -> Option<Fold> {
    let any = msgs.iter().any(|m| {
        m.get(MARK_KEY).is_some()
            || m["content"].as_array().is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|b| b["type"] == "tool_use" && b["name"] == "ui")
            })
    });
    any.then(|| fold_history(msgs))
}

/// The fold results of `items`, the display items of `msg`, in order.
fn ui_of(
    msg: &serde_json::Value,
    items: &[DisplayItem],
    fold: Option<&Fold>,
) -> Vec<Option<UiItem>> {
    let mut out = vec![None; items.len()];
    let Some(fold) = fold else {
        return out;
    };
    // A host answer is its message's first item (its text, as sent).
    if msg["role"] == "user" && user_content(msg).is_string() {
        if let (Some(a), Some(slot)) = (fold_answer(msg), out.first_mut()) {
            *slot = Some(UiItem::Answer {
                card: a.card,
                answer: a.answer_id,
                rev: a.rev,
                status: a.status,
                late: a.late,
            });
        }
    }
    for (item, slot) in items.iter().zip(out.iter_mut()) {
        if let DisplayItem::ToolUse {
            tool_id, tool_name, ..
        } = item
        {
            if tool_name == "ui" {
                *slot = fold
                    .calls
                    .iter()
                    .find(|c| c.call_id == *tool_id)
                    .map(|c| UiItem::call(&c.outcome));
            }
        }
    }
    out
}

/// `ui` for items projected from messages: dropped when none has one.
fn item_ui(ui: Vec<Option<UiItem>>) -> Vec<Option<UiItem>> {
    if ui.iter().all(Option::is_none) {
        Vec::new()
    } else {
        ui
    }
}

/// `times` for items projected from messages: one entry per item, dropped
/// entirely when none has a time.
fn item_times(times: Vec<Option<String>>) -> Vec<Option<String>> {
    if times.iter().all(Option::is_none) {
        Vec::new()
    } else {
        times
    }
}

fn time_of(msg: &serde_json::Value) -> Option<String> {
    crate::runtime::message_time::message_time(msg).map(str::to_owned)
}

/// True iff `content` is a canonical agent-event payload
/// (`format_event_for_agent`): `<event id=… …>…</event>`.
pub fn is_event_payload(content: &str) -> bool {
    content.starts_with("<event ") && content.ends_with("</event>")
}

/// The saved extension blocks and markers of a message, as display text.
fn saved_texts(msg: &serde_json::Value) -> impl Iterator<Item = String> + '_ {
    extension_context::extension_blocks(msg)
        .iter()
        .filter_map(|block| block["text"].as_str())
        .map(|text| text.trim_start().to_string())
}

/// Display items of ONE api message, in order. Empty for skipped messages.
///
/// A user message is read as the user sent it (`user_content`: extension
/// context saved on it removed, #414); each saved extension block or marker
/// then follows as its own `User` item holding the block's text, which
/// clients recognise with `extension_context::parse_saved` and show as
/// extension context, not as something the user typed. Older clients show
/// it as a user line (the wire shape is unchanged).
fn items_of(msg: &serde_json::Value, out: &mut Vec<DisplayItem>) {
    let content = user_content(msg);
    if let Some(content) = content.as_str() {
        if content.contains("<context-summary>") || is_event_payload(content) {
            return;
        }
    }
    match msg["role"].as_str() {
        Some("user") => {
            if let Some(content) = content.as_str() {
                out.push(DisplayItem::User { text: content.to_string() });
            }
            out.extend(saved_texts(msg).map(|text| DisplayItem::User { text }));
        }
        Some("assistant") => {
            if let Some(content) = msg["content"].as_array() {
                for block in content {
                    match block["type"].as_str() {
                        Some("thinking") => {
                            if let Some(text) = block["thinking"].as_str() {
                                out.push(DisplayItem::Thinking { text: text.to_string() });
                            }
                        }
                        Some("text") => {
                            if let Some(text) = block["text"].as_str() {
                                out.push(DisplayItem::Text { text: text.to_string() });
                            }
                        }
                        Some("tool_use") => {
                            out.push(DisplayItem::ToolUse {
                                tool_id: block["id"].as_str().unwrap_or("").to_string(),
                                tool_name: block["name"].as_str().unwrap_or("").to_string(),
                                input: serde_json::to_string(&block["input"]).unwrap_or_default(),
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }
}

/// `items_of(msg).len()` without building the items: the same skip rules,
/// no string clones (M4 — the daemon runs this over the whole history on
/// every attach/compaction/resync).
fn count_items_of(msg: &serde_json::Value) -> usize {
    let content = user_content(msg);
    if let Some(content) = content.as_str() {
        if content.contains("<context-summary>") || is_event_payload(content) {
            return 0;
        }
    }
    match msg["role"].as_str() {
        Some("user") => usize::from(content.is_string()) + saved_texts(msg).count(),
        Some("assistant") => msg["content"]
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| match b["type"].as_str() {
                        Some("thinking") => b["thinking"].is_string(),
                        Some("text") => b["text"].is_string(),
                        Some("tool_use") => true,
                        _ => false,
                    })
                    .count()
            })
            .unwrap_or(0),
        _ => 0,
    }
}

/// Every display item of `msgs` (no cap).
pub fn display_items(msgs: &[SharedMessage]) -> Vec<DisplayItem> {
    let mut out = Vec::new();
    for m in msgs {
        items_of(m, &mut out);
    }
    out
}

/// Every display item of `msgs` with its message's time and its fold.
type Projected = (Vec<DisplayItem>, Vec<Option<String>>, Vec<Option<UiItem>>);

fn timed_items(msgs: &[SharedMessage]) -> Projected {
    let fold = fold_of(msgs);
    let mut items = Vec::new();
    let mut times = Vec::new();
    let mut ui = Vec::new();
    for m in msgs {
        let start = items.len();
        items_of(m, &mut items);
        if times.len() < items.len() {
            times.resize(items.len(), time_of(m));
            ui.extend(ui_of(m, &items[start..], fold.as_ref()));
        }
    }
    (items, times, ui)
}

/// The last `max_items` display items (`0` = unbounded). Walks from the back
/// and stops once the cap is met; the head is only counted (no allocation),
/// so a 20 MB history costs O(tail) allocations + one O(n) scan.
pub fn display_tail(msgs: &[SharedMessage], max_items: usize) -> DisplayTail {
    if max_items == 0 {
        let (items, times, ui) = timed_items(msgs);
        return DisplayTail {
            items,
            omitted: 0,
            times: item_times(times),
            ui: item_ui(ui),
        };
    }
    // The fold reads the whole history (a result or an answer can come long
    // after its call); only the tail is projected.
    let fold = fold_of(msgs);
    // Per-message item groups, newest first, until we hold >= max_items.
    let mut groups: Vec<(Vec<DisplayItem>, Option<String>, Vec<Option<UiItem>>)> = Vec::new();
    let mut held = 0usize;
    let mut idx = msgs.len();
    while idx > 0 && held < max_items {
        idx -= 1;
        let mut g = Vec::new();
        items_of(&msgs[idx], &mut g);
        held += g.len();
        let at = if g.is_empty() { None } else { time_of(&msgs[idx]) };
        let ui = ui_of(&msgs[idx], &g, fold.as_ref());
        groups.push((g, at, ui));
    }
    // Items before `idx` were never projected: count them without cloning.
    let mut omitted: usize = msgs[..idx].iter().map(|m| count_items_of(m)).sum();
    let mut items: Vec<DisplayItem> = Vec::with_capacity(held);
    let mut times: Vec<Option<String>> = Vec::with_capacity(held);
    let mut ui: Vec<Option<UiItem>> = Vec::with_capacity(held);
    for (g, at, u) in groups.into_iter().rev() {
        times.extend(std::iter::repeat_n(at, g.len()));
        items.extend(g);
        ui.extend(u);
    }
    if items.len() > max_items {
        let extra = items.len() - max_items;
        items.drain(..extra);
        times.drain(..extra);
        ui.drain(..extra);
        omitted += extra;
    }
    DisplayTail {
        items,
        omitted,
        times: item_times(times),
        ui: item_ui(ui),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn user(s: &str) -> SharedMessage {
        Arc::new(serde_json::json!({"role": "user", "content": s}))
    }
    fn assistant(blocks: serde_json::Value) -> SharedMessage {
        Arc::new(serde_json::json!({"role": "assistant", "content": blocks}))
    }

    #[test]
    fn filter_matches_741b6b60() {
        let msgs = vec![
            user("hi"),
            user("<context-summary>x</context-summary>"),
            user("<event id=\"1\">e</event>"),
            assistant(serde_json::json!([
                {"type": "thinking", "thinking": "hm"},
                {"type": "text", "text": "yo"},
                {"type": "tool_use", "id": "t1", "name": "bash", "input": {"cmd": "ls"}},
                {"type": "tool_result", "tool_use_id": "t1", "content": "x"}
            ])),
            Arc::new(serde_json::json!({"role": "user", "content": [{"type": "tool_result"}]})),
        ];
        let items = display_items(&msgs);
        assert_eq!(
            items,
            vec![
                DisplayItem::User { text: "hi".into() },
                DisplayItem::Thinking { text: "hm".into() },
                DisplayItem::Text { text: "yo".into() },
                DisplayItem::ToolUse { tool_id: "t1".into(), tool_name: "bash".into(), input: "{\"cmd\":\"ls\"}".into() },
            ]
        );
    }

    #[test]
    fn count_items_of_matches_items_of() {
        let msgs: Vec<serde_json::Value> = vec![
            serde_json::json!({"role": "user", "content": "hi"}),
            serde_json::json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "x"}]}),
            serde_json::json!({"role": "user", "content": "<context-summary>s</context-summary>"}),
            serde_json::json!({"role": "user", "content": "<event id=1>x</event>"}),
            serde_json::json!({"role": "assistant", "content": "plain string"}),
            serde_json::json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "t"},
                {"type": "thinking", "signature": "no-text"},
                {"type": "text", "text": "a"},
                {"type": "text"},
                {"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "ls"}},
                {"type": "tool_use"},
                {"type": "unknown"}
            ]}),
            serde_json::json!({"role": "system", "content": "s"}),
            serde_json::json!({}),
        ];
        for m in &msgs {
            let mut v = Vec::new();
            items_of(m, &mut v);
            assert_eq!(count_items_of(m), v.len(), "{m}");
        }
    }

    #[test]
    fn tail_caps_and_counts_omitted() {
        let msgs: Vec<SharedMessage> = (0..10).map(|i| user(&format!("m{i}"))).collect();
        let t = display_tail(&msgs, 3);
        assert_eq!(t.omitted, 7);
        assert_eq!(t.items.len(), 3);
        assert_eq!(t.items[0], DisplayItem::User { text: "m7".into() });
        let full = display_tail(&msgs, 0);
        assert_eq!(full.omitted, 0);
        assert_eq!(full.items.len(), 10);
        let big = display_tail(&msgs, 100);
        assert_eq!(big.omitted, 0);
        assert_eq!(big.items.len(), 10);
    }

    #[test]
    fn tail_splits_inside_a_multi_block_message() {
        let msgs = vec![
            user("a"),
            assistant(serde_json::json!([{"type": "text", "text": "b"}, {"type": "text", "text": "c"}])),
        ];
        let t = display_tail(&msgs, 1);
        assert_eq!(t.items, vec![DisplayItem::Text { text: "c".into() }]);
        assert_eq!(t.omitted, 2);
        let all = display_items(&msgs);
        assert_eq!(all[all.len() - t.items.len()..], t.items[..]);
    }

    #[test]
    fn tail_equals_items_suffix_for_every_cap() {
        let msgs = vec![
            user("a"),
            user("<context-summary>s</context-summary>"),
            assistant(serde_json::json!([{"type": "thinking", "thinking": "t"}, {"type": "text", "text": "b"}])),
            user("<event id=\"1\">e</event>"),
            assistant(serde_json::json!([{"type": "tool_use", "id": "x", "name": "bash", "input": {}}])),
            user("c"),
        ];
        let all = display_items(&msgs);
        for cap in 1..=all.len() + 2 {
            let t = display_tail(&msgs, cap);
            let keep = cap.min(all.len());
            assert_eq!(t.items[..], all[all.len() - keep..], "cap={cap}");
            assert_eq!(t.omitted, all.len() - keep, "cap={cap}");
        }
    }

    fn timed(mut m: serde_json::Value, ts: &str) -> SharedMessage {
        m["_synaps_at"] = ts.into();
        Arc::new(m)
    }

    #[test]
    fn tail_carries_each_items_message_time() {
        let msgs = vec![
            user("legacy, no time"),
            timed(serde_json::json!({"role": "user", "content": "q"}), "2026-10-25T05:47:00Z"),
            timed(
                serde_json::json!({"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "t"}, {"type": "text", "text": "a"}
                ]}),
                "2026-10-25T05:48:00Z",
            ),
            timed(serde_json::json!({"role": "user", "content": [{"type": "tool_result"}]}), "2026-10-25T05:49:00Z"),
        ];
        let expect = |t: &DisplayTail| {
            (0..t.items.len()).map(|i| t.time(i).map(str::to_owned)).collect::<Vec<_>>()
        };
        let all = display_tail(&msgs, 0);
        assert_eq!(
            expect(&all),
            vec![
                None,
                Some("2026-10-25T05:47:00Z".into()),
                Some("2026-10-25T05:48:00Z".into()),
                Some("2026-10-25T05:48:00Z".into()),
            ]
        );
        for cap in 1..=5 {
            let t = display_tail(&msgs, cap);
            let keep = cap.min(4);
            assert_eq!(expect(&t), expect(&all)[4 - keep..], "cap={cap}");
        }
        // Wire: absent when nothing has a time, and old payloads still parse.
        let legacy = display_tail(&[user("a"), user("b")], 0);
        assert!(legacy.times.is_empty());
        let wire = serde_json::to_value(&legacy).unwrap();
        assert!(wire.get("times").is_none(), "{wire}");
        let old: DisplayTail = serde_json::from_value(serde_json::json!({
            "items": [{"kind": "user", "text": "a"}], "omitted": 0
        }))
        .unwrap();
        assert_eq!(old.time(0), None);
        let round: DisplayTail = serde_json::from_value(serde_json::to_value(&all).unwrap()).unwrap();
        assert_eq!(round, all);
    }

    mod ui {
        use super::*;
        use crate::ui::answer::{format_answer_message, AnswerMessage};
        use agent_core::core::ui_types::Choice;
        use serde_json::{json, Value};

        const CARD: &str = "s_3f2a91c0d4e2a871";
        const ANS: &str = "a_6c1d4b7e9f203a58";

        fn m(v: Value) -> SharedMessage {
            Arc::new(v)
        }

        fn call(id: &str, input: Value) -> SharedMessage {
            m(
                json!({"role": "assistant", "content": [{"type": "text", "text": "asking"},
                {"type": "tool_use", "id": id, "name": "ui", "input": input}]}),
            )
        }

        fn result(id: &str, content: Value, mark: Option<Value>) -> SharedMessage {
            let mut v = json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": id,
                "content": content.to_string()}]});
            if let Some(mark) = mark {
                v[MARK_KEY] = mark;
            }
            m(v)
        }

        fn answer_text() -> String {
            format_answer_message(&AnswerMessage {
                card: CARD.into(),
                answer_id: ANS.into(),
                q: "Which approach?".into(),
                status: AnswerStatus::Answered,
                rev: 1,
                values: Answer {
                    choice: Some(Choice::One("Refactor the parser".into())),
                    ..Answer::default()
                },
                asked: None,
            })
        }

        fn answered(marked: bool) -> SharedMessage {
            let mut v = json!({"role": "user", "content": answer_text()});
            if marked {
                v[MARK_KEY] = json!({"l": "9b1f", "card": CARD, "answer": ANS, "rev": 1, "status": "answered"});
            }
            m(v)
        }

        fn question() -> Value {
            json!({"question": "Which approach?", "options": ["Refactor the parser", "Rewrite from scratch"]})
        }

        fn history() -> Vec<SharedMessage> {
            vec![
                user("refactor the parser"),
                call("toolu_1", question()),
                result(
                    "toolu_1",
                    json!({"status": "shown", "id": CARD}),
                    Some(json!({"l": "9b1f", "results": [{"card": CARD, "answer": null}]})),
                ),
                answered(true),
                // A look-alike typed by the user: no mark, typed text.
                answered(false),
                call("toolu_2", question()),
                m(
                    json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_2",
                    "is_error": true, "content": "blocks: unknown parameter"}]}),
                ),
                call("toolu_3", question()),
            ]
        }

        /// `ui` is the daemon's fold, parallel to `items`: a proven `ui`
        /// call's outcome, a marked host answer, nothing for typed text or
        /// other items; an error result is a tool row (`no_card`), a call
        /// without evidence `not_recorded`.
        #[test]
        fn the_tail_carries_the_fold() {
            let tail = display_tail(&history(), 0);
            assert_eq!(tail.ui.len(), tail.items.len());
            let call = |o, card: Option<&str>| UiItem::Call {
                outcome: o,
                card: card.map(str::to_string),
                answer: None,
                values: None,
            };
            let expect = vec![
                None, // "refactor the parser"
                None, // "asking"
                Some(call(UiOutcome::Shown, Some(CARD))),
                Some(UiItem::Answer {
                    card: CARD.into(),
                    answer: ANS.into(),
                    rev: 1,
                    status: AnswerStatus::Answered,
                    late: false,
                }),
                None, // the look-alike
                None,
                Some(call(UiOutcome::NoCard, None)),
                None,
                Some(call(UiOutcome::NotRecorded, None)),
            ];
            assert_eq!(tail.ui, expect, "{:?}", tail.items);
            assert_eq!(tail.ui_item(3), expect[3].as_ref());
            // On the wire, as §2.5 writes it.
            let wire = serde_json::to_value(&tail).unwrap();
            assert_eq!(
                wire["ui"][2],
                json!({"call": {"outcome": "shown", "card": CARD}})
            );
            assert_eq!(
                wire["ui"][3],
                json!({"answer": {"card": CARD, "answer": ANS, "rev": 1, "status": "answered", "late": false}})
            );
            assert_eq!(wire["ui"][8], json!({"call": {"outcome": "not_recorded"}}));
            assert_eq!(serde_json::from_value::<DisplayTail>(wire).unwrap(), tail);
        }

        /// A capped tail keeps `ui` parallel: the fold still reads the whole
        /// history (a result folds its call even when the call's message
        /// is the tail's first), and only the tail is projected.
        #[test]
        fn a_capped_tail_keeps_the_fold_parallel() {
            let full = display_tail(&history(), 0);
            for cap in 1..=full.items.len() {
                let tail = display_tail(&history(), cap);
                assert_eq!(tail.items.len(), cap);
                let from = full.items.len() - cap;
                let want = item_ui(full.ui[from..].to_vec());
                assert_eq!(tail.ui, want, "cap {cap}");
            }
        }

        /// No `ui` call and no mark: no `ui` on the wire (older readers see
        /// the tail they always did).
        #[test]
        fn no_fold_no_field() {
            let msgs = vec![
                user("hi"),
                assistant(json!([{"type": "text", "text": "yo"}])),
            ];
            let tail = display_tail(&msgs, 0);
            assert!(tail.ui.is_empty());
            assert!(serde_json::to_value(&tail).unwrap().get("ui").is_none());
            // An older daemon's tail (no field) reads as no fold.
            let old: DisplayTail =
                serde_json::from_value(json!({"items": [], "omitted": 0})).unwrap();
            assert!(old.ui.is_empty());
        }
    }
}
