//! Sidecar protocol types — line-JSON over stdio.
//!
//! The host treats sidecars as lego-block processes: it starts them, sends
//! generic trigger frames, and consumes generic status/text frames. A sidecar
//! may implement speech, OCR, gestures, automation, telemetry, or anything
//! else; modality semantics live entirely in the plugin.
//!
//! Wire format: one JSON object per line on the sidecar's stdin/stdout.

use serde::{Deserialize, Serialize};

/// Protocol version understood by this build.
pub const SIDECAR_PROTOCOL_VERSION: u16 = 2;

/// Commands sent from Synaps CLI to the sidecar (one per line, JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SidecarCommand {
    /// Plugin-defined initialization payload. Core forwards the value
    /// verbatim and does not interpret its schema.
    Init {
        config: serde_json::Value,
    },
    /// Generic activation trigger. `name` and `payload` are plugin-defined.
    Trigger {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<serde_json::Value>,
    },
    Shutdown,
}

/// How text emitted by a sidecar should be applied to the input buffer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InsertTextMode {
    Append,
    Final,
    Replace,
}

/// Frames emitted by the sidecar (one per line, JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SidecarFrame {
    Hello {
        protocol_version: u16,
        extension: String,
        #[serde(default)]
        capabilities: Vec<String>,
    },
    Status {
        state: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default)]
        capabilities: Vec<String>,
    },
    InsertText {
        text: String,
        mode: InsertTextMode,
    },
    Error {
        message: String,
    },
    #[serde(other)]
    Custom,
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_round_trip_init() {
        let cmd = SidecarCommand::Init {
            config: serde_json::json!({
                "protocol_version": SIDECAR_PROTOCOL_VERSION,
                "plugin_config": { "mode": "whatever-the-plugin-wants" }
            }),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"type\":\"init\""));
        assert!(json.contains("whatever-the-plugin-wants"));
        let parsed: SidecarCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cmd);
    }

    #[test]
    fn command_round_trip_trigger_shutdown() {
        let commands = vec![
            SidecarCommand::Trigger { name: "press".into(), payload: None },
            SidecarCommand::Trigger { name: "release".into(), payload: Some(serde_json::json!({"source":"keybind"})) },
            SidecarCommand::Shutdown,
        ];
        for cmd in commands {
            let json = serde_json::to_string(&cmd).unwrap();
            let parsed: SidecarCommand = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, cmd);
        }
    }

    #[test]
    fn trigger_payload_is_optional() {
        let without_payload = serde_json::to_string(&SidecarCommand::Trigger {
            name: "tap".into(),
            payload: None,
        }).unwrap();
        assert!(without_payload.contains("\"type\":\"trigger\""));
        assert!(!without_payload.contains("payload"));

        let with_payload = serde_json::to_string(&SidecarCommand::Trigger {
            name: "tap".into(),
            payload: Some(serde_json::json!({"count":2})),
        }).unwrap();
        assert!(with_payload.contains("payload"));
    }

    #[test]
    fn frame_round_trip_hello_status_insert_error() {
        let frames = vec![
            SidecarFrame::Hello {
                protocol_version: SIDECAR_PROTOCOL_VERSION,
                extension: "example-plugin".to_string(),
                capabilities: vec!["anything".into(), "input.text".into()],
            },
            SidecarFrame::Status {
                state: "busy".into(),
                label: Some("Working".into()),
                capabilities: vec![],
            },
            SidecarFrame::InsertText {
                text: "partial".into(),
                mode: InsertTextMode::Append,
            },
            SidecarFrame::InsertText {
                text: "done".into(),
                mode: InsertTextMode::Final,
            },
            SidecarFrame::Error { message: "missing model".into() },
        ];
        for frame in frames {
            let json = serde_json::to_string(&frame).unwrap();
            let parsed: SidecarFrame = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, frame);
        }
    }

    #[test]
    fn parses_generic_hello_line() {
        let line = r#"{"type":"hello","protocol_version":2,"extension":"example-plugin","capabilities":["text.insert"]}"#;
        let parsed: SidecarFrame = serde_json::from_str(line).unwrap();
        match parsed {
            SidecarFrame::Hello { protocol_version, extension, capabilities } => {
                assert_eq!(protocol_version, SIDECAR_PROTOCOL_VERSION);
                assert_eq!(extension, "example-plugin");
                assert_eq!(capabilities, vec!["text.insert".to_string()]);
            }
            other => panic!("expected Hello, got {:?}", other),
        }
    }

    #[test]
    fn parses_insert_text_final_line() {
        let line = r#"{"type":"insert_text","text":"hello from a plugin","mode":"final"}"#;
        let parsed: SidecarFrame = serde_json::from_str(line).unwrap();
        assert_eq!(
            parsed,
            SidecarFrame::InsertText {
                text: "hello from a plugin".into(),
                mode: InsertTextMode::Final,
            }
        );
    }
}
