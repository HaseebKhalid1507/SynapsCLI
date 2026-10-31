//! Daemon-side display projection of `api_messages` (PLAN-phase4 §2.3).
//!
//! EXACTLY the filter the TUI's `rebuild_display_messages` applied at
//! 741b6b60 (helpers.rs:170-224): skip `<context-summary>` messages, skip
//! `<event …>…</event>` payloads, project user text and assistant
//! thinking/text/tool_use blocks. Both `LocalTransport` and Digest socket
//! clients build their transcript from this, so they cannot drift.
//!
//! No TUI types here — this crate never depends on agent-tui.

use serde::{Deserialize, Serialize};

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
}

impl DisplayTail {
    /// The recorded time of `items[i]`, if any.
    pub fn time(&self, i: usize) -> Option<&str> {
        self.times.get(i)?.as_deref()
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

/// Display items of ONE api message, in order. Empty for skipped messages.
fn items_of(msg: &serde_json::Value, out: &mut Vec<DisplayItem>) {
    if let Some(content) = msg["content"].as_str() {
        if content.contains("<context-summary>") || is_event_payload(content) {
            return;
        }
    }
    match msg["role"].as_str() {
        Some("user") => {
            if let Some(content) = msg["content"].as_str() {
                out.push(DisplayItem::User { text: content.to_string() });
            }
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
    if let Some(content) = msg["content"].as_str() {
        if content.contains("<context-summary>") || is_event_payload(content) {
            return 0;
        }
    }
    match msg["role"].as_str() {
        Some("user") => usize::from(msg["content"].is_string()),
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

/// Every display item of `msgs` with its message's time.
fn timed_items(msgs: &[SharedMessage]) -> (Vec<DisplayItem>, Vec<Option<String>>) {
    let mut items = Vec::new();
    let mut times = Vec::new();
    for m in msgs {
        items_of(m, &mut items);
        if times.len() < items.len() {
            times.resize(items.len(), time_of(m));
        }
    }
    (items, times)
}

/// The last `max_items` display items (`0` = unbounded). Walks from the back
/// and stops once the cap is met; the head is only counted (no allocation),
/// so a 20 MB history costs O(tail) allocations + one O(n) scan.
pub fn display_tail(msgs: &[SharedMessage], max_items: usize) -> DisplayTail {
    if max_items == 0 {
        let (items, times) = timed_items(msgs);
        return DisplayTail { items, omitted: 0, times: item_times(times) };
    }
    // Per-message item groups, newest first, until we hold >= max_items.
    let mut groups: Vec<(Vec<DisplayItem>, Option<String>)> = Vec::new();
    let mut held = 0usize;
    let mut idx = msgs.len();
    while idx > 0 && held < max_items {
        idx -= 1;
        let mut g = Vec::new();
        items_of(&msgs[idx], &mut g);
        held += g.len();
        let at = if g.is_empty() { None } else { time_of(&msgs[idx]) };
        groups.push((g, at));
    }
    // Items before `idx` were never projected: count them without cloning.
    let mut omitted: usize = msgs[..idx].iter().map(|m| count_items_of(m)).sum();
    let mut items: Vec<DisplayItem> = Vec::with_capacity(held);
    let mut times: Vec<Option<String>> = Vec::with_capacity(held);
    for (g, at) in groups.into_iter().rev() {
        times.extend(std::iter::repeat_n(at, g.len()));
        items.extend(g);
    }
    if items.len() > max_items {
        let extra = items.len() - max_items;
        items.drain(..extra);
        times.drain(..extra);
        omitted += extra;
    }
    DisplayTail { items, omitted, times: item_times(times) }
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
}
