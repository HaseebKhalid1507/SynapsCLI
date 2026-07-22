//! Interactive PTY-based shell sessions for agents.
//!
//! Provides three tools: `shell_start`, `shell_send`, `shell_end` that let agents
//! drive persistent interactive terminal sessions (SSH, REPLs, debuggers, etc).

pub mod config;
mod end;
pub mod pty;
pub mod readiness;
mod send;
pub mod session;
mod start;

pub use config::ShellConfig;
pub use end::ShellEndTool;
pub use pty::{pty_output_snapshot, PtyOutputSnapshot};
pub use send::ShellSendTool;
pub use session::{start_reaper, SendResult, SessionManager, SessionOpts};
pub use start::ShellStartTool;
