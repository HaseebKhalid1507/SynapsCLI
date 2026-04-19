//! Interactive PTY-based shell sessions for agents.
//!
//! Provides three tools: `shell_start`, `shell_send`, `shell_end` that let agents
//! drive persistent interactive terminal sessions (SSH, REPLs, debuggers, etc).

pub mod config;
pub mod pty;
pub mod readiness;
pub mod session;
mod start;
mod send;
mod end;

pub use config::ShellConfig;
pub use session::SessionManager;
pub use start::ShellStartTool;
pub use send::ShellSendTool;
pub use end::ShellEndTool;
