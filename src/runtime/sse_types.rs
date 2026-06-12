//! Typed wire model for Anthropic SSE events. Borrowing deserializer:
//! `Cow<'a, str>` borrows from the line buffer on the escape-free fast
//! path, allocates only when JSON escapes force it. NOT &'a str — serde
//! hard-errors on escaped strings for &str targets.
#![allow(dead_code)] // TODO(slice 3): remove

use serde::Deserialize;
use std::borrow::Cow;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum AnthropicEvent<'a> {
    ContentBlockStart {
        #[serde(borrow)]
        content_block: ContentBlock<'a>,
    },
    ContentBlockDelta {
        #[serde(borrow)]
        delta: Delta<'a>,
    },
    ContentBlockStop,
    MessageStart {
        #[serde(borrow)]
        message: MessageStartPayload<'a>,
    },
    MessageDelta {
        #[serde(borrow, default)]
        delta: Option<MessageDeltaInner<'a>>,
        #[serde(default)]
        usage: Option<UsagePayload>,
    },
    MessageStop,
    /// Unit variant required: serde's #[serde(other)] only supports unit
    /// variants under internal tagging. Payload discarded — matches the
    /// current `_ => {}`. Covers `ping`, `error`, and future event types.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum ContentBlock<'a> {
    Text,     // initial `text` field ignored — current code never reads it
    Thinking, // initial `thinking`/`signature` ignored — same
    ToolUse {
        // #[serde(default)]: current code does .as_str().unwrap_or("") —
        // a missing id/name must not kill the event. Cow: Default = Borrowed("").
        #[serde(borrow, default)]
        id: Cow<'a, str>,
        #[serde(borrow, default)]
        name: Cow<'a, str>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
// Variant names mirror the wire tags (`text_delta`, `thinking_delta`, ...) —
// renaming to satisfy the lint would obscure the protocol mapping.
#[allow(clippy::enum_variant_names)]
pub(super) enum Delta<'a> {
    TextDelta {
        #[serde(borrow)]
        text: Cow<'a, str>,
    },
    ThinkingDelta {
        #[serde(borrow)]
        thinking: Cow<'a, str>,
    },
    SignatureDelta {
        #[serde(borrow)]
        signature: Cow<'a, str>,
    },
    InputJsonDelta {
        #[serde(borrow)]
        partial_json: Cow<'a, str>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
pub(super) struct MessageStartPayload<'a> {
    #[serde(borrow, default)]
    pub id: Option<Cow<'a, str>>,
    #[serde(default)]
    pub usage: Option<UsagePayload>,
}

#[derive(Debug, Deserialize)]
pub(super) struct MessageDeltaInner<'a> {
    #[serde(borrow, default)]
    pub stop_reason: Option<Cow<'a, str>>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct UsagePayload {
    // #[serde(default)] mirrors .as_u64().unwrap_or(0)
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_creation: Option<CacheCreationDetail>,
}

