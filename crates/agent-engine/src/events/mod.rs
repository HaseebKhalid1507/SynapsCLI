//! Event Bus — universal message ingestion for agent sessions.
//!
//! Any external system (Discord, Slack, Uptime Kuma, cron, CLI, other agents)
//! can push events into a running session. Events are formatted as system
//! messages with source metadata, allowing the agent to respond through
//! the appropriate channel.

pub mod format;
pub mod ingest;
pub mod queue;
pub mod registry;
pub mod socket;
pub mod types;

pub use format::format_event_for_agent;
pub use ingest::watch_inbox;
pub use queue::EventQueue;
pub use registry::*;
pub use types::{Event, EventChannel, EventContent, EventSender, EventSource, Severity};
