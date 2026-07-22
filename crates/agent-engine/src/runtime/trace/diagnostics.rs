//! Cache-prefix diagnostics (Task 12, spec §6.6) and the `/context` report.
//!
//! ## Canonicalization (documented approximations)
//!
//! The tools-prefix / system-prefix / history-tail digests are keyed HMAC
//! over **canonical component bytes**, derived from the same structural
//! inputs the request builder used — the registered tool schemas, the
//! caller-supplied system prompt, and the annotated message array:
//!
//! - **tools prefix**: for each tool, in send order:
//!   `name ++ 0x1f ++ canonical-JSON(input_schema) ++ 0x1e`. This matches
//!   stable-prefix semantics (a tool added, removed, reordered, or with a
//!   changed schema changes the digest) but is *not* the exact wire slice —
//!   provider-specific envelope keys and separators are excluded by design,
//!   so the digest is stable across cosmetic wire changes.
//! - **system prefix**: the exact UTF-8 bytes of the system prompt string.
//! - **history tail**: for each message strictly *after* the last
//!   message-level cache boundary (`cache_control` annotation this process
//!   applied), `canonical-JSON(message) ++ 0x1e`. When no boundary exists
//!   the tail is the whole history. Canonical JSON is `serde_json::to_vec`
//!   of the already-built message value — the same value the wire body was
//!   serialized from, but not byte-sliced out of the wire buffer.
//!
//! None of these bytes are retained: only lengths and keyed digests
//! (installation-scoped HMAC key, see `trace::key`) leave this module.
//!
//! ## Session snapshot
//!
//! [`CacheSnapshotStore`] keeps the previous emitted request's component
//! digests (bounded metadata only — digests, byte lengths, tool IDs) inside
//! the session's `TraceContext`. Compare-and-update is atomic under one
//! mutex, per emitted request: a retried identical request compares equal
//! (all segments `Unchanged`); an intentional tool order/schema change is
//! flagged with the precise tool IDs. No provider or prompt content is
//! ever stored.

use super::key::{keyed_digest, DigestDomain, TraceDigestKey};
use super::types::{CacheSegmentDelta, PrefixMeta, SegmentChange, TraceId};
use serde_json::Value;
use std::sync::Mutex;

/// Record separator between canonical components.
const RS: u8 = 0x1e;
/// Unit separator between a tool name and its schema bytes.
const US: u8 = 0x1f;

// --- Canonical component bytes ---

/// Canonical tools-prefix bytes (see module docs for the exact recipe).
pub fn tools_prefix_bytes(tools_schema: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    for tool in tools_schema {
        if let Some(name) = tool["name"].as_str() {
            out.extend_from_slice(name.as_bytes());
        }
        out.push(US);
        if let Ok(schema) = serde_json::to_vec(&tool["input_schema"]) {
            out.extend_from_slice(&schema);
        }
        out.push(RS);
    }
    out
}

/// Canonical system-prefix bytes: the system prompt itself.
pub fn system_prefix_bytes(system_prompt: &str) -> Vec<u8> {
    system_prompt.as_bytes().to_vec()
}

/// Index of the first history-tail message: one past the last message that
/// carries a message-level cache boundary annotation. With no boundary the
/// whole history is the tail.
fn history_tail_start(messages: &[crate::SharedMessage]) -> usize {
    messages
        .iter()
        .rposition(|m| {
            m["content"]
                .as_array()
                .and_then(|blocks| blocks.last())
                .and_then(|b| b.get("cache_control"))
                .is_some()
        })
        .map(|idx| idx + 1)
        .unwrap_or(0)
}

/// Canonical history-tail bytes (see module docs).
pub fn history_tail_bytes(messages: &[crate::SharedMessage]) -> Vec<u8> {
    let start = history_tail_start(messages);
    let mut out = Vec::new();
    for message in &messages[start..] {
        if let Ok(bytes) = serde_json::to_vec(&**message) {
            out.extend_from_slice(&bytes);
        }
        out.push(RS);
    }
    out
}

// --- Bounded per-segment snapshot ---

