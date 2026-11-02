//! Daemon-side display projection of `api_messages` (PLAN-phase4 §2.3).
//!
//! The filter the TUI's `rebuild_display_messages` applied at 741b6b60
//! (helpers.rs:170-224): skip `<context-summary>` messages, skip
//! `<event …>…</event>` payloads, project user text and assistant
//! thinking/text/tool_use blocks. Both `LocalTransport` and Digest socket
//! clients build their transcript from this, so they cannot drift.
//!
//! Focus hold mode (S1, wire contract v1.3 §3.1): a released digest and
//! the join block in front of a user message are event payloads too, but
//! they come out as a [`DisplayItem::Held`] item (keyed on the envelope's
//! `type` attribute, read structurally). A user message whose content is
//! an array of blocks gives one item per text block (the join block, then
//! the user's own text); tool results and attachments give none.
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
    /// (S1) A released hold digest, or the join block in front of a user
    /// message: `text` is the envelope exactly as in history.
    Held { source: HeldSource, text: String },
    /// A kind from a newer daemon.
    #[serde(other)]
    Unknown,
}

/// [`DisplayItem::Held`]'s `source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeldSource {
    /// A `held_digest` user message.
    Digest,
    /// A `held_join` block.
    Join,
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

/// The `type` and `severity` attributes of a canonical event payload, read
/// structurally: the envelope opens `<event id="…" type="…" severity="…"`,
/// and no attribute value holds a `"` (events/format.rs `attr`), so neither
/// the id nor the text can pass for them.
fn payload_head(content: &str) -> Option<(&str, &str)> {
    if !is_event_payload(content) {
        return None;
    }
    let rest = content.strip_prefix("<event id=\"")?;
    let rest = &rest[rest.find('"')? + 1..];
    let rest = rest.strip_prefix(" type=\"")?;
    let end = rest.find('"')?;
    let (kind, rest) = (&rest[..end], &rest[end + 1..]);
    let rest = rest.strip_prefix(" severity=\"")?;
    Some((kind, &rest[..rest.find('"')?]))
}

/// See [`payload_head`].
pub fn event_payload_type(content: &str) -> Option<&str> {
    payload_head(content).map(|(kind, _)| kind)
}

/// See [`payload_head`].
pub fn event_payload_severity(content: &str) -> Option<&str> {
    payload_head(content).map(|(_, severity)| severity)
}

/// What a user text (a string content, or one text block) shows as.
enum UserText {
    Skip,
    User,
    Held(HeldSource),
}

fn classify_user_text(text: &str) -> UserText {
    if is_event_payload(text) {
        return match event_payload_type(text) {
            Some(super::held::HELD_DIGEST_TYPE) => UserText::Held(HeldSource::Digest),
            Some(super::held::HELD_JOIN_TYPE) => UserText::Held(HeldSource::Join),
            _ => UserText::Skip,
        };
    }
    if text.contains("<context-summary>") {
        return UserText::Skip;
    }
    UserText::User
}

fn push_user_text(text: &str, out: &mut Vec<DisplayItem>) {
    match classify_user_text(text) {
        UserText::Skip => {}
        UserText::User => out.push(DisplayItem::User { text: text.to_string() }),
        UserText::Held(source) => out.push(DisplayItem::Held {
            source,
            text: text.to_string(),
        }),
    }
}

/// A user message's texts: its string content, or each of its text blocks.
fn user_texts(content: &serde_json::Value) -> impl Iterator<Item = &str> {
    let (one, blocks) = match content {
        serde_json::Value::String(s) => (Some(s.as_str()), None),
        serde_json::Value::Array(blocks) => (None, Some(blocks.iter())),
        _ => (None, None),
    };
    one.into_iter().chain(
        blocks
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str()),
    )
}

