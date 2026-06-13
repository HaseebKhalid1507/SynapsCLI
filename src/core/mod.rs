//! Core infrastructure — config, session, auth, logging, error types, protocol.

pub mod shell_config;
pub mod config;
pub mod session;
pub mod auth;
pub mod logging;
pub mod protocol;
pub mod error;
pub mod watcher_types;
pub mod models;
pub mod chain;
pub mod session_index;
pub mod rpc_protocol;
pub mod rpc_dispatch;