/// Bounded metadata snapshot of one segment: digest + canonical byte length.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SegmentSnapshot {
    digest: super::key::ComponentDigest,
    byte_len: u64,
}

impl SegmentSnapshot {
    fn from_meta(meta: &PrefixMeta) -> Self {
        Self {
            digest: meta.digest.clone(),
            byte_len: meta.byte_len,
        }
    }
}

/// Bounded metadata identity of one tool for order/schema comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolSnapshot {
    stable_id: TraceId,
    schema_digest: super::key::ComponentDigest,
}

#[derive(Debug, Default)]
struct SnapshotInner {
    tools: Option<SegmentSnapshot>,
    system: Option<SegmentSnapshot>,
    history_tail: Option<SegmentSnapshot>,
    tool_list: Vec<ToolSnapshot>,
    /// Last computed delta + prefix metas, retained (bounded) for `/context`.
    last: Option<CacheActivity>,
}

/// Latest cache diagnostics, retained for the `/context` report. Bounded
/// metadata only (digests, byte lengths, enums, tool IDs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheActivity {
    pub tools_prefix: Option<PrefixMeta>,
    pub system_prefix: Option<PrefixMeta>,
    pub history_tail: Option<PrefixMeta>,
    pub delta: CacheSegmentDelta,
}

/// Computed diagnostics for one outgoing request.
#[derive(Debug, Clone, Default)]
pub struct CacheDiagnostics {
    pub tools_prefix: Option<PrefixMeta>,
    pub system_prefix: Option<PrefixMeta>,
    pub history_tail: Option<PrefixMeta>,
    pub delta: Option<CacheSegmentDelta>,
}

/// Session-scoped previous-component snapshot (see module docs). Shared via
/// `Arc` inside `TraceContext`; all mutation is under one internal mutex so
/// compare-and-update is atomic per emitted request.
#[derive(Debug, Default)]
pub struct CacheSnapshotStore {
    inner: Mutex<SnapshotInner>,
}

