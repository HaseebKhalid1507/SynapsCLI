//! Event Bus — universal message ingestion for agent sessions.
//!
//! Any external system (Discord, Slack, Uptime Kuma, cron, CLI, other agents)
//! can push events into a running session. Events are formatted as system
//! messages with source metadata, allowing the agent to respond through
//! the appropriate channel.

pub mod types;
pub mod queue;
pub mod format;
pub mod ingest;
pub mod socket;
pub mod registry;

pub use types::{Event, EventSource, EventChannel, EventSender, EventContent, Severity};
pub use queue::EventQueue;
pub use format::format_event_for_agent;
pub use ingest::watch_inbox;
pub use registry::*;
