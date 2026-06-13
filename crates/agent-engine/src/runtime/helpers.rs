use serde_json::{json, Value};
use tokio::sync::mpsc;
use super::types::{StreamEvent, AgentEvent};
use crate::core::config::CacheTtl;
use crate::truncate_str;

/// Where a cache_control marker sits in the request body.
/// The API's logical emission order is tools → system → messages, so
/// StablePrefix markers always precede the MessageTail marker — this ordering
/// is what makes the Hybrid combination legal under Anthropic's rule that
/// longer-TTL breakpoints must precede shorter-TTL ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MarkerSite {
    /// Tool marker, OAuth system marker, API-key system marker.
    StablePrefix,
    /// `annotate_cache_breakpoint`'s last-message marker.
    MessageTail,
}

/// The single source of the cache_control JSON value. Bare ephemeral (5m)
/// omits `ttl` entirely — byte-identical to today's payloads, so the
/// default path cannot invalidate existing cached prefixes.
///
/// Cost trade-off (honest numbers, not marketing):
/// - 5m:     1.25× write everywhere. Wins for rapid-fire sessions.
/// - 1h:     2.0× write everywhere. Wins for SPARSE long sessions — note
///           that under Hybrid, every >5m gap forces a 5m re-write of the
///           message tail, and that tail write covers the WHOLE conversation
///           since the system breakpoint, not just the increment. Uniform 1h
///           avoids that repeated full-tail re-write.
/// - Hybrid: 2.0× write on the stable prefix (tools+system, written rarely),
///           1.25× on the message tail (written every turn). Wins for
///           bursty / medium-gap cadence: cheap per-turn writes while the
///           expensive prefix survives gaps up to 1h.
pub(super) fn cache_control_value(ttl: CacheTtl, site: MarkerSite) -> Value {
    match (ttl, site) {
        (CacheTtl::FiveMinutes, _) => json!({"type": "ephemeral"}),
        (CacheTtl::OneHour, _) => json!({"type": "ephemeral", "ttl": "1h"}),
        (CacheTtl::Hybrid, MarkerSite::StablePrefix) => json!({"type": "ephemeral", "ttl": "1h"}),
        (CacheTtl::Hybrid, MarkerSite::MessageTail) => json!({"type": "ephemeral"}),
    }
}

pub(super) struct HelperMethods;

impl HelperMethods {
    /// Drain all pending steering messages from the channel and inject them
    /// into the conversation as user messages. Returns true if any were injected.
    pub(super) fn drain_steering(
        steering_rx: &mut Option<mpsc::UnboundedReceiver<String>>,
        messages: &mut Vec<Value>,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) -> bool {
        let rx = match steering_rx.as_mut() {
            Some(rx) => rx,
            None => return false,
        };

        let mut injected = false;
        while let Ok(msg) = rx.try_recv() {
            tracing::info!("Steering message injected: {}", truncate_str(&msg, 80));
            let _ = tx.send(StreamEvent::Agent(AgentEvent::SteeringDelivered { message: msg.clone() }));
            messages.push(json!({"role": "user", "content": msg}));
            injected = true;
        }
        injected
    }

