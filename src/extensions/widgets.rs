//! Plugin widget notification types and parser.
//!
//! Phase B Phase 3 contract — see
//! `docs/plans/2026-05-03-extension-contracts-for-rich-plugins.md`.
//!
//! Plugins push spontaneous JSON-RPC notifications to create, update, or
//! dismiss persistent on-screen widgets (HUD panels, status boxes, etc.)
//! outside the slash-command request/response cycle.
//! Method names: `widget.upsert`, `widget.dismiss`.
//!
//! Wire shapes (params per method):
//!
//! - `widget.upsert`:  `{ id, lines: string[], position?: string, title?: string, ttl_secs?: number | null }`
//! - `widget.dismiss`: `{ id }`
//!
//! Valid `position` values: `"top_left"`, `"top_center"`, `"top_right"`,
//! `"middle_left"`, `"center"`, `"middle_right"`, `"bottom_left"`,
//! `"bottom_center"`, `"bottom_right"`. Defaults to `"top_right"`.
//!
//! `ttl_secs: None` means the widget is persistent (no auto-dismiss).
//! `ttl_secs: Some(n)` means the widget auto-dismisses after `n` seconds.

use serde_json::Value;

/// Default widget position when the plugin omits the field.
const DEFAULT_POSITION: &str = "top_right";

/// All valid position string values accepted over the wire.
const VALID_POSITIONS: &[&str] = &[
    "top_left",
    "top_center",
    "top_right",
    "middle_left",
    "center",
    "middle_right",
    "bottom_left",
    "bottom_center",
    "bottom_right",
];

/// A single styled text span sent over the wire from an extension.
/// Extensions specify colors as CSS-style hex strings (e.g. `"#ff0000"`).
#[derive(Debug, Clone, PartialEq)]
pub struct StyledSpan {
    pub text: String,
    pub fg: Option<String>,
    pub bg: Option<String>,
}

/// Parsed widget notification.
#[derive(Debug, Clone, PartialEq)]
pub enum WidgetEvent {
    /// Create or update a widget. `position` is stored as a raw string; the
    /// TUI layer is responsible for converting it to a `ToastPosition`.
    Upsert {
        id: String,
        lines: Vec<String>,
        /// Optional per-span styled lines. Each inner Vec is one display line,
        /// each `StyledSpan` carries text + optional fg/bg colors. When present,
        /// the TUI renders these instead of plain `lines`.
        styled_lines: Option<Vec<Vec<StyledSpan>>>,
        /// One of the nine position strings; always present (defaults to
        /// `"top_right"` when the plugin omits the field).
        position: String,
        title: Option<String>,
        /// `None` → persistent widget; `Some(n)` → auto-dismiss after n secs.
        ttl_secs: Option<u64>,
    },
    /// Remove a widget by id.
    Dismiss { id: String },
}

impl WidgetEvent {
    pub fn id(&self) -> &str {
        match self {
            WidgetEvent::Upsert { id, .. } | WidgetEvent::Dismiss { id } => id,
        }
    }
}

/// Returns true if `method` is one of the recognised widget notifications.
pub fn is_widget_method(method: &str) -> bool {
    matches!(method, "widget.upsert" | "widget.dismiss")
}

