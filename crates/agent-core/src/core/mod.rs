//! Core infrastructure — config, session, auth, logging, error types, protocol.

pub mod auth;
pub mod chain;
pub mod compaction;
pub mod config;
pub mod context_head;
pub mod context_policy;
pub mod disclosure;
pub mod error;
pub mod extension_context;
pub mod host_note;
pub mod lock_file;
pub mod logging;
pub mod memstat;
pub mod models;
pub mod private_fs;
pub mod reasoning;
pub mod retention;
pub mod rpc_dispatch;
pub mod rpc_protocol;
pub mod session;
pub mod session_catalog;
pub mod session_draft;
pub mod session_index;
pub mod session_journal;
pub mod session_lock;
mod session_save_order;
/// Agent cards' durability: the `.ui` sidecar, its store and recovery
/// (agent-ui v3.2, C2). Inert: nothing is wired to a session yet.
pub mod session_surfaces;
pub mod stream_types;
pub mod tool_spill;
/// Agent cards: the card model's pure data types and bounds (agent-ui v3.2
/// #12). Inert: only `agent_engine::ui` uses them.
pub mod ui_types;