    /// Strip invalid thinking blocks from assistant messages before sending to the API.
    ///
    /// Anthropic rejects any `{"type": "thinking", ...}` block whose `thinking` field
    /// is missing or empty:
    ///
    /// > messages.N.content.M.thinking: each thinking block must contain thinking
    ///
    /// And rejects empty text blocks:
    ///
    /// > messages: text content blocks must be non-empty
    ///
    /// These can sneak in from (a) older sessions persisted before the streaming
    /// accumulator was hardened, (b) redacted-thinking blocks that lost their data, or
    /// (c) any future provider quirk. We drop them defensively so a single bad block
    /// can't permanently brick a session.
    ///
    /// Algorithm:
    ///   1. For each assistant message, retain only valid (`thinking` non-empty,
    ///      `redacted_thinking` data non-empty, or any other type).
    ///   2. Also drop any text blocks that are empty/whitespace-only — those would
    ///      trigger the "text content blocks must be non-empty" error.
    ///   3. If an assistant message ends up with no content at all, mark it for
    ///      removal — it produced no real output and is not safe to ship as `[]`
    ///      (the API rejects empty content arrays too).
    ///   4. Remove the marked messages, and merge any resulting consecutive
    ///      same-role messages so we don't violate Anthropic's alternation rule.
    pub(super) fn sanitize_thinking_blocks(messages: &mut Vec<Value>) {
        // Pass 1: filter blocks within each assistant message.
        let mut to_remove: Vec<usize> = Vec::new();
        for (i, msg) in messages.iter_mut().enumerate() {
            if msg["role"].as_str() != Some("assistant") {
                continue;
            }
            let content = match msg["content"].as_array_mut() {
                Some(c) => c,
                None => continue,
            };
            content.retain(|block| {
                match block["type"].as_str() {
                    Some("thinking") => block["thinking"]
                        .as_str()
                        .map(|s| !s.is_empty())
                        .unwrap_or(false),
                    Some("redacted_thinking") => block["data"]
                        .as_str()
                        .map(|s| !s.is_empty())
                        .unwrap_or(false),
                    Some("text") => block["text"]
                        .as_str()
                        .map(|s| !s.is_empty())
                        .unwrap_or(false),
                    _ => true,
                }
            });
            if content.is_empty() {
                // No salvageable content. The API rejects empty content arrays
                // and empty text placeholders alike, so we must drop the whole message.
                to_remove.push(i);
            }
        }

        // Pass 2: drop the empty assistant messages (back-to-front so indices stay valid).
        for &i in to_remove.iter().rev() {
            messages.remove(i);
        }

        // Pass 3: merge any consecutive same-role messages that now sit adjacent.
        // Dropping an assistant message between two user messages would otherwise
        // violate Anthropic's strict role-alternation rule.
        let mut i = 0;
        while i + 1 < messages.len() {
            let same_role = messages[i]["role"] == messages[i + 1]["role"];
            if same_role {
                // Coerce both contents to arrays, then concatenate.
                let next = messages.remove(i + 1);
                let next_blocks = Self::coerce_content_to_blocks(next["content"].clone());
                let current_blocks = Self::coerce_content_to_blocks(messages[i]["content"].clone());
                let mut merged = current_blocks;
                merged.extend(next_blocks);
                messages[i]["content"] = Value::Array(merged);
            } else {
                i += 1;
            }
        }
    }

    /// Normalize a `content` value to a Vec of content blocks. Anthropic accepts
    /// either a string or an array; we always want an array for merge operations.
    fn coerce_content_to_blocks(content: Value) -> Vec<Value> {
        match content {
            Value::String(s) if !s.is_empty() => vec![json!({"type": "text", "text": s})],
            Value::String(_) => Vec::new(),
            Value::Array(a) => a,
            _ => Vec::new(),
        }
    }

    /// Annotate a cache breakpoint on the last message (single-last strategy).
    /// Per S204 benchmarks, single-last matches sliding-4 performance (96–97% hit
    /// vs 96.7%) — a single stationary marker on the most recent message maximizes
    /// the stable cacheable prefix. With no old markers to prune, the
    /// prefix-invalidation bug class is eliminated entirely.
    ///
    /// The marker is the message-tail site: bare 5m under both `FiveMinutes`
    /// and `Hybrid`, `"ttl":"1h"` only under uniform `OneHour`.
    pub(super) fn annotate_cache_breakpoint(messages: &mut [Value], ttl: CacheTtl) {
        let Some(last) = messages.last_mut() else { return };

        // Coerce raw string content into a block array so we can attach cache_control.
        if let Some(text) = last["content"].as_str().map(str::to_owned) {
            last["content"] = json!([{"type": "text", "text": text}]);
        }

        if let Some(block) = last["content"].as_array_mut().and_then(|c| c.last_mut()) {
            block["cache_control"] = cache_control_value(ttl, MarkerSite::MessageTail);
        }
    }