/// Parse a `widget.*` notification given the JSON-RPC method and params.
pub fn parse_widget_event(method: &str, params: &Value) -> Result<WidgetEvent, String> {
    let obj = params
        .as_object()
        .ok_or_else(|| format!("{method} params must be a JSON object"))?;

    let id = obj
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{method} missing 'id'"))?
        .to_string();
    if id.is_empty() {
        return Err(format!("{method} 'id' must be non-empty"));
    }

    match method {
        "widget.upsert" => {
            // --- lines ---
            let lines_raw = obj
                .get("lines")
                .ok_or_else(|| "widget.upsert missing 'lines'".to_string())?;
            let lines_arr = lines_raw
                .as_array()
                .ok_or_else(|| "widget.upsert 'lines' must be an array".to_string())?;
            let mut lines = Vec::with_capacity(lines_arr.len());
            for (i, v) in lines_arr.iter().enumerate() {
                let s = v.as_str().ok_or_else(|| {
                    format!("widget.upsert 'lines[{i}]' must be a string")
                })?;
                lines.push(s.to_string());
            }

            // --- position ---
            let position = match obj.get("position") {
                None => DEFAULT_POSITION.to_string(),
                Some(Value::Null) => DEFAULT_POSITION.to_string(),
                Some(v) => {
                    let s = v
                        .as_str()
                        .ok_or_else(|| "widget.upsert 'position' must be a string".to_string())?;
                    if !VALID_POSITIONS.contains(&s) {
                        return Err(format!("widget.upsert unknown position '{s}'"));
                    }
                    s.to_string()
                }
            };

            // --- title ---
            let title = match obj.get("title") {
                None | Some(Value::Null) => None,
                Some(v) => {
                    let s = v
                        .as_str()
                        .ok_or_else(|| "widget.upsert 'title' must be a string".to_string())?;
                    if s.is_empty() { None } else { Some(s.to_string()) }
                }
            };

            // --- ttl_secs ---
            let ttl_secs = match obj.get("ttl_secs") {
                None | Some(Value::Null) => None,
                Some(v) => {
                    let n = v.as_u64().ok_or_else(|| {
                        format!(
                            "widget.upsert 'ttl_secs' must be a non-negative integer or null, got {v}"
                        )
                    })?;
                    Some(n)
                }
            };

            // --- styled_lines (optional) ---
            let styled_lines = match obj.get("styled_lines") {
                None | Some(Value::Null) => None,
                Some(v) => {
                    let outer = v.as_array().ok_or_else(|| {
                        "widget.upsert 'styled_lines' must be an array of arrays".to_string()
                    })?;
                    let mut result = Vec::with_capacity(outer.len());
                    for (i, row) in outer.iter().enumerate() {
                        let spans_arr = row.as_array().ok_or_else(|| {
                            format!("widget.upsert 'styled_lines[{i}]' must be an array of span objects")
                        })?;
                        let mut spans = Vec::with_capacity(spans_arr.len());
                        for (j, span_val) in spans_arr.iter().enumerate() {
                            let span_obj = span_val.as_object().ok_or_else(|| {
                                format!("widget.upsert 'styled_lines[{i}][{j}]' must be an object")
                            })?;
                            let text = span_obj.get("text")
                                .and_then(Value::as_str)
                                .ok_or_else(|| {
                                    format!("widget.upsert 'styled_lines[{i}][{j}].text' must be a string")
                                })?
                                .to_string();
                            let fg = span_obj.get("fg")
                                .and_then(Value::as_str)
                                .map(str::to_string);
                            let bg = span_obj.get("bg")
                                .and_then(Value::as_str)
                                .map(str::to_string);
                            spans.push(StyledSpan { text, fg, bg });
                        }
                        result.push(spans);
                    }
                    Some(result)
                }
            };

            Ok(WidgetEvent::Upsert { id, lines, styled_lines, position, title, ttl_secs })
        }

        "widget.dismiss" => Ok(WidgetEvent::Dismiss { id }),

        other => Err(format!("not a widget method: {other}")),
    }
}

/// Event sent from a background notification watcher to the TUI.
/// Carries the source extension id and the parsed widget event.
#[derive(Debug, Clone)]
pub struct ExtensionWidgetEvent {
    pub extension_id: String,
    pub event: WidgetEvent,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── widget.upsert ─────────────────────────────────────────────────────────

    #[test]
    fn parses_upsert_minimal() {
        let ev = parse_widget_event(
            "widget.upsert",
            &json!({"id": "status", "lines": ["hello"]}),
        )
        .unwrap();
        assert_eq!(
            ev,
            WidgetEvent::Upsert {
                id: "status".into(),
                lines: vec!["hello".into()],
                position: "top_right".into(),
                title: None,
                ttl_secs: None,
                styled_lines: None,
            }
        );
    }

    #[test]
    fn parses_upsert_full() {
        let ev = parse_widget_event(
            "widget.upsert",
            &json!({
                "id": "hud",
                "lines": ["line one", "line two"],
                "position": "bottom_left",
                "title": "My Widget",
                "ttl_secs": 30
            }),
        )
        .unwrap();
        assert_eq!(
            ev,
            WidgetEvent::Upsert {
                id: "hud".into(),
                lines: vec!["line one".into(), "line two".into()],
                position: "bottom_left".into(),
                title: Some("My Widget".into()),
                ttl_secs: Some(30),
                styled_lines: None,
            }
        );
    }