#[derive(Debug, Deserialize)]
pub(super) struct CacheCreationDetail {
    #[serde(default)]
    pub ephemeral_5m_input_tokens: Option<u64>,
    #[serde(default)]
    pub ephemeral_1h_input_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(data: &str) -> AnthropicEvent<'_> {
        serde_json::from_str(data).expect("event should parse")
    }

    #[test]
    fn content_block_start_tool_use() {
        let event = parse(
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01ABC","name":"bash","input":{}}}"#,
        );
        match event {
            AnthropicEvent::ContentBlockStart {
                content_block: ContentBlock::ToolUse { id, name },
            } => {
                assert_eq!(id, "toolu_01ABC");
                assert_eq!(name, "bash");
            }
            other => panic!("expected ToolUse start, got {:?}", other),
        }
    }

    #[test]
    fn content_block_start_tool_use_missing_id_name_defaults_empty() {
        let event = parse(
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use"}}"#,
        );
        match event {
            AnthropicEvent::ContentBlockStart {
                content_block: ContentBlock::ToolUse { id, name },
            } => {
                assert_eq!(id, "");
                assert_eq!(name, "");
            }
            other => panic!("expected ToolUse start, got {:?}", other),
        }
    }

    #[test]
    fn content_block_start_thinking_and_text() {
        let event = parse(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
        );
        assert!(matches!(
            event,
            AnthropicEvent::ContentBlockStart {
                content_block: ContentBlock::Thinking
            }
        ));

        let event = parse(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        );
        assert!(matches!(
            event,
            AnthropicEvent::ContentBlockStart {
                content_block: ContentBlock::Text
            }
        ));
    }

    #[test]
    fn content_block_start_unknown_block_type() {
        let event = parse(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","weird":42}}"#,
        );
        assert!(matches!(
            event,
            AnthropicEvent::ContentBlockStart {
                content_block: ContentBlock::Unknown
            }
        ));
    }

    #[test]
    fn text_delta_borrows_when_escape_free() {
        // Borrow tripwire: escape-free input MUST take the zero-copy path.
        // If a #[serde(borrow)] gets dropped, this becomes Cow::Owned and fails.
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello world"}}"#;
        let event = parse(data);
        match event {
            AnthropicEvent::ContentBlockDelta {
                delta: Delta::TextDelta { text },
            } => {
                assert!(matches!(text, Cow::Borrowed(_)), "expected borrowed Cow");
                assert_eq!(text, "hello world");
            }
            other => panic!("expected TextDelta, got {:?}", other),
        }
    }

    #[test]
    fn text_delta_with_escapes_is_owned_and_correct() {
        // Escaped input forces decoding — this is why &'a str would hard-fail.
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"line\none \u00e9"}}"#;
        let event = parse(data);
        match event {
            AnthropicEvent::ContentBlockDelta {
                delta: Delta::TextDelta { text },
            } => {
                assert!(matches!(text, Cow::Owned(_)), "expected owned Cow");
                assert_eq!(text, "line\none é");
            }
            other => panic!("expected TextDelta, got {:?}", other),
        }
    }

    #[test]
    fn text_delta_multibyte_utf8() {
        // Raw (unescaped) multi-byte UTF-8 stays on the borrow path.
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"héllo ✨"}}"#;
        let event = parse(data);
        match event {
            AnthropicEvent::ContentBlockDelta {
                delta: Delta::TextDelta { text },
            } => {
                assert!(matches!(text, Cow::Borrowed(_)), "expected borrowed Cow");
                assert_eq!(text.as_bytes(), "héllo ✨".as_bytes());
            }
            other => panic!("expected TextDelta, got {:?}", other),
        }
    }

    #[test]
    fn thinking_delta_signature_delta_input_json_delta() {
        let event = parse(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
        );
        match event {
            AnthropicEvent::ContentBlockDelta {
                delta: Delta::ThinkingDelta { thinking },
            } => assert_eq!(thinking, "hmm"),
            other => panic!("expected ThinkingDelta, got {:?}", other),
        }

        let event = parse(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig123"}}"#,
        );
        match event {
            AnthropicEvent::ContentBlockDelta {
                delta: Delta::SignatureDelta { signature },
            } => assert_eq!(signature, "sig123"),
            other => panic!("expected SignatureDelta, got {:?}", other),
        }

        let event = parse(
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":"}}"#,
        );
        match event {
            AnthropicEvent::ContentBlockDelta {
                delta: Delta::InputJsonDelta { partial_json },
            } => assert_eq!(partial_json, "{\"cmd\":"),
            other => panic!("expected InputJsonDelta, got {:?}", other),
        }
    }

    #[test]
    fn delta_unknown_subtype() {
        let event = parse(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"citations_delta","citation":{}}}"#,
        );
        assert!(matches!(
            event,
            AnthropicEvent::ContentBlockDelta {
                delta: Delta::Unknown
            }
        ));
    }

    #[test]
    fn message_start_full() {
        let event = parse(
            r#"{"type":"message_start","message":{"id":"msg_01XYZ","role":"assistant","model":"claude-sonnet-4-6","usage":{"input_tokens":100,"output_tokens":1,"cache_read_input_tokens":50,"cache_creation_input_tokens":25,"cache_creation":{"ephemeral_5m_input_tokens":20,"ephemeral_1h_input_tokens":5}}}}"#,
        );
        match event {
            AnthropicEvent::MessageStart { message } => {
                assert_eq!(message.id.as_deref(), Some("msg_01XYZ"));
                let usage = message.usage.expect("usage present");
                assert_eq!(usage.input_tokens, 100);
                assert_eq!(usage.output_tokens, 1);
                assert_eq!(usage.cache_read_input_tokens, 50);
                assert_eq!(usage.cache_creation_input_tokens, 25);
                let cc = usage.cache_creation.expect("cache_creation present");
                assert_eq!(cc.ephemeral_5m_input_tokens, Some(20));
                assert_eq!(cc.ephemeral_1h_input_tokens, Some(5));
            }
            other => panic!("expected MessageStart, got {:?}", other),
        }
    }

    #[test]
    fn message_delta_stop_reason_and_usage() {
        let event = parse(
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":42,"cache_creation":{"ephemeral_5m_input_tokens":7,"ephemeral_1h_input_tokens":9}}}"#,
        );
        match event {
            AnthropicEvent::MessageDelta { delta, usage } => {
                let delta = delta.expect("delta present");
                assert_eq!(delta.stop_reason.as_deref(), Some("tool_use"));
                let usage = usage.expect("usage present");
                assert_eq!(usage.output_tokens, 42);
                let cc = usage.cache_creation.expect("cache_creation present");
                assert_eq!(cc.ephemeral_5m_input_tokens, Some(7));
                assert_eq!(cc.ephemeral_1h_input_tokens, Some(9));
            }
            other => panic!("expected MessageDelta, got {:?}", other),
        }
    }

    #[test]
    fn usage_missing_fields_default_zero() {
        let usage: UsagePayload = serde_json::from_str("{}").expect("empty usage parses");
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 0);
        assert_eq!(usage.cache_read_input_tokens, 0);
        assert_eq!(usage.cache_creation_input_tokens, 0);
        assert!(usage.cache_creation.is_none());
    }

    #[test]
    fn unknown_event_type_is_unit_unknown() {
        for data in [
            r#"{"type":"ping"}"#,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            r#"{"type":"fnord","payload":[1,2,3]}"#,
        ] {
            assert!(
                matches!(parse(data), AnthropicEvent::Unknown),
                "expected Unknown for {data}"
            );
        }
    }

    #[test]
    fn content_block_stop_and_message_stop_parse() {
        assert!(matches!(
            parse(r#"{"type":"content_block_stop","index":3}"#),
            AnthropicEvent::ContentBlockStop
        ));
        assert!(matches!(
            parse(r#"{"type":"message_stop","amazon-bedrock-invocationMetrics":{"x":1}}"#),
            AnthropicEvent::MessageStop
        ));
    }
}
