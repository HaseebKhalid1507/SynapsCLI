use super::types::Event;

/// Format an event as a system message the agent can understand.
/// Example: `[alert/high from uptime-kuma #alerts] Jellyfin is DOWN. Status 503.`
pub fn format_event_for_agent(event: &Event) -> String {
    let sev = event
        .content
        .severity
        .as_ref()
        .map(|s| s.as_str())
        .unwrap_or("medium");

    let header = match &event.channel {
        Some(ch) => format!(
            "[{}/{} from {} #{}]",
            event.content.content_type, sev, event.source.source_type, ch.name
        ),
        None => format!(
            "[{}/{} from {}]",
            event.content.content_type, sev, event.source.source_type
        ),
    };

    let mut out = format!("{} {}", header, event.content.text);

    if let Some(data) = &event.content.data {
        out.push_str(&format!(
            "\nData: {}",
            serde_json::to_string(data).unwrap_or_default()
        ));
    }

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
        assert_eq!(s, "[message/low from cli] ping");
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
        assert_eq!(
            s,
            "[alert/high from uptime-kuma #alerts] Jellyfin is DOWN. Status 503."
        );
    }

    #[test]
    fn format_defaults_to_medium_when_no_severity() {
        let e = Event::simple("cli", "hi", None);
        let s = format_event_for_agent(&e);
        assert!(s.starts_with("[message/medium from cli]"));
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
