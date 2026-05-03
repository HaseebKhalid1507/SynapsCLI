//! Plugin custom-editor JSON-RPC payloads (Path B Phase 4).
//!
//! When a user opens a plugin-declared settings field whose `editor` is
//! `"custom"`, Synaps and the plugin exchange these typed messages over
//! the existing JSON-RPC channel:
//!
//! ```text
//! synaps → plugin   settings.editor.open      { category, field }
//! plugin → synaps   settings.editor.render    { rows, cursor?, footer? }   (notification, repeated)
//! synaps → plugin   settings.editor.key       { key }                      (per keypress)
//! plugin → synaps   settings.editor.commit    { value }                    (when user accepts)
//! ```
//!
//! The render notification is a server-push that may be emitted multiple
//! times as the plugin's editor state evolves; consumers should debounce
//! at the UI layer to avoid flicker.
//!
//! Wire shape mirrors `extensions::commands::CommandOutputEvent`. This
//! module owns *only* the typed contracts and a small parser; the
//! settings UI glue (overlay rendering, key dispatch, debounce) lives in
//! `chatui/settings/`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const METHOD_OPEN: &str = "settings.editor.open";
pub const METHOD_RENDER: &str = "settings.editor.render";
pub const METHOD_KEY: &str = "settings.editor.key";
pub const METHOD_COMMIT: &str = "settings.editor.commit";
pub const METHOD_CLOSE: &str = "settings.editor.close";

/// `settings.editor.open` — synaps → plugin request when the user opens
/// a custom editor for `(category, field)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsEditorOpenParams {
    pub category: String,
    pub field: String,
}

/// `settings.editor.key` — synaps → plugin notification on each
/// keypress while a custom editor is focused. `key` is the string form
/// produced by `crossterm::event::KeyEvent` (e.g. `"Down"`, `"Enter"`,
/// `"Esc"`, `"Char(' ')"` — the exact lexicon is documented alongside
/// the keybind subsystem).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsEditorKeyParams {
    pub key: String,
}

/// `settings.editor.commit` — plugin → synaps notification when the user
/// accepts a value. Synaps writes `value` to the plugin's config
/// namespace at `(category, field)` and closes the overlay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsEditorCommitParams {
    pub value: Value,
}

/// `settings.editor.close` — either side may emit this to dismiss the
/// overlay (e.g. plugin-side cancellation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SettingsEditorCloseParams {
    /// Optional reason string surfaced to the user as a transient note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `settings.editor.render` — plugin → synaps notification carrying the
/// current visual state of the editor body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsEditorRenderParams {
    pub rows: Vec<SettingsEditorRow>,
    /// Index into `rows` of the currently-highlighted entry. `None`
    /// means no row is selected (e.g. a free-text editor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<usize>,
    /// Bottom hint line (e.g. `"↓ Up/Down  Enter to select  Esc to cancel"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footer: Option<String>,
}

/// A single row in the custom editor body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsEditorRow {
    pub label: String,
    /// Short marker glyph, e.g. `"✓"`, `" "`, `"→"`. The UI renders this
    /// in a fixed-width gutter so columns align across rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<String>,
    /// `true` when the row reacts to Enter. Non-selectable rows are
    /// rendered dimmed and skipped by cursor navigation.
    #[serde(default = "default_true")]
    pub selectable: bool,
    /// Plugin-side opaque payload echoed back via `settings.editor.commit`
    /// when the user accepts this row. Synaps does not interpret it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

fn default_true() -> bool {
    true
}

/// Errors returned when parsing a JSON-RPC frame whose method belongs
/// to the `settings.editor.*` family but whose params don't match the
/// expected shape.
#[derive(Debug, thiserror::Error)]
pub enum SettingsEditorParseError {
    #[error("unknown settings.editor method: {0}")]
    UnknownMethod(String),
    #[error("invalid params for {method}: {source}")]
    InvalidParams {
        method: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

/// Normalised view of a single inbound frame. Distinguishes the four
/// notifications the core can receive from the plugin side. (The
/// `open`/`key` requests originate from the core and are typed via the
/// individual params structs above.)
#[derive(Debug, Clone, PartialEq)]
pub enum InboundSettingsEditorFrame {
    Render(SettingsEditorRenderParams),
    Commit(SettingsEditorCommitParams),
    Close(SettingsEditorCloseParams),
}

/// Parse the params object of a `settings.editor.*` notification.
pub fn parse_inbound(
    method: &str,
    params: Value,
) -> Result<InboundSettingsEditorFrame, SettingsEditorParseError> {
    match method {
        METHOD_RENDER => serde_json::from_value(params)
            .map(InboundSettingsEditorFrame::Render)
            .map_err(|source| SettingsEditorParseError::InvalidParams {
                method: METHOD_RENDER,
                source,
            }),
        METHOD_COMMIT => serde_json::from_value(params)
            .map(InboundSettingsEditorFrame::Commit)
            .map_err(|source| SettingsEditorParseError::InvalidParams {
                method: METHOD_COMMIT,
                source,
            }),
        METHOD_CLOSE => serde_json::from_value(params)
            .map(InboundSettingsEditorFrame::Close)
            .map_err(|source| SettingsEditorParseError::InvalidParams {
                method: METHOD_CLOSE,
                source,
            }),
        other => Err(SettingsEditorParseError::UnknownMethod(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn open_params_round_trip() {
        let p = SettingsEditorOpenParams {
            category: "voice".to_string(),
            field: "model_path".to_string(),
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v, json!({"category":"voice","field":"model_path"}));
        let back: SettingsEditorOpenParams = serde_json::from_value(v).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn render_params_parse_full_example() {
        let v = json!({
            "rows": [
                { "label": "tiny.en (75 MB)", "marker": "✓", "selectable": true,  "data": "/abs/path/.bin" },
                { "label": "base (142 MB)",   "marker": " ", "selectable": true,  "data": "download:base" },
                { "label": "(separator)",                      "selectable": false }
            ],
            "cursor": 2,
            "footer": "Up/Down  Enter to select"
        });
        let frame = parse_inbound(METHOD_RENDER, v).unwrap();
        match frame {
            InboundSettingsEditorFrame::Render(r) => {
                assert_eq!(r.rows.len(), 3);
                assert_eq!(r.rows[0].marker.as_deref(), Some("✓"));
                assert!(r.rows[0].selectable);
                assert_eq!(r.rows[0].data.as_ref().unwrap(), &json!("/abs/path/.bin"));
                assert!(!r.rows[2].selectable);
                assert_eq!(r.cursor, Some(2));
                assert_eq!(r.footer.as_deref(), Some("Up/Down  Enter to select"));
            }
            _ => panic!("expected render frame"),
        }
    }

    #[test]
    fn render_row_selectable_defaults_to_true() {
        let v = json!({ "rows": [ { "label": "x" } ] });
        let frame = parse_inbound(METHOD_RENDER, v).unwrap();
        match frame {
            InboundSettingsEditorFrame::Render(r) => {
                assert!(r.rows[0].selectable);
                assert!(r.rows[0].marker.is_none());
                assert!(r.rows[0].data.is_none());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn commit_params_carry_arbitrary_value() {
        let v = json!({ "value": { "path": "/x", "id": 7 } });
        let frame = parse_inbound(METHOD_COMMIT, v).unwrap();
        match frame {
            InboundSettingsEditorFrame::Commit(c) => {
                assert_eq!(c.value["path"], "/x");
                assert_eq!(c.value["id"], 7);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn close_params_optional_reason() {
        let frame = parse_inbound(METHOD_CLOSE, json!({})).unwrap();
        match frame {
            InboundSettingsEditorFrame::Close(c) => assert!(c.reason.is_none()),
            _ => panic!(),
        }
        let frame = parse_inbound(METHOD_CLOSE, json!({"reason":"cancelled"})).unwrap();
        match frame {
            InboundSettingsEditorFrame::Close(c) => assert_eq!(c.reason.as_deref(), Some("cancelled")),
            _ => panic!(),
        }
    }

    #[test]
    fn unknown_method_rejected() {
        let err = parse_inbound("settings.editor.bogus", json!({})).unwrap_err();
        assert!(matches!(err, SettingsEditorParseError::UnknownMethod(_)));
    }

    #[test]
    fn invalid_render_params_rejected() {
        // `rows` must be an array.
        let err = parse_inbound(METHOD_RENDER, json!({"rows": "nope"})).unwrap_err();
        assert!(matches!(
            err,
            SettingsEditorParseError::InvalidParams {
                method: METHOD_RENDER,
                ..
            }
        ));
    }

    #[test]
    fn key_params_round_trip() {
        let p = SettingsEditorKeyParams { key: "Down".into() };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v, json!({"key":"Down"}));
    }
}