/// Display items of ONE api message, in order. Empty for skipped messages.
fn items_of(msg: &serde_json::Value, out: &mut Vec<DisplayItem>) {
    match msg["role"].as_str() {
        Some("user") => {
            for text in user_texts(&msg["content"]) {
                push_user_text(text, out);
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
    match msg["role"].as_str() {
        Some("user") => user_texts(&msg["content"])
            .filter(|t| !matches!(classify_user_text(t), UserText::Skip))
            .count(),
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
            // (S1, v1.3) a digest, a joined user message, an attachment submit.
            serde_json::json!({"role": "user", "content": envelope("held_digest", "- [low] w: x (h-1)", Severity::Low)}),
            serde_json::json!({"role": "user", "content": [
                {"type": "text", "text": envelope("held_join", "", Severity::Medium)},
                {"type": "text", "text": "q"}
            ]}),
            serde_json::json!({"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA=="}},
                {"type": "text", "text": envelope("message", "steered", Severity::Low)},
                {"type": "text"}
            ]}),
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

    use crate::events::types::{Event, Severity};

    /// An event payload as history holds it.
    fn envelope(content_type: &str, text: &str, severity: Severity) -> String {
        let mut e = Event::simple("synaps", text, Some(severity));
        e.content.content_type = content_type.into();
        crate::events::format_event_for_agent(&e)
    }

    fn user_blocks(texts: &[&str]) -> SharedMessage {
        let blocks: Vec<serde_json::Value> =
            texts.iter().map(|t| serde_json::json!({"type": "text", "text": t})).collect();
        Arc::new(serde_json::json!({"role": "user", "content": blocks}))
    }

    /// (S1, v1.3) A digest and a join block come out as `held` items, keyed
    /// on the envelope's type attribute; a joined user message gives the
    /// held item, then the user's own text. Other events stay skipped, and
    /// an event's text can't pass for the attribute.
    #[test]
    fn a_digest_and_a_join_block_are_held_items() {
        let digest = envelope("held_digest", "- [low] w: x (h-1)", Severity::High);
        let join = envelope("held_join", "", Severity::Medium);
        let other = envelope("message", "type=\"held_digest\" severity=\"high\" is just text", Severity::Low);
        let msgs = vec![
            user(&digest),
            user_blocks(&[&join, "my question"]),
            user(&other),
            user_blocks(&[&other, "after an event block"]),
        ];
        assert_eq!(
            display_items(&msgs),
            vec![
                DisplayItem::Held { source: HeldSource::Digest, text: digest.clone() },
                DisplayItem::Held { source: HeldSource::Join, text: join.clone() },
                DisplayItem::User { text: "my question".into() },
                DisplayItem::User { text: "after an event block".into() },
            ]
        );
        assert_eq!(event_payload_type(&digest), Some("held_digest"));
        assert_eq!(event_payload_severity(&digest), Some("high"));
        assert_eq!(event_payload_type(&other), Some("message"));
        assert_eq!(event_payload_severity(&other), Some("low"));
        assert_eq!(event_payload_type("<event id=\"1\">e</event>"), None);
        assert_eq!(event_payload_type("plain text"), None);
    }

    /// (S1, v1.3) A user message with array content gives one user item per
    /// text block (an attachment submit's text was hidden before); images,
    /// tool results and a context summary give none.
    #[test]
    fn array_user_content_gives_its_text_blocks() {
        let msgs = vec![Arc::new(serde_json::json!({"role": "user", "content": [
            {"type": "text", "text": "look at this"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA=="}},
            {"type": "text", "text": "<context-summary>s</context-summary>"},
            {"type": "tool_result", "tool_use_id": "t", "content": "x"}
        ]}))];
        assert_eq!(display_items(&msgs), vec![DisplayItem::User { text: "look at this".into() }]);
    }

    /// (S1, v1.3) The `held` item's wire shape; a kind from a newer daemon
    /// decodes as `Unknown` instead of failing the whole tail.
    #[test]
    fn the_held_item_shape_and_unknown_kinds() {
        let item = DisplayItem::Held { source: HeldSource::Join, text: "<event>".into() };
        assert_eq!(
            serde_json::to_value(&item).unwrap(),
            serde_json::json!({"kind": "held", "source": "join", "text": "<event>"})
        );
        let digest: DisplayItem =
            serde_json::from_value(serde_json::json!({"kind": "held", "source": "digest", "text": "t"})).unwrap();
        assert_eq!(digest, DisplayItem::Held { source: HeldSource::Digest, text: "t".into() });
        let tail: DisplayTail = serde_json::from_value(serde_json::json!({
            "items": [{"kind": "user", "text": "a"}, {"kind": "later_kind", "x": 1}],
            "omitted": 0
        }))
        .unwrap();
        assert_eq!(tail.items, vec![DisplayItem::User { text: "a".into() }, DisplayItem::Unknown]);
    }
}
