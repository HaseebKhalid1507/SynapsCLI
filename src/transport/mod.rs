//! Transport abstraction — trait, events, bus, and driver.
//!
//! Two-tier consumer model:
//! - Tier 1: Direct bus consumers (TUI) use bus.subscribe() + bus.inbound()
//! - Tier 2: Transport trait impls (stdio, websocket, discord) use bus.connect()

pub mod events;
pub mod inbound;
pub mod sync;
pub mod bus;
pub mod driver;
pub mod stdio;
pub mod websocket;
pub mod agent;

pub use events::{AgentEvent, ToolEvent, SubagentEvent, MetaEvent};
pub use inbound::Inbound;
pub use sync::SyncState;
pub use bus::{AgentBus, BusHandle};
pub use driver::ConversationDriver;
pub use stdio::StdioTransport;
pub use websocket::WebSocketTransport;
pub use agent::AgentHarness;

use async_trait::async_trait;

/// A Transport is a simple I/O adapter for the AgentBus.
/// For complex consumers (TUI) that need their own event loop,
/// use bus.subscribe() + bus.inbound() directly instead.
#[async_trait]
pub trait Transport: Send + 'static {
    /// Receive the next inbound message. Return None to disconnect.
    async fn recv(&mut self) -> Option<Inbound>;

    /// Send an agent event. Return false if disconnected.
    async fn send(&mut self, event: AgentEvent) -> bool;

    /// Called once on connect with session state snapshot.
    async fn on_sync(&mut self, _state: SyncState) {}

    /// Human-readable name for logging.
    fn name(&self) -> &str { "unknown" }
}