impl CacheSnapshotStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Compare the current request's components against the previous
    /// snapshot, update the snapshot, and return the diagnostics. Requires
    /// the digest key — without it no digests exist to compare, so the
    /// caller gets `CacheDiagnostics::default()` and the snapshot is left
    /// untouched (never cleared: a transient key failure must not turn the
    /// next successful comparison into a spurious `New`).
    pub fn compare_and_update(
        &self,
        key: Option<&TraceDigestKey>,
        tools_schema: &[Value],
        system_prompt: Option<&str>,
        messages: &[crate::SharedMessage],
    ) -> CacheDiagnostics {
        let Some(key) = key else {
            return CacheDiagnostics::default();
        };

        let tools_bytes = tools_prefix_bytes(tools_schema);
        let tools_prefix = (!tools_schema.is_empty()).then(|| PrefixMeta {
            byte_len: tools_bytes.len() as u64,
            digest: keyed_digest(key, DigestDomain::ToolsPrefix, &tools_bytes),
        });
        let system_prefix = system_prompt.filter(|s| !s.is_empty()).map(|s| {
            let bytes = system_prefix_bytes(s);
            PrefixMeta {
                byte_len: bytes.len() as u64,
                digest: keyed_digest(key, DigestDomain::SystemPrefix, &bytes),
            }
        });
        let tail_bytes = history_tail_bytes(messages);
        let history_tail = (!messages.is_empty()).then(|| PrefixMeta {
            byte_len: tail_bytes.len() as u64,
            digest: keyed_digest(key, DigestDomain::HistoryTail, &tail_bytes),
        });
        let tool_list: Vec<ToolSnapshot> = tools_schema
            .iter()
            .filter_map(|tool| {
                let name = tool["name"].as_str()?;
                let stable_id = TraceId::new(name).ok()?;
                let schema = serde_json::to_vec(&tool["input_schema"]).ok()?;
                Some(ToolSnapshot {
                    stable_id,
                    schema_digest: keyed_digest(key, DigestDomain::ToolSchema, &schema),
                })
            })
            .collect();

        let mut inner = self.inner.lock().expect("cache snapshot store poisoned");
        let had_previous = inner.tools.is_some()
            || inner.system.is_some()
            || inner.history_tail.is_some()
            || !inner.tool_list.is_empty();

        let delta = if had_previous {
            let tools_change = segment_change(inner.tools.as_ref(), tools_prefix.as_ref());
            let system_change = segment_change(inner.system.as_ref(), system_prefix.as_ref());
            let tail_change = segment_change(inner.history_tail.as_ref(), history_tail.as_ref());
            let (changed_tool_ids, tool_order_changed) =
                tool_delta(&inner.tool_list, &tool_list, tools_change);
            let (reused, recomputed) = reuse_estimate(&[
                (tools_change, tools_prefix.as_ref()),
                (system_change, system_prefix.as_ref()),
                (tail_change, history_tail.as_ref()),
            ]);
            CacheSegmentDelta {
                tools: tools_change,
                system: system_change,
                history_tail: tail_change,
                changed_tool_ids,
                tool_order_changed,
                estimated_reused_bytes: Some(reused),
                estimated_recomputed_bytes: Some(recomputed),
            }
        } else {
            CacheSegmentDelta {
                tools: tools_prefix.as_ref().map(|_| SegmentChange::New),
                system: system_prefix.as_ref().map(|_| SegmentChange::New),
                history_tail: history_tail.as_ref().map(|_| SegmentChange::New),
                changed_tool_ids: Vec::new(),
                tool_order_changed: false,
                estimated_reused_bytes: Some(0),
                estimated_recomputed_bytes: Some(
                    [&tools_prefix, &system_prefix, &history_tail]
                        .iter()
                        .filter_map(|m| m.as_ref().map(|m| m.byte_len))
                        .sum(),
                ),
            }
        };

        inner.tools = tools_prefix.as_ref().map(SegmentSnapshot::from_meta);
        inner.system = system_prefix.as_ref().map(SegmentSnapshot::from_meta);
        inner.history_tail = history_tail.as_ref().map(SegmentSnapshot::from_meta);
        inner.tool_list = tool_list;
        inner.last = Some(CacheActivity {
            tools_prefix: tools_prefix.clone(),
            system_prefix: system_prefix.clone(),
            history_tail: history_tail.clone(),
            delta: delta.clone(),
        });

        CacheDiagnostics {
            tools_prefix,
            system_prefix,
            history_tail,
            delta: Some(delta),
        }
    }

    /// Latest computed cache activity (for `/context`).
    pub fn last_activity(&self) -> Option<CacheActivity> {
        self.inner
            .lock()
            .expect("cache snapshot store poisoned")
            .last
            .clone()
    }
}

fn segment_change(
    previous: Option<&SegmentSnapshot>,
    current: Option<&PrefixMeta>,
) -> Option<SegmentChange> {
    match (previous, current) {
        (None, None) => None,
        (None, Some(_)) | (Some(_), None) => Some(SegmentChange::New),
        (Some(prev), Some(cur)) if prev.digest == cur.digest => Some(SegmentChange::Unchanged),
        _ => Some(SegmentChange::Changed),
    }
}

/// Changed tool IDs (schema changed, added, or removed) and whether the
/// same tool set was merely reordered.
fn tool_delta(
    previous: &[ToolSnapshot],
    current: &[ToolSnapshot],
    tools_change: Option<SegmentChange>,
) -> (Vec<TraceId>, bool) {
    if tools_change != Some(SegmentChange::Changed) {
        return (Vec::new(), false);
    }
    let mut changed: Vec<TraceId> = Vec::new();
    for cur in current {
        match previous.iter().find(|p| p.stable_id == cur.stable_id) {
            None => changed.push(cur.stable_id.clone()),
            Some(prev) if prev.schema_digest != cur.schema_digest => {
                changed.push(cur.stable_id.clone())
            }
            Some(_) => {}
        }
    }
    for prev in previous {
        if !current.iter().any(|c| c.stable_id == prev.stable_id) {
            changed.push(prev.stable_id.clone());
        }
    }
    // Same multiset of (id, schema) pairs in a different order?
    let order_changed = changed.is_empty() && previous != current;
    (changed, order_changed)
}

