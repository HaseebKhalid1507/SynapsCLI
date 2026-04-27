use serde_json::{json, Value};
use tokio::sync::mpsc;
use super::types::{StreamEvent, AgentEvent};
use crate::truncate_str;

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

    /// Annotate a cache breakpoint on the conversation prefix.
    /// To maximize cache hits, we must place stationary boundaries. Modifying an old marker
    /// breaks the cache for that prefix. We retain up to 2 conversational markers.
    pub(super) fn annotate_cache_breakpoint(messages: &mut [Value]) {
        let user_indices: Vec<usize> = messages.iter().enumerate()
            .filter(|(_, m)| m["role"].as_str() == Some("user"))
            .map(|(i, _)| i)
            .collect();

        if user_indices.is_empty() { return; }

        // Find existing markers
        let mut existing_markers = Vec::new();
        for &idx in &user_indices {
            if let Some(content) = messages[idx]["content"].as_array() {
                if content.last().and_then(|b| b.get("cache_control")).is_some() {
                    existing_markers.push(idx);
                }
            }
        }

        // We only place a new marker if the last one is 4+ user messages away (e.g. 4 tool loops!)
        let target_idx = user_indices[user_indices.len() - 1]; // We can just mark the latest
        let should_add = match existing_markers.last() {
            Some(&last_idx) => user_indices.len() as isize - user_indices.iter().position(|&x| x == last_idx).unwrap_or(0) as isize >= 4,
            None => true,
        };

        if should_add && !existing_markers.contains(&target_idx) {
            existing_markers.push(target_idx);

            // Convert raw string content to block array to allow adding cache_control
            if messages[target_idx]["content"].is_string() {
                if let Some(text) = messages[target_idx]["content"].as_str() {
                    messages[target_idx]["content"] = json!([{"type": "text", "text": text}]);
                }
            }

            if let Some(content) = messages[target_idx]["content"].as_array_mut() {
                if let Some(last_block) = content.last_mut() {
                    last_block["cache_control"] = json!({"type": "ephemeral"});
                }
            }
        }

        // Enforce max 2 conversational markers to avoid Anthropic's 4-marker limit
        if existing_markers.len() > 2 {
            let keep = &existing_markers[existing_markers.len() - 2..];
            for (i, msg) in messages.iter_mut().enumerate() {
                if !keep.contains(&i) && msg["role"].as_str() == Some("user") {
                    if let Some(content) = msg["content"].as_array_mut() {
                        if let Some(last_block) = content.last_mut() {
                            if last_block.get("cache_control").is_some() {
                                last_block.as_object_mut().map(|obj| obj.remove("cache_control"));
                            }
                        }
                    }
                }
            }
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
}