    #[test]
    fn upsert_null_position_defaults_to_top_right() {
        let ev = parse_widget_event(
            "widget.upsert",
            &json!({"id": "w", "lines": [], "position": null}),
        )
        .unwrap();
        assert!(matches!(
            ev,
            WidgetEvent::Upsert { position, .. } if position == "top_right"
        ));
    }

    #[test]
    fn upsert_null_ttl_means_persistent() {
        let ev = parse_widget_event(
            "widget.upsert",
            &json!({"id": "w", "lines": [], "ttl_secs": null}),
        )
        .unwrap();
        assert!(matches!(ev, WidgetEvent::Upsert { ttl_secs: None, .. }));
    }

    #[test]
    fn upsert_empty_lines_is_valid() {
        let ev = parse_widget_event(
            "widget.upsert",
            &json!({"id": "w", "lines": []}),
        )
        .unwrap();
        assert!(matches!(ev, WidgetEvent::Upsert { lines, .. } if lines.is_empty()));
    }

    #[test]
    fn upsert_empty_title_coerces_to_none() {
        let ev = parse_widget_event(
            "widget.upsert",
            &json!({"id": "w", "lines": [], "title": ""}),
        )
        .unwrap();
        assert!(matches!(ev, WidgetEvent::Upsert { title: None, .. }));
    }

    #[test]
    fn upsert_all_positions_accepted() {
        for pos in VALID_POSITIONS {
            let ev = parse_widget_event(
                "widget.upsert",
                &json!({"id": "w", "lines": [], "position": pos}),
            )
            .unwrap();
            assert!(
                matches!(&ev, WidgetEvent::Upsert { position, .. } if position == pos),
                "position '{pos}' was rejected"
            );
        }
    }

    // ── widget.dismiss ────────────────────────────────────────────────────────

    #[test]
    fn parses_dismiss() {
        let ev =
            parse_widget_event("widget.dismiss", &json!({"id": "hud"})).unwrap();
        assert_eq!(ev, WidgetEvent::Dismiss { id: "hud".into() });
    }

    // ── error cases ───────────────────────────────────────────────────────────

    #[test]
    fn rejects_missing_id() {
        assert!(parse_widget_event("widget.upsert", &json!({"lines": []})).is_err());
        assert!(parse_widget_event("widget.dismiss", &json!({})).is_err());
    }

    #[test]
    fn rejects_empty_id() {
        assert!(
            parse_widget_event("widget.upsert", &json!({"id": "", "lines": []})).is_err()
        );
    }

    #[test]
    fn rejects_missing_lines() {
        assert!(parse_widget_event("widget.upsert", &json!({"id": "w"})).is_err());
    }

    #[test]
    fn rejects_non_string_line_element() {
        let err = parse_widget_event(
            "widget.upsert",
            &json!({"id": "w", "lines": ["ok", 42]}),
        )
        .unwrap_err();
        assert!(err.contains("lines[1]"));
    }

    #[test]
    fn rejects_unknown_position() {
        let err = parse_widget_event(
            "widget.upsert",
            &json!({"id": "w", "lines": [], "position": "floating"}),
        )
        .unwrap_err();
        assert!(err.contains("unknown position"));
    }

    #[test]
    fn rejects_non_object_params() {
        assert!(parse_widget_event("widget.upsert", &json!("bad")).is_err());
        assert!(parse_widget_event("widget.dismiss", &json!(null)).is_err());
    }

    #[test]
    fn rejects_unknown_method() {
        let err =
            parse_widget_event("widget.flash", &json!({"id": "w"})).unwrap_err();
        assert!(err.contains("not a widget method"));
    }

    // ── helpers ───────────────────────────────────────────────────────────────

    #[test]
    fn is_widget_method_works() {
        assert!(is_widget_method("widget.upsert"));
        assert!(is_widget_method("widget.dismiss"));
        assert!(!is_widget_method("widget.flash"));
        assert!(!is_widget_method("task.start"));
        assert!(!is_widget_method(""));
    }

    #[test]
    fn event_id_helper() {
        let upsert = parse_widget_event(
            "widget.upsert",
            &json!({"id": "my-widget", "lines": []}),
        )
        .unwrap();
        assert_eq!(upsert.id(), "my-widget");

        let dismiss =
            parse_widget_event("widget.dismiss", &json!({"id": "my-widget"})).unwrap();
        assert_eq!(dismiss.id(), "my-widget");
    }
}
