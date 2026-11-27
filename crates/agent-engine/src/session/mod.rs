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
mod cache_expiry;
/// Test-only: hold `CacheExpiry` jobs (`cfg(test)` / the `testing`
/// feature).
#[cfg(any(test, feature = "testing"))]
pub use cache_expiry::testing as cache_expiry_testing;
pub mod cache_keepwarm;
pub mod client_args;
pub mod display;
pub mod driver;
mod fresh_id;
/// Test-only: force the ids fresh sessions draw (`cfg(test)` / the
/// `testing` feature).
#[cfg(any(test, feature = "testing"))]
pub use fresh_id::testing as fresh_id_testing;
pub mod handle;
pub(crate) mod owned;
mod persister;
pub mod query;
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
pub use cache_expiry::{render_cache_expiry, CacheExpiry, CacheReason, CacheState};
pub use display::{DisplayItem, DisplayTail};
pub use handle::SessionHandle;
pub use query::{decode_query_reply, QueryError, QueryErrorCode};
pub use socket_transport::SocketTransport;
pub use transport::{ClientTransport, LocalTransport, TransportError};
pub use types::*;
pub use view::{RuntimeRead, RuntimeView};