fn reuse_estimate(segments: &[(Option<SegmentChange>, Option<&PrefixMeta>)]) -> (u64, u64) {
    let mut reused = 0u64;
    let mut recomputed = 0u64;
    for (change, meta) in segments {
        let Some(meta) = meta else { continue };
        match change {
            Some(SegmentChange::Unchanged) => reused += meta.byte_len,
            Some(SegmentChange::Changed) | Some(SegmentChange::New) => recomputed += meta.byte_len,
            None => {}
        }
    }
    (reused, recomputed)
}

// --- `/context` report ---

/// Why a `/context` line has no number: honest provenance, never a
/// fabricated zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportValue {
    Count(u64),
    /// The owning surface did not provide this data to the report.
    Unavailable,
}

impl std::fmt::Display for ReportValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReportValue::Count(n) => write!(f, "{n}"),
            ReportValue::Unavailable => f.write_str("unavailable"),
        }
    }
}

/// Structured `/context` report: counts, bytes, enums, and counters only —
/// never prompt/tool/skill/memory content.
#[derive(Debug, Clone)]
pub struct ContextReport {
    pub model: String,
    pub system_prompt_bytes: ReportValue,
    pub tool_count: ReportValue,
    /// History message count, when the calling surface owns and provided
    /// the conversation; `Unavailable` otherwise (the runtime does not own
    /// session history — it is never fabricated here).
    pub history_messages: ReportValue,
    pub history_bytes: ReportValue,
    /// Loaded skills / memory documents, when enumerable from runtime
    /// state. `Unavailable` (with that provenance printed) when the
    /// runtime cannot enumerate them without owning surface state.
    pub loaded_skills: ReportValue,
    pub loaded_memories: ReportValue,
    pub cache: Option<CacheActivity>,
    pub trace_enabled: bool,
    pub writer_stats: Option<crate::runtime::telemetry::WriterStats>,
    pub degraded_records: u64,
}

fn fmt_change(change: Option<SegmentChange>) -> &'static str {
    match change {
        Some(SegmentChange::Unchanged) => "unchanged",
        Some(SegmentChange::Changed) => "changed",
        Some(SegmentChange::New) => "new",
        None => "absent",
    }
}

