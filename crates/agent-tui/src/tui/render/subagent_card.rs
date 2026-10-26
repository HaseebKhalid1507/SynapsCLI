//! Human rendering of subagent tool results (display-only).
//!
//! `subagent_collect` / `subagent_status` / `subagent_start` /
//! `subagent_resume` / `subagent_steer` return JSON meant for the model —
//! archive paths, authorization digests, catalog ids, terminal causes. The
//! transcript shows the user a card instead: status, name/handle, elapsed,
//! tool count, any failure reason, and the worker's output as markdown.
//! The `ToolResult` content (what the model received, and what copy
//! reconstructs from) is never touched.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde_json::Value;

use super::super::markdown::{render_markdown_spans, wrap_text};
use super::super::theme::THEME;
use super::super::transcript::{ChatMessage, TranscriptStore};

/// `3.4s`, `59.9s`, `2m 05s`, `1h 02m`.
pub(crate) fn fmt_secs(secs: f64) -> String {
    if secs < 60.0 {
        return format!("{secs:.1}s");
    }
    let s = secs.round() as u64;
    if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// Glyph, label and colour for a subagent status string.
pub(crate) fn status_badge(status: &str) -> (&'static str, String, Color) {
    let t = THEME.load();
    match status {
        "completed" => ("\u{2713}", "completed".into(), t.tool_result_ok),
        "running" => ("\u{25CF}", "running".into(), t.status_streaming),
        "failed" => ("\u{2717}", "failed".into(), t.error_color),
        "cancelled" => ("\u{2717}", "cancelled".into(), t.warning_color),
        "timed_out" => ("\u{2717}", "timed out".into(), t.warning_color),
        other => ("\u{00B7}", other.replace('_', " "), t.muted),
    }
}

/// The parsed model-facing completion sentence built by
/// `agent_engine::tools::subagent::finalize::build_completion_event`.
#[derive(Debug, PartialEq)]
pub(crate) struct Completion<'a> {
    pub(crate) name: &'a str,
    pub(crate) handle: &'a str,
    pub(crate) status: &'a str,
    pub(crate) secs: f64,
    pub(crate) reason: Option<&'a str>,
    pub(crate) preview: &'a str,
}

/// Parse "Subagent '{name}' ({handle}) finished with status '{status}' after
/// {secs}s.[ Reason: {r}] Call subagent_collect with handle_id "{handle}" to
/// retrieve the full result.[ Preview: {p}]". `None` for anything else —
/// callers then show the text verbatim.
pub(crate) fn parse_completion(text: &str) -> Option<Completion<'_>> {
    let rest = text.strip_prefix("Subagent '")?;
    let (name, rest) = rest.split_once("' (")?;
    let (handle, rest) = rest.split_once(") finished with status '")?;
    let (status, rest) = rest.split_once("' after ")?;
    let (secs, rest) = rest.split_once("s.")?;
    let secs: f64 = secs.parse().ok()?;
    let call =
        format!(" Call subagent_collect with handle_id \"{handle}\" to retrieve the full result.");
    let at = rest.find(&call)?;
    let reason = rest[..at]
        .strip_prefix(" Reason: ")
        .map(str::trim)
        .filter(|r| !r.is_empty());
    if reason.is_none() && !rest[..at].trim().is_empty() {
        return None;
    }
    let tail = &rest[at + call.len()..];
    let preview = match tail.strip_prefix(" Preview: ") {
        Some(p) => p.trim(),
        None if tail.trim().is_empty() => "",
        None => return None,
    };
    Some(Completion {
        name,
        handle,
        status,
        secs,
        reason,
        preview,
    })
}

impl TranscriptStore {
    /// Name and run time of a subagent handle, from what the transcript
    /// already shows: its completion event, or a start/status result that
    /// carried `agent_name`. Used to label `subagent_collect` cards, whose
    /// JSON carries neither.
    pub(super) fn subagent_facts(&self, handle: &str) -> (Option<String>, Option<f64>) {
        let (mut name, mut secs) = (None, None);
        for m in self.messages().iter().rev() {
            match &m.msg {
                ChatMessage::Event { source, text, .. } if source == "subagent" => {
                    if let Some(c) = parse_completion(text).filter(|c| c.handle == handle) {
                        return (Some(c.name.to_string()), Some(c.secs));
                    }
                }
                ChatMessage::ToolResult { content, .. }
                    if name.is_none() && content.starts_with('{') && content.contains(handle) =>
                {
                    if let Ok(v) = serde_json::from_str::<Value>(content) {
                        if v["handle_id"].as_str() == Some(handle) {
                            if let Some(n) = v["agent_name"].as_str() {
                                name = Some(n.to_string());
                                secs = secs.or(v["elapsed_secs"].as_f64());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        (name, secs)
    }
}

/// The human card body for a `subagent_*` tool result, or `None` when the
/// result is not a JSON object this knows (the caller then shows it raw).
/// Rows carry the card margin `m`; `facts` supplies a name/elapsed for a
/// handle when the JSON lacks them.
pub(super) fn tool_result_card(
    tool: &str,
    content: &str,
    m: &str,
    width: usize,
    msg_idx: usize,
    facts: impl Fn(&str) -> (Option<String>, Option<f64>),
) -> Option<Vec<Line<'static>>> {
    if !matches!(
        tool,
        "subagent_collect"
            | "subagent_status"
            | "subagent_start"
            | "subagent_resume"
            | "subagent_steer"
    ) {
        return None;
    }
    let v: Value = serde_json::from_str(content.trim()).ok()?;
    let obj = v.as_object()?;
    let t = THEME.load();
    let muted = Style::default().fg(t.muted);
    let pad = format!("{m}       ");
    let mut rows: Vec<Line<'static>> = Vec::new();

    // ── status line ─────────────────────────────────────────────────────
    let (glyph, label, color) = if let Some(ack) = obj.get("acknowledged").and_then(Value::as_bool)
    {
        if ack {
            ("\u{2713}", "steer delivered".to_string(), t.tool_result_ok)
        } else {
            (
                "\u{2717}",
                "steer not delivered".to_string(),
                t.warning_color,
            )
        }
    } else {
        status_badge(obj.get("status")?.as_str()?)
    };
    let handle = obj.get("handle_id").and_then(Value::as_str);
    let (known_name, known_secs) = handle.map(&facts).unwrap_or((None, None));
    let name = obj
        .get("agent_name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or(known_name);
    let secs = obj
        .get("elapsed_secs")
        .and_then(Value::as_f64)
        .or(known_secs);
    let mut facts_txt: Vec<String> = Vec::new();
    match (&name, handle) {
        (Some(n), Some(h)) => facts_txt.push(format!("{n} ({h})")),
        (Some(n), None) => facts_txt.push(n.clone()),
        (None, Some(h)) => facts_txt.push(h.to_string()),
        (None, None) => {}
    }
    if let Some(from) = obj.get("resumed_from").and_then(Value::as_str) {
        facts_txt.push(format!("resumed from {from}"));
    }
    if let Some(s) = secs {
        facts_txt.push(fmt_secs(s));
    }
    if let Some(n) = obj.get("tool_count").and_then(Value::as_u64) {
        facts_txt.push(format!("{n} tool{}", if n == 1 { "" } else { "s" }));
    }
    let mut head = vec![
        Span::raw(pad.clone()),
        Span::styled(
            format!("{glyph} {label}"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ];
    if !facts_txt.is_empty() {
        head.push(Span::styled(
            format!("  {}", facts_txt.join(" \u{00B7} ")),
            muted,
        ));
    }
    rows.push(Line::from(head));

    // ── reason / warning / user-relevant note ───────────────────────────
    let mut notes: Vec<(String, Color)> = Vec::new();
    if let Some(r) = obj.get("error").and_then(Value::as_str) {
        notes.push((format!("reason: {r}"), t.error_color));
    }
    if let Some(w) = obj.get("warning").and_then(Value::as_str) {
        notes.push((w.to_string(), t.warning_color));
    }
    if obj.get("status").and_then(Value::as_str) == Some("expired") {
        if let Some(n) = obj.get("note").and_then(Value::as_str) {
            notes.push((n.to_string(), t.muted));
        }
    }
    for (text, c) in notes {
        for w in wrap_text(&format!("{pad}{text}"), width) {
            rows.push(Line::from(Span::styled(w, Style::default().fg(c))));
        }
    }

    // ── worker output ───────────────────────────────────────────────────
    let output = ["output", "output_so_far", "partial_output"]
        .iter()
        .find_map(|k| obj.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .unwrap_or("");
    if !output.is_empty() {
        rows.push(Line::from(""));
        // render_markdown_spans indents two cells past its prefix, so
        // `m + 5` lands the body on the card's 7-cell content column.
        let md_prefix = format!("{m}     ");
        rows.extend(
            render_markdown_spans(output, &md_prefix, width, msg_idx)
                .into_iter()
                .map(|(l, _)| l),
        );
    }
    Some(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVENT: &str =
        "Subagent 'tag-lister' (sa_2) finished with status 'completed' after 3.4s. \
         Call subagent_collect with handle_id \"sa_2\" to retrieve the full result. \
         Preview: This repo has no git tags.";

    #[test]
    fn parses_completion_sentence() {
        let c = parse_completion(EVENT).unwrap();
        assert_eq!(
            c,
            Completion {
                name: "tag-lister",
                handle: "sa_2",
                status: "completed",
                secs: 3.4,
                reason: None,
                preview: "This repo has no git tags.",
            }
        );
        let failed = "Subagent 'x' (sa_9) finished with status 'failed' after 61.0s. Reason: boom \
             Call subagent_collect with handle_id \"sa_9\" to retrieve the full result. Preview: ";
        let c = parse_completion(failed).unwrap();
        assert_eq!(
            (c.status, c.reason, c.preview),
            ("failed", Some("boom"), "")
        );
        assert!(parse_completion("Subagent 'x' is steering").is_none());
    }

    #[test]
    fn fmt_secs_is_human() {
        assert_eq!(fmt_secs(3.44), "3.4s");
        assert_eq!(fmt_secs(125.0), "2m 05s");
        assert_eq!(fmt_secs(3720.0), "1h 02m");
    }

    use super::super::super::transcript::{LineMeta, RenderCtx};

    fn rows_of(store: &TranscriptStore, idx: usize, width: usize) -> Vec<String> {
        let ctx = RenderCtx {
            spinner_frame: 0,
            streaming: false,
            agent_name: "agent",
        };
        let entry = store.render_message_lines(idx, width, &ctx);
        assert_eq!(entry.lines().len(), entry.meta_slice().len());
        assert!(
            entry
                .meta_slice()
                .iter()
                .all(|m| !matches!(m, LineMeta::Content { .. })),
            "lock L2: tool-card rows are never Content"
        );
        entry
            .lines()
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    const COLLECT_JSON: &str = r#"{"archiveError":null,"archive_path":"/home/someone/.synaps-cli/subagent-history/274984/sa_2.json","authorization":{"catalog_digest":"1427a3975a13300c","catalog_snapshot_id":"catalog-1427a3975a13300c","correlation_id":"sa_2","cross_provider_grant_id":null,"network_attempted":false,"policy_digest":"523998a9586625aff0a6e834afe056e7826a41a78a68d852","selection_source":"foreground_inheritance"},"collected":false,"handle_id":"sa_2","model":"anthropic/claude-sonnet-5-5","output":"This repo has **no** git tags, so `git tag` returned\nnothing.","status":"completed","terminal_cause":{"category":"completed","code":"completed","correlation_id":"sa_2","network_attempted":true,"safe_message":"worker completed","stage":"inference"}}"#;

    fn push_call(store: &mut TranscriptStore, id: &str, tool: &str, input: &str, result: &str) {
        store.push_msg(ChatMessage::ToolUse {
            tool_id: id.into(),
            tool_name: tool.into(),
            input: input.into(),
        });
        store.push_msg(ChatMessage::ToolResult {
            tool_id: id.into(),
            content: result.into(),
            elapsed_ms: Some(1),
        });
    }

    #[test]
    fn collect_card_is_human_not_json() {
        let mut store = TranscriptStore::new(crate::tui::clock::TuiClock::real());
        store.push_msg(ChatMessage::Event {
            source: "subagent".into(),
            severity: "high".into(),
            text: EVENT.into(),
        });
        push_call(
            &mut store,
            "c1",
            "subagent_collect",
            r#"{"handle_id":"sa_2","reconciled":true}"#,
            COLLECT_JSON,
        );
        for width in [60usize, 100] {
            let rows = rows_of(&store, 2, width).join("\n");
            for leak in ["archive_path", "authorization", "/home/", "catalog", "{\""] {
                assert!(!rows.contains(leak), "w={width} leaked {leak:?}:\n{rows}");
            }
            assert!(rows.contains("\u{2713} completed"), "{rows}");
            assert!(rows.contains("tag-lister (sa_2)"), "{rows}");
            assert!(rows.contains("3.4s"), "{rows}");
            assert!(
                rows.contains("This repo has no git tags"),
                "markdown rendered: {rows}"
            );
            assert!(!rows.contains("(1 lines)"), "{rows}");
        }
        // The model-facing content is untouched.
        match &store.messages()[2].msg {
            ChatMessage::ToolResult { content, .. } => assert_eq!(content, COLLECT_JSON),
            _ => unreachable!(),
        }
    }

    #[test]
    fn status_card_shows_tool_count_and_failure_reason() {
        let mut store = TranscriptStore::new(crate::tui::clock::TuiClock::real());
        push_call(
            &mut store,
            "s1",
            "subagent_status",
            r#"{"handle_id":"sa_1"}"#,
            r#"{"handle_id":"sa_1","agent_name":"rs-counter","status":"failed","error":"provider refused","partial_output":"","elapsed_secs":12.5,"tool_count":2,"model":"m","terminal_cause":{},"authorization":{"policy_digest":"abc"},"archiveError":null,"output_length":0}"#,
        );
        let rows = rows_of(&store, 1, 80).join("\n");
        assert!(rows.contains("\u{2717} failed"), "{rows}");
        assert!(
            rows.contains("rs-counter (sa_1) \u{00B7} 12.5s \u{00B7} 2 tools"),
            "{rows}"
        );
        assert!(rows.contains("reason: provider refused"), "{rows}");
        assert!(!rows.contains("policy_digest"), "{rows}");
    }

    #[test]
    fn non_json_or_other_tools_fall_back_to_raw() {
        let mut store = TranscriptStore::new(crate::tui::clock::TuiClock::real());
        push_call(
            &mut store,
            "b1",
            "bash",
            r#"{"command":"x"}"#,
            r#"{"status":"completed"}"#,
        );
        push_call(
            &mut store,
            "c2",
            "subagent_collect",
            "{}",
            "plain text result",
        );
        assert!(rows_of(&store, 1, 80)
            .join("\n")
            .contains(r#"{"status":"completed"}"#));
        assert!(rows_of(&store, 3, 80)
            .join("\n")
            .contains("plain text result"));
    }
}
