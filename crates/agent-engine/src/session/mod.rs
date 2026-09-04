//! Session actor surface (Phase 2 daemon mode).
//!
//! One `SessionActor` (A1) owns THE `Runtime` + `ConversationState` for a
//! conversation and runs its turn machine; clients talk to it through a
//! `ClientTransport` — `LocalTransport` in-process (same `StreamEvent`
//! values, never serialised = byte-identical), `SocketTransport` (B2) over
//! the daemon UDS. `agent-tui` must never be a dependency of anything here.

pub mod actor;
pub mod budgets;
pub mod handle;
pub mod socket_transport;
pub mod transport;
pub mod types;
pub mod view;
pub mod wire;

pub use actor::{SessionActor, SessionTask};
pub use handle::SessionHandle;
pub use socket_transport::SocketTransport;
pub use transport::{ClientTransport, LocalTransport, TransportError};
pub use types::*;
pub use view::{RuntimeRead, RuntimeView};
