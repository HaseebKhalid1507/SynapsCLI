use serde_json::{json, Value};
use tokio::sync::mpsc;
use super::types::StreamEvent;
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
            let _ = tx.send(StreamEvent::SteeringDelivered { message: msg.clone() });
            messages.push(json!({"role": "user", "content": msg}));
            injected = true;
        }
        injected
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
    pub(super) const MAX_TOOL_RESULT_CHARS: usize = 30_000;

    pub(super) fn truncate_tool_result(result: &str) -> String {
        if result.len() <= Self::MAX_TOOL_RESULT_CHARS {
            return result.to_string();
        }
        let truncated: String = result.chars().take(Self::MAX_TOOL_RESULT_CHARS).collect();
        format!("{}\n\n[truncated — {} total chars, showing first {}]",
            truncated, result.len(), Self::MAX_TOOL_RESULT_CHARS)
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
}