    /// Mark the last tool in `body["tools"]` so all tool schemas are cached.
    /// Stable-prefix site — carries `"ttl":"1h"` under OneHour and Hybrid.
    pub(super) fn mark_last_tool(body: &mut Value, ttl: CacheTtl) {
        if let Some(tool_list) = body["tools"].as_array_mut() {
            if let Some(last_tool) = tool_list.last_mut() {
                last_tool["cache_control"] = cache_control_value(ttl, MarkerSite::StablePrefix);
            }
        }
    }

    /// Build the `system` value for a request, with the cache marker on the
    /// last block. Shared by both transports so the auth-specific block
    /// layout (and the OAuth identity-first invariant) has exactly one truth.
    ///
    /// - OAuth: identity block FIRST (never reorder — it heads the cached
    ///   prefix; changing it invalidates every active session), then the
    ///   spoof-guard block, then the optional user system prompt. Last block
    ///   gets the stable-prefix marker.
    /// - API key (or any non-oauth): single user-prompt block with the
    ///   stable-prefix marker; `None` when there is no system prompt.
    pub(super) fn build_system_blocks(
        auth_type: &str,
        system_prompt: &Option<String>,
        ttl: CacheTtl,
    ) -> Option<Value> {
        if auth_type == "oauth" {
            let mut system_blocks = vec![
                json!({"type": "text", "text": crate::core::config::get_identity()}),
                json!({"type": "text", "text": "You are a helpful AI assistant with access to tools. Use them when needed."}),
            ];
            if let Some(ref prompt) = system_prompt {
                system_blocks.push(json!({"type": "text", "text": prompt}));
            }
            // Prompt caching: mark the last system block so entire system prompt is cached
            if let Some(last) = system_blocks.last_mut() {
                last["cache_control"] = cache_control_value(ttl, MarkerSite::StablePrefix);
            }
            Some(json!(system_blocks))
        } else {
            system_prompt.as_ref().map(|prompt| {
                json!([
                    {"type": "text", "text": prompt, "cache_control": cache_control_value(ttl, MarkerSite::StablePrefix)}
                ])
            })
        }
    }

    /// Truncate tool results to avoid ballooning message history.
    /// The full result is still sent to the UI — this only caps what goes into
    /// the API messages that are re-sent on every subsequent call.
    pub(super) fn truncate_tool_result(result: &str, max_chars: usize) -> String {
        if result.len() <= max_chars {
            return result.to_string();
        }
        let truncated: String = result.chars().take(max_chars).collect();
        format!("{}\n\n[truncated — {} total chars, showing first {}]",
            truncated, result.len(), max_chars)
    }

    /// Returns the max output tokens for a given model.
    /// Opus-class models support 128K, Sonnet/Haiku cap at 64K.
    pub(super) fn max_tokens_for_model(model: &str) -> u64 {
        if model.contains("opus") {
            128000
        } else {
            64000
        }
    }

