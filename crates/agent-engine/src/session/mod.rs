//! Session actor surface (Phase 2 daemon mode).
//!
//! One `SessionActor` (A1) owns THE `Runtime` + `ConversationState` for a
//! conversation and runs its turn machine; clients talk to it through a
//! `ClientTransport` — `LocalTransport` in-process (same `StreamEvent`
//! values, never serialised = byte-identical), `SocketTransport` (B2) over
//! the daemon UDS. `agent-tui` must never be a dependency of anything here.

pub mod actor;
pub mod actor_cmds;
pub mod budgets;
pub mod cache_keepwarm;
mod actor_capacity;
mod actor_held;
pub mod capacity_resume;
pub mod child_events;
mod child_spend;
/// Test-only controls of the actor's child-spend passes (`cfg(test)` / the
/// `testing` feature).
#[cfg(any(test, feature = "testing"))]
pub use child_spend::testing as child_spend_testing;
pub mod children;
pub mod client_args;
pub mod client_tools;
mod actor_client_tools;
pub mod display;
pub mod driver;
pub mod fork;
pub mod handle;
pub mod held;
pub mod held_store;
pub mod live;
mod persister;
/// Test-only counts of the actor's queued saves (`cfg(test)` / the
/// `testing` feature).
#[cfg(any(test, feature = "testing"))]
pub use persister::testing as persister_testing;
pub(crate) mod session_start;
/// Test-only controls of `on_session_start` scheduling (`cfg(test)` / the
/// `testing` feature).
#[cfg(any(test, feature = "testing"))]
pub use session_start::testing as session_start_testing;
pub mod socket_transport;
pub mod transport;
pub mod types;
pub mod view;
pub mod wire;

pub use actor::{SessionActor, SessionTask};
pub use display::{DisplayItem, DisplayTail};
pub use handle::SessionHandle;
pub use socket_transport::SocketTransport;
pub use transport::{ClientTransport, LocalTransport, TransportError};
pub use types::*;
pub use view::{RuntimeRead, RuntimeView};
