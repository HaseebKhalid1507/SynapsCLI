use super::types::Event;

/// Strip any variation of </event> tags (case-insensitive, with whitespace)
fn regex_strip_event_close(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let lower = s.to_lowercase();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Check for </ event > pattern at current position
        if i + 7 < bytes.len() && &lower[i..i+2] == "</" {
            // Scan for "event" after optional whitespace
            let mut j = i + 2;
            while j < bytes.len() && lower.as_bytes()[j] == b' ' { j += 1; }
            if j + 5 <= bytes.len() && &lower[j..j+5] == "event" {
                let mut k = j + 5;
                while k < bytes.len() && lower.as_bytes()[k] == b' ' { k += 1; }
                if k < bytes.len() && bytes[k] == b'>' {
                    i = k + 1; // skip the entire closing tag
                    continue;
                }
            }
        }
        result.push(s.as_bytes()[i] as char);
        i += 1;
    }
    result
}

/// Format an event as a system message the agent can understand.
/// Wrapped in XML tags to prevent prompt injection — the model should treat
/// content inside <event> tags as DATA, not instructions.
/// Example: `<event id="abc" type="alert" severity="high" source="uptime-kuma" channel="alerts">Jellyfin is DOWN.</event>`
pub fn format_event_for_agent(event: &Event) -> String {
    let sev = event
        .content
        .severity
        .as_ref()
        .map(|s| s.as_str())
        .unwrap_or("medium");

    let channel_attr = match &event.channel {
        Some(ch) => format!(" channel=\"{}\"", ch.name.replace('"', "'")),
        None => String::new(),
    };

    // Sanitize text — strip any closing </event> tags to prevent breakout
    let safe_text = regex_strip_event_close(&event.content.text);
    let safe_source = event.source.source_type.replace('"', "'");
    let safe_content_type = event.content.content_type.replace('"', "'");

    let mut out = format!(
        "<event id=\"{}\" type=\"{}\" severity=\"{}\" source=\"{}\"{}>{}",
        event.id, safe_content_type, sev, safe_source, channel_attr, safe_text
    );

    if let Some(data) = &event.content.data {
        let data_str = serde_json::to_string(data).unwrap_or_default();
        // Cap data size to prevent token abuse
        let truncated: String = data_str.chars().take(1000).collect();
        // Strip closing event tags from data (case-insensitive) to prevent breakout
        let safe_data = regex_strip_event_close(&truncated);
        out.push_str(&format!("\nData: {}", safe_data));
    }

    out.push_str("</event>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::types::{EventChannel, Severity};
    use serde_json::json;

    #[test]
    fn format_without_channel() {
        let e = Event::simple("cli", "ping", Some(Severity::Low));
        let s = format_event_for_agent(&e);
        assert!(s.starts_with("<event id="));
        assert!(s.contains("type=\"message\""));
        assert!(s.contains("severity=\"low\""));
        assert!(s.contains("source=\"cli\""));
        assert!(s.contains("ping"));
        assert!(s.ends_with("</event>"));
    }

    #[test]
    fn format_with_channel() {
        let mut e = Event::simple("uptime-kuma", "Jellyfin is DOWN. Status 503.", Some(Severity::High));
        e.content.content_type = "alert".into();
        e.channel = Some(EventChannel {
            id: "1".into(),
            name: "alerts".into(),
        });
        let s = format_event_for_agent(&e);
        assert!(s.contains("source=\"uptime-kuma\""));
        assert!(s.contains("channel=\"alerts\""));
        assert!(s.contains("severity=\"high\""));
        assert!(s.contains("Jellyfin is DOWN. Status 503."));
        assert!(s.ends_with("</event>"));
    }

    #[test]
    fn format_defaults_to_medium_when_no_severity() {
        let e = Event::simple("cli", "hi", None);
        let s = format_event_for_agent(&e);
        assert!(s.contains("severity=\"medium\""));
    }

    #[test]
    fn format_appends_data() {
        let mut e = Event::simple("system", "boom", Some(Severity::Critical));
        e.content.data = Some(json!({"code": 500}));
        let s = format_event_for_agent(&e);
        assert!(s.contains("boom"));
        assert!(s.contains("\nData: "));
        assert!(s.contains("\"code\":500"));
    }
}
