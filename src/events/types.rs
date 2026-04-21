use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub fn as_str(&self) -> &str {
        match self {
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
            Severity::Critical => "critical",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "low" => Severity::Low,
            "high" => Severity::High,
            "critical" => Severity::Critical,
            _ => Severity::Medium,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventSource {
    pub source_type: String,
    pub name: String,
    pub callback: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventChannel {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventSender {
    pub id: String,
    pub name: String,
    pub sender_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventContent {
    pub text: String,
    pub content_type: String,
    pub severity: Option<Severity>,
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub timestamp: DateTime<Utc>,
    pub source: EventSource,
    pub channel: Option<EventChannel>,
    pub sender: Option<EventSender>,
    pub content: EventContent,
    pub expects_response: bool,
    pub reply_to: Option<String>,
}

impl Event {
    /// Create a simple event from minimal params (for CLI / quick use).
    pub fn simple(source_type: &str, text: &str, severity: Option<Severity>) -> Self {
        Event {
            id: Uuid::new_v4().to_string(),
            timestamp: Utc::now(),
            source: EventSource {
                source_type: source_type.to_string(),
                name: source_type.to_string(),
                callback: None,
            },
            channel: None,
            sender: None,
            content: EventContent {
                text: text.to_string(),
                content_type: "message".to_string(),
                severity,
                data: None,
            },
            expects_response: false,
            reply_to: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_ordering() {
        assert!(Severity::Low < Severity::Medium);
        assert!(Severity::Medium < Severity::High);
        assert!(Severity::High < Severity::Critical);
    }

    #[test]
    fn severity_str_roundtrip() {
        for s in [Severity::Low, Severity::Medium, Severity::High, Severity::Critical] {
            assert_eq!(Severity::from_str(s.as_str()), s);
        }
        assert_eq!(Severity::from_str("garbage"), Severity::Medium);
    }

    #[test]
    fn event_simple_defaults() {
        let e = Event::simple("cli", "hello", Some(Severity::High));
        assert_eq!(e.source.source_type, "cli");
        assert_eq!(e.content.text, "hello");
        assert_eq!(e.content.severity, Some(Severity::High));
        assert!(!e.expects_response);
        assert!(e.channel.is_none());
    }

    #[test]
    fn event_serde_roundtrip() {
        let e = Event::simple("discord", "yo", Some(Severity::Critical));
        let json = serde_json::to_string(&e).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, e.id);
        assert_eq!(back.content.text, "yo");
        assert_eq!(back.content.severity, Some(Severity::Critical));
    }
}
