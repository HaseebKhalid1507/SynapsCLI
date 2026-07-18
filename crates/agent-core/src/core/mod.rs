//! Core infrastructure — config, session, auth, logging, error types, protocol.

pub mod auth;
pub mod chain;
pub mod config;
pub mod error;
pub mod logging;
pub mod models;
pub mod private_fs;
pub mod protocol;
pub mod reasoning;
pub mod rpc_dispatch;
pub mod rpc_protocol;
pub mod session;
pub mod session_index;
pub mod shell_config;
pub mod stream_types;
pub mod watcher_types;