    /// Append a single-line usage record to the per-call log — opt-in via the
    /// `SYNAPS_USAGE_LOG` env var. Silent no-op if unset or set to "0".
    ///
    /// Value semantics:
    /// - unset or "0" or empty → logging disabled
    /// - "1" or "true" → default path `~/.cache/synaps/usage.log`
    /// - anything else → treated as an absolute path
    ///
    /// File is created with mode 0600 to prevent co-located-user snooping
    /// (previous versions wrote to `/tmp/synaps-usage.log` world-readable —
    /// flagged by S172 security review). Errors silently dropped so a broken
    /// log path never breaks the request pipeline.
    pub(super) fn log_usage(input_t: u64, cache_read: u64, cache_create: u64, output_t: u64) {
        let setting = match std::env::var("SYNAPS_USAGE_LOG") {
            Ok(v) if !v.is_empty() && v != "0" => v,
            _ => return,
        };

        let path = if matches!(setting.as_str(), "1" | "true" | "True" | "TRUE") {
            let home = match std::env::var("HOME") {
                Ok(h) => h,
                Err(_) => return,
            };
            format!("{}/.cache/synaps/usage.log", home)
        } else {
            setting
        };

        // Best-effort: create parent dir; ignore failure (open will error out)
        if let Some(parent) = std::path::Path::new(&path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let total = input_t + cache_read + cache_create;
        let pct = if total > 0 { (cache_read as f64 / total as f64 * 100.0) as u32 } else { 0 };

        use std::os::unix::fs::OpenOptionsExt;
        // O_NOFOLLOW: refuse to open if the target is a symlink. Defensive
        // against a co-located user planting a symlink at a custom
        // SYNAPS_USAGE_LOG path (CWE-59). The default path lives under
        // $HOME/.cache which is typically 0700 so this is belt-and-braces.
        #[cfg(target_os = "linux")]
        const O_NOFOLLOW_FLAG: i32 = 0o400000;
        #[cfg(target_os = "macos")]
        const O_NOFOLLOW_FLAG: i32 = 0x0100;
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        const O_NOFOLLOW_FLAG: i32 = 0;
        let result = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW_FLAG)
            .open(&path);
        if let Ok(mut f) = result {
            use std::io::Write;
            let _ = writeln!(
                f,
                "uncached={} cache_read={} cache_write={} output={} hit={}%",
                input_t, cache_read, cache_create, output_t, pct
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use crate::core::config::CacheTtl;

    #[test]
    fn sanitize_drops_empty_thinking_blocks() {
        let mut msgs = vec![
            json!({
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "", "signature": "sig1"},
                    {"type": "text", "text": "hello"},
                ]
            }),
        ];
        HelperMethods::sanitize_thinking_blocks(&mut msgs);
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
    }

    #[test]
    fn sanitize_keeps_non_empty_thinking_blocks() {
        let mut msgs = vec![
            json!({
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "reasoning here", "signature": "sig1"},
                    {"type": "text", "text": "hello"},
                ]
            }),
        ];
        HelperMethods::sanitize_thinking_blocks(&mut msgs);
        assert_eq!(msgs[0]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn sanitize_drops_thinking_with_missing_field() {
        let mut msgs = vec![
            json!({
                "role": "assistant",
                "content": [
                    {"type": "thinking", "signature": "sig1"},
                    {"type": "text", "text": "hello"},
                ]
            }),
        ];
        HelperMethods::sanitize_thinking_blocks(&mut msgs);
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
    }

    #[test]
    fn sanitize_replaces_emptied_content_with_placeholder() {
        let mut msgs = vec![
            json!({"role": "user", "content": "first"}),
            json!({
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "", "signature": "sig1"},
                ]
            }),
            json!({"role": "user", "content": "second"}),
        ];
        HelperMethods::sanitize_thinking_blocks(&mut msgs);
        // Empty assistant message must be dropped entirely (cannot be turned into
        // an empty text block — the API rejects those too).
        // The two surrounding user messages must then be merged.
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["text"], "first");
        assert_eq!(content[1]["text"], "second");
    }

    #[test]
    fn sanitize_drops_empty_text_blocks() {
        let mut msgs = vec![
            json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": ""},
                    {"type": "text", "text": "real content"},
                ]
            }),
        ];
        HelperMethods::sanitize_thinking_blocks(&mut msgs);
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], "real content");
    }

    #[test]
    fn sanitize_merges_consecutive_user_messages_after_drop() {
        let mut msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "a"}]}),
            json!({"role": "assistant", "content": [{"type": "thinking", "thinking": ""}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "b"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "ok"}]}),
        ];
        HelperMethods::sanitize_thinking_blocks(&mut msgs);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(msgs[1]["role"], "assistant");
    }

    #[test]
    fn sanitize_preserves_alternation_when_no_drops_needed() {
        let mut msgs = vec![
            json!({"role": "user", "content": "a"}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "b"}]}),
            json!({"role": "user", "content": "c"}),
        ];
        HelperMethods::sanitize_thinking_blocks(&mut msgs);
        assert_eq!(msgs.len(), 3);
    }

    #[test]
    fn sanitize_skips_user_messages() {
        let mut msgs = vec![
            json!({
                "role": "user",
                "content": [
                    {"type": "thinking", "thinking": "", "signature": "sig1"},
                ]
            }),
        ];
        HelperMethods::sanitize_thinking_blocks(&mut msgs);
        // We only police assistant messages — user messages would be malformed for
        // a different reason and aren't ours to rewrite.
        assert_eq!(msgs[0]["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn sanitize_drops_redacted_thinking_with_empty_data() {
        let mut msgs = vec![
            json!({
                "role": "assistant",
                "content": [
                    {"type": "redacted_thinking", "data": ""},
                    {"type": "text", "text": "hi"},
                ]
            }),
        ];
        HelperMethods::sanitize_thinking_blocks(&mut msgs);
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
    }

    // --- annotate_cache_breakpoint (single-last strategy) ---

    fn has_marker(msg: &Value) -> bool {
        msg["content"]
            .as_array()
            .map(|c| c.iter().any(|b| b.get("cache_control").is_some()))
            .unwrap_or(false)
    }

    #[test]
    fn cache_empty_messages_is_noop() {
        let mut msgs: Vec<Value> = vec![];
        HelperMethods::annotate_cache_breakpoint(&mut msgs, CacheTtl::FiveMinutes);
        assert!(msgs.is_empty());
    }

    #[test]
    fn cache_single_user_string_content_coerced_and_marked() {
        let mut msgs = vec![json!({"role": "user", "content": "hello"})];
        HelperMethods::annotate_cache_breakpoint(&mut msgs, CacheTtl::FiveMinutes);
        let content = msgs[0]["content"].as_array().expect("coerced to block array");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "hello");
        assert_eq!(content[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn cache_only_last_message_gets_marker() {
        let mut msgs = vec![
            json!({"role": "user", "content": "one"}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "two"}]}),
            json!({"role": "user", "content": "three"}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "four"}]}),
            json!({"role": "user", "content": "five"}),
        ];
        HelperMethods::annotate_cache_breakpoint(&mut msgs, CacheTtl::FiveMinutes);
        for msg in &msgs[..4] {
            assert!(!has_marker(msg), "earlier message must not have cache_control");
        }
        assert!(has_marker(&msgs[4]));
        // Earlier string contents must remain untouched strings.
        assert!(msgs[0]["content"].is_string());
        assert!(msgs[2]["content"].is_string());
    }

    #[test]
    fn cache_marks_trailing_assistant_message() {
        let mut msgs = vec![
            json!({"role": "user", "content": "question"}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "answer"}]}),
        ];
        HelperMethods::annotate_cache_breakpoint(&mut msgs, CacheTtl::FiveMinutes);
        assert!(!has_marker(&msgs[0]));
        assert!(has_marker(&msgs[1]), "single-last marks ANY trailing role");
        assert_eq!(msgs[1]["content"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn cache_only_final_block_of_multi_block_content_marked() {
        let mut msgs = vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "first"},
                {"type": "text", "text": "second"},
                {"type": "text", "text": "third"},
            ]
        })];
        HelperMethods::annotate_cache_breakpoint(&mut msgs, CacheTtl::FiveMinutes);
        let content = msgs[0]["content"].as_array().unwrap();
        assert!(content[0].get("cache_control").is_none());
        assert!(content[1].get("cache_control").is_none());
        assert_eq!(content[2]["cache_control"]["type"], "ephemeral");
    }

    // ── cache_control_value: exact-string matrix (spec §5) ─────────────────
    //
    // THE most important property of the whole cache_ttl change: the default
    // (FiveMinutes) emits exactly `{"type":"ephemeral"}` — single key, no ttl
    // field — at EVERY site, so default-mode requests are byte-identical to
    // the previous release and can never invalidate existing cached prefixes.

    #[test]
    fn ccv_five_minutes_is_bare_ephemeral_exact_bytes_at_every_site() {
        for site in [MarkerSite::StablePrefix, MarkerSite::MessageTail] {
            let v = cache_control_value(CacheTtl::FiveMinutes, site);
            assert_eq!(
                serde_json::to_string(&v).unwrap(),
                r#"{"type":"ephemeral"}"#,
                "5m must serialize byte-identically to the legacy literal at {site:?}"
            );
        }
    }

    // NOTE: serde_json (without preserve_order) sorts object keys, so the
    // exact serialized form is {"ttl":"1h","type":"ephemeral"}. JSON object
    // key order is semantically irrelevant to the API; what matters is the
    // 5m form is the single-key legacy literal and 1h carries ttl:"1h".
    #[test]
    fn ccv_one_hour_carries_ttl_at_every_site() {
        for site in [MarkerSite::StablePrefix, MarkerSite::MessageTail] {
            let v = cache_control_value(CacheTtl::OneHour, site);
            assert_eq!(
                serde_json::to_string(&v).unwrap(),
                r#"{"ttl":"1h","type":"ephemeral"}"#,
                "1h must carry ttl at {site:?}"
            );
        }
    }

    #[test]
    fn ccv_hybrid_splits_by_site() {
        assert_eq!(
            serde_json::to_string(&cache_control_value(CacheTtl::Hybrid, MarkerSite::StablePrefix)).unwrap(),
            r#"{"ttl":"1h","type":"ephemeral"}"#,
        );
        assert_eq!(
            serde_json::to_string(&cache_control_value(CacheTtl::Hybrid, MarkerSite::MessageTail)).unwrap(),
            r#"{"type":"ephemeral"}"#,
        );
    }

    // ── annotate_cache_breakpoint TTL behavior ──────────────────────────────

    #[test]
    fn cache_breakpoint_5m_has_no_ttl_key() {
        let mut msgs = vec![json!({"role": "user", "content": "hello"})];
        HelperMethods::annotate_cache_breakpoint(&mut msgs, CacheTtl::FiveMinutes);
        let cc = &msgs[0]["content"][0]["cache_control"];
        assert_eq!(cc["type"], "ephemeral");
        assert!(cc.get("ttl").is_none(), "5m must not emit a ttl key (assert absence)");
        assert_eq!(cc.as_object().unwrap().len(), 1, "exactly one key: type");
    }

    #[test]
    fn cache_breakpoint_1h_emits_ttl() {
        let mut msgs = vec![json!({"role": "user", "content": "hello"})];
        HelperMethods::annotate_cache_breakpoint(&mut msgs, CacheTtl::OneHour);
        let cc = &msgs[0]["content"][0]["cache_control"];
        assert_eq!(cc["type"], "ephemeral");
        assert_eq!(cc["ttl"], "1h");
    }

    #[test]
    fn cache_breakpoint_hybrid_message_tail_has_no_ttl_key() {
        // Hybrid's message-tail marker is the 5m one — bare ephemeral.
        let mut msgs = vec![json!({"role": "user", "content": "hello"})];
        HelperMethods::annotate_cache_breakpoint(&mut msgs, CacheTtl::Hybrid);
        let cc = &msgs[0]["content"][0]["cache_control"];
        assert_eq!(cc["type"], "ephemeral");
        assert!(cc.get("ttl").is_none());
    }

    #[test]
    fn cache_breakpoint_coercion_and_single_last_unchanged_under_all_modes() {
        for ttl in [CacheTtl::FiveMinutes, CacheTtl::OneHour, CacheTtl::Hybrid] {
            // String coercion still happens.
            let mut msgs = vec![
                json!({"role": "user", "content": "one"}),
                json!({"role": "user", "content": [
                    {"type": "text", "text": "a"},
                    {"type": "text", "text": "b"},
                ]}),
            ];
            HelperMethods::annotate_cache_breakpoint(&mut msgs, ttl);
            assert!(!has_marker(&msgs[0]), "earlier message unmarked under {ttl:?}");
            assert!(msgs[0]["content"].is_string(), "earlier string content untouched");
            let content = msgs[1]["content"].as_array().unwrap();
            assert!(content[0].get("cache_control").is_none(), "only final block marked");
            assert!(content[1].get("cache_control").is_some());
        }
    }

    // ── mark_last_tool (stable-prefix site, both transports) ────────────────

    fn tools_body() -> Value {
        json!({"tools": [
            {"name": "bash", "input_schema": {}},
            {"name": "read", "input_schema": {}},
        ]})
    }

    #[test]
    fn mark_last_tool_5m_is_bare_ephemeral() {
        let mut body = tools_body();
        HelperMethods::mark_last_tool(&mut body, CacheTtl::FiveMinutes);
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(
            serde_json::to_string(&body["tools"][1]["cache_control"]).unwrap(),
            r#"{"type":"ephemeral"}"#,
        );
    }

    #[test]
    fn mark_last_tool_1h_and_hybrid_carry_ttl() {
        for ttl in [CacheTtl::OneHour, CacheTtl::Hybrid] {
            let mut body = tools_body();
            HelperMethods::mark_last_tool(&mut body, ttl);
            assert_eq!(body["tools"][1]["cache_control"]["ttl"], "1h", "under {ttl:?}");
        }
    }

    #[test]
    fn mark_last_tool_no_tools_is_noop() {
        let mut body = json!({"tools": []});
        HelperMethods::mark_last_tool(&mut body, CacheTtl::OneHour);
        assert_eq!(body, json!({"tools": []}));
    }

    // ── build_system_blocks (OAuth + API-key sites, both transports) ────────

    #[test]
    fn system_blocks_oauth_identity_first_and_marker_on_last() {
        for ttl in [CacheTtl::FiveMinutes, CacheTtl::OneHour, CacheTtl::Hybrid] {
            let prompt = Some("custom prompt".to_string());
            let system = HelperMethods::build_system_blocks("oauth", &prompt, ttl).unwrap();
            let blocks = system.as_array().unwrap();
            assert_eq!(blocks.len(), 3);
            // Identity block is FIRST and unmarked — head of the cached prefix.
            assert_eq!(blocks[0]["text"], crate::core::config::get_identity());
            assert!(blocks[0].get("cache_control").is_none());
            assert!(blocks[1].get("cache_control").is_none());
            // Last block carries the stable-prefix marker.
            let cc = &blocks[2]["cache_control"];
            assert_eq!(cc["type"], "ephemeral");
            match ttl {
                CacheTtl::FiveMinutes => assert!(cc.get("ttl").is_none(), "5m: no ttl key"),
                _ => assert_eq!(cc["ttl"], "1h"),
            }
        }
    }

    #[test]
    fn system_blocks_oauth_5m_marker_exact_bytes() {
        let system = HelperMethods::build_system_blocks("oauth", &None, CacheTtl::FiveMinutes).unwrap();
        let last = system.as_array().unwrap().last().unwrap().clone();
        assert_eq!(
            serde_json::to_string(&last["cache_control"]).unwrap(),
            r#"{"type":"ephemeral"}"#,
        );
    }

    #[test]
    fn system_blocks_api_key_single_block() {
        for ttl in [CacheTtl::FiveMinutes, CacheTtl::OneHour, CacheTtl::Hybrid] {
            let prompt = Some("sys".to_string());
            let system = HelperMethods::build_system_blocks("api_key", &prompt, ttl).unwrap();
            let blocks = system.as_array().unwrap();
            assert_eq!(blocks.len(), 1);
            assert_eq!(blocks[0]["text"], "sys");
            let cc = &blocks[0]["cache_control"];
            match ttl {
                CacheTtl::FiveMinutes => assert_eq!(
                    serde_json::to_string(cc).unwrap(),
                    r#"{"type":"ephemeral"}"#,
                ),
                _ => assert_eq!(cc["ttl"], "1h"),
            }
        }
    }

    #[test]
    fn system_blocks_api_key_without_prompt_is_none() {
        assert!(HelperMethods::build_system_blocks("api_key", &None, CacheTtl::OneHour).is_none());
    }

    // ── Hybrid ordering invariant (spec §3.3 / §5) ──────────────────────────

    /// Build a request the way both transports do (tools marked, system
    /// built, message tail annotated), then walk markers in the API's
    /// logical emission order (tools → system → messages) and assert every
    /// 1h marker precedes the 5m tail marker.
    #[test]
    fn hybrid_ordering_invariant_1h_prefix_precedes_5m_tail() {
        let ttl = CacheTtl::Hybrid;
        let mut body = tools_body();
        HelperMethods::mark_last_tool(&mut body, ttl);
        let prompt = Some("sys".to_string());
        body["system"] = HelperMethods::build_system_blocks("oauth", &prompt, ttl).unwrap();
        let mut messages = vec![json!({"role": "user", "content": "hi"})];
        HelperMethods::annotate_cache_breakpoint(&mut messages, ttl);
        body["messages"] = json!(messages);

        // Collect (logical_order, ttl_str) for every marker in the body.
        let mut markers: Vec<&str> = Vec::new();
        for tool in body["tools"].as_array().unwrap() {
            if let Some(cc) = tool.get("cache_control") {
                markers.push(cc.get("ttl").and_then(|t| t.as_str()).unwrap_or("5m"));
            }
        }
        for block in body["system"].as_array().unwrap() {
            if let Some(cc) = block.get("cache_control") {
                markers.push(cc.get("ttl").and_then(|t| t.as_str()).unwrap_or("5m"));
            }
        }
        for msg in body["messages"].as_array().unwrap() {
            if let Some(arr) = msg["content"].as_array() {
                for block in arr {
                    if let Some(cc) = block.get("cache_control") {
                        markers.push(cc.get("ttl").and_then(|t| t.as_str()).unwrap_or("5m"));
                    }
                }
            }
        }

        assert_eq!(markers, vec!["1h", "1h", "5m"], "tool 1h, system 1h, tail 5m");
        // Ordering rule: once a 5m marker appears, no 1h marker may follow.
        let first_5m = markers.iter().position(|m| *m == "5m").unwrap();
        assert!(
            markers[first_5m..].iter().all(|m| *m == "5m"),
            "every 1h marker must precede the 5m tail"
        );
        // And the hybrid tail marker carries NO ttl key at all.
        let tail_cc = &body["messages"][0]["content"][0]["cache_control"];
        assert!(tail_cc.get("ttl").is_none());
    }

    /// Full-body default-mode check: with FiveMinutes, every marker in a
    /// fully constructed request serializes to exactly the legacy literal.
    #[test]
    fn default_mode_full_body_markers_byte_identical_to_legacy() {
        let ttl = CacheTtl::FiveMinutes;
        let mut body = tools_body();
        HelperMethods::mark_last_tool(&mut body, ttl);
        let prompt = Some("sys".to_string());
        for auth in ["oauth", "api_key"] {
            let system = HelperMethods::build_system_blocks(auth, &prompt, ttl).unwrap();
            for block in system.as_array().unwrap() {
                if let Some(cc) = block.get("cache_control") {
                    assert_eq!(serde_json::to_string(cc).unwrap(), r#"{"type":"ephemeral"}"#);
                }
            }
        }
        let mut messages = vec![json!({"role": "user", "content": "hi"})];
        HelperMethods::annotate_cache_breakpoint(&mut messages, ttl);
        assert_eq!(
            serde_json::to_string(&messages[0]["content"][0]["cache_control"]).unwrap(),
            r#"{"type":"ephemeral"}"#,
        );
        assert_eq!(
            serde_json::to_string(&body["tools"][1]["cache_control"]).unwrap(),
            r#"{"type":"ephemeral"}"#,
        );
    }
}