impl ContextReport {
    /// Render for terminal display. Metadata only — no content.
    pub fn render(&self) -> String {
        let mut out = String::new();
        use std::fmt::Write as _;
        let _ = writeln!(out, "context — model {}", self.model);
        let _ = writeln!(out, "  system prompt: {} bytes", self.system_prompt_bytes);
        let _ = writeln!(out, "  tools: {}", self.tool_count);
        let _ = writeln!(
            out,
            "  history: {} messages, {} bytes",
            self.history_messages, self.history_bytes
        );
        let _ = writeln!(
            out,
            "  loaded skills: {}, memories: {}",
            self.loaded_skills, self.loaded_memories
        );
        match &self.cache {
            Some(activity) => {
                let d = &activity.delta;
                let _ = writeln!(
                    out,
                    "  cache: tools {}, system {}, history tail {}",
                    fmt_change(d.tools),
                    fmt_change(d.system),
                    fmt_change(d.history_tail),
                );
                if !d.changed_tool_ids.is_empty() {
                    let ids: Vec<&str> = d.changed_tool_ids.iter().map(|t| t.as_str()).collect();
                    let _ = writeln!(out, "  changed tools: {}", ids.join(", "));
                }
                if d.tool_order_changed {
                    let _ = writeln!(out, "  tool order changed (prefix invalidated)");
                }
                if let (Some(reused), Some(recomputed)) =
                    (d.estimated_reused_bytes, d.estimated_recomputed_bytes)
                {
                    let _ = writeln!(
                        out,
                        "  estimated reuse: {reused} bytes reused, {recomputed} bytes recomputed",
                    );
                }
            }
            None => {
                let _ = writeln!(
                    out,
                    "  cache: no diagnostics yet (tracing {} this session)",
                    if self.trace_enabled {
                        "enabled, no traced request"
                    } else {
                        "disabled"
                    }
                );
            }
        }
        match self.writer_stats {
            Some(stats) => {
                let _ = writeln!(
                    out,
                    "  trace writer: {} enqueued, {} written, {} dropped; {} degraded records",
                    stats.enqueued, stats.written, stats.dropped, self.degraded_records,
                );
            }
            None => {
                let _ = writeln!(
                    out,
                    "  trace writer: off; {} degraded records",
                    self.degraded_records
                );
            }
        }
        out.trim_end().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn test_key() -> TraceDigestKey {
        let dir = tempfile::tempdir().expect("tempdir");
        super::super::key::load_or_create_digest_key_at(&dir.path().join("k.key"))
            .expect("test key")
    }

    fn tool(name: &str, schema: Value) -> Value {
        serde_json::json!({"name": name, "input_schema": schema})
    }

    fn msg(role: &str, text: &str) -> crate::SharedMessage {
        Arc::new(serde_json::json!({"role": role, "content": text}))
    }

    #[test]
    fn first_request_is_new_then_identical_retry_is_unchanged() {
        let key = test_key();
        let store = CacheSnapshotStore::new();
        let tools = vec![tool("alpha", serde_json::json!({"type": "object"}))];
        let messages = vec![msg("user", "hi")];

        let first = store.compare_and_update(Some(&key), &tools, Some("sys"), &messages);
        let d = first.delta.expect("delta");
        assert_eq!(d.tools, Some(SegmentChange::New));
        assert_eq!(d.system, Some(SegmentChange::New));
        assert_eq!(d.history_tail, Some(SegmentChange::New));
        assert_eq!(d.estimated_reused_bytes, Some(0));

        // Retried identical attempt: everything unchanged.
        let second = store.compare_and_update(Some(&key), &tools, Some("sys"), &messages);
        let d = second.delta.expect("delta");
        assert_eq!(d.tools, Some(SegmentChange::Unchanged));
        assert_eq!(d.system, Some(SegmentChange::Unchanged));
        assert_eq!(d.history_tail, Some(SegmentChange::Unchanged));
        assert!(d.changed_tool_ids.is_empty());
        assert!(!d.tool_order_changed);
        assert_eq!(d.estimated_recomputed_bytes, Some(0));
        assert!(d.estimated_reused_bytes.unwrap() > 0);
    }

    #[test]
    fn tool_order_change_flags_prefix_change_without_changed_ids() {
        let key = test_key();
        let store = CacheSnapshotStore::new();
        let a = tool("alpha", serde_json::json!({"type": "object"}));
        let b = tool("beta", serde_json::json!({"type": "object"}));
        let messages = vec![msg("user", "hi")];

        store.compare_and_update(Some(&key), &[a.clone(), b.clone()], None, &messages);
        let swapped = store.compare_and_update(Some(&key), &[b, a], None, &messages);
        let d = swapped.delta.expect("delta");
        assert_eq!(d.tools, Some(SegmentChange::Changed));
        assert!(d.changed_tool_ids.is_empty(), "same tools, only reordered");
        assert!(d.tool_order_changed);
    }

    #[test]
    fn tool_schema_change_names_the_precise_tool() {
        let key = test_key();
        let store = CacheSnapshotStore::new();
        let a1 = tool("alpha", serde_json::json!({"type": "object"}));
        let a2 = tool(
            "alpha",
            serde_json::json!({"type": "object", "properties": {"x": {}}}),
        );
        let b = tool("beta", serde_json::json!({"type": "object"}));
        let messages = vec![msg("user", "hi")];

        store.compare_and_update(Some(&key), &[a1, b.clone()], None, &messages);
        let changed = store.compare_and_update(Some(&key), &[a2, b], None, &messages);
        let d = changed.delta.expect("delta");
        assert_eq!(d.tools, Some(SegmentChange::Changed));
        assert_eq!(
            d.changed_tool_ids
                .iter()
                .map(|t| t.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha"]
        );
        assert!(!d.tool_order_changed);
    }

    #[test]
    fn prefix_digests_are_keyed_hmac_over_canonical_bytes() {
        let key = test_key();
        let store = CacheSnapshotStore::new();
        let tools = vec![tool("alpha", serde_json::json!({"type": "object"}))];
        let messages = vec![msg("user", "hello")];
        let diag = store.compare_and_update(Some(&key), &tools, Some("sys"), &messages);

        let expected_tools =
            keyed_digest(&key, DigestDomain::ToolsPrefix, &tools_prefix_bytes(&tools));
        assert_eq!(diag.tools_prefix.unwrap().digest, expected_tools);

        let expected_system = keyed_digest(&key, DigestDomain::SystemPrefix, b"sys");
        assert_eq!(diag.system_prefix.unwrap().digest, expected_system);

        let expected_tail = keyed_digest(
            &key,
            DigestDomain::HistoryTail,
            &history_tail_bytes(&messages),
        );
        assert_eq!(diag.history_tail.unwrap().digest, expected_tail);
    }

    #[test]
    fn history_tail_excludes_messages_up_to_last_cache_boundary() {
        let cached: crate::SharedMessage = Arc::new(serde_json::json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "old", "cache_control": {"type": "ephemeral"}}
            ]
        }));
        let fresh = msg("user", "new");
        let tail = history_tail_bytes(&[cached.clone(), fresh.clone()]);
        let only_fresh = history_tail_bytes(&[fresh.clone()]);
        // The tail after the boundary is exactly the fresh message.
        assert_eq!(tail, only_fresh);
        // Without a boundary, the whole history is the tail.
        let full = history_tail_bytes(&[cached, fresh]);
        assert_eq!(full, only_fresh);
    }

    #[test]
    fn missing_key_yields_no_diagnostics_and_preserves_snapshot() {
        let key = test_key();
        let store = CacheSnapshotStore::new();
        let tools = vec![tool("alpha", serde_json::json!({"type": "object"}))];
        let messages = vec![msg("user", "hi")];
        store.compare_and_update(Some(&key), &tools, None, &messages);

        let degraded = store.compare_and_update(None, &tools, None, &messages);
        assert!(degraded.delta.is_none());
        assert!(degraded.tools_prefix.is_none());

        // Snapshot survived the degraded request: next comparison is still
        // against the first request, not spuriously New.
        let after = store.compare_and_update(Some(&key), &tools, None, &messages);
        assert_eq!(after.delta.unwrap().tools, Some(SegmentChange::Unchanged));
    }

    #[test]
    fn cache_meta_without_new_fields_still_deserializes() {
        // Backward compatibility: a record written before Task 12.
        let old = serde_json::json!({"boundaries": []});
        let meta: super::super::types::CacheMeta =
            serde_json::from_value(old).expect("old CacheMeta parses");
        assert!(meta.history_tail.is_none());
        assert!(meta.delta.is_none());
    }

    #[test]
    fn context_report_render_contains_no_content_sentinel() {
        let secret = "SENTINEL-SECRET-9f8e7d";
        // Build a report from inputs that contain the sentinel; only counts
        // and digests may surface.
        let key = test_key();
        let store = CacheSnapshotStore::new();
        let tools = vec![tool(
            "alpha",
            serde_json::json!({"type": "object", "d": secret}),
        )];
        let messages = vec![msg("user", secret)];
        store.compare_and_update(Some(&key), &tools, Some(secret), &messages);
        let report = ContextReport {
            model: "anthropic/claude-test".to_string(),
            system_prompt_bytes: ReportValue::Count(secret.len() as u64),
            tool_count: ReportValue::Count(1),
            history_messages: ReportValue::Count(1),
            history_bytes: ReportValue::Count(42),
            loaded_skills: ReportValue::Unavailable,
            loaded_memories: ReportValue::Unavailable,
            cache: store.last_activity(),
            trace_enabled: true,
            writer_stats: None,
            degraded_records: 0,
        };
        let rendered = report.render();
        assert!(!rendered.contains(secret), "sentinel leaked: {rendered}");
        assert!(rendered.contains("unavailable"));
        assert!(rendered.contains("cache:"));
    }
}
