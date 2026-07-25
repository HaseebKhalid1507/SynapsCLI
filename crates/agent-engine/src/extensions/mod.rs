//! Extension system for SynapsCLI.
//!
//! Provides compiled-in hook call sites (`HookBus`) and external extension
//! runtimes that can subscribe to hooks, register tools, and register providers
//! via a stable JSON-RPC 2.0 protocol.
//!
//! # Architecture
//!
//! ```text
//! SynapsCLI binary
//!   ├─ HookBus (dispatcher)          ← this module
//!   ├─ ExtensionManager (lifecycle)  ← this module
//!   └─ optional external extensions
//!         └─ Process/JSON-RPC runtime ← phase 1
//! ```

pub mod active_tasks;
pub mod audit;
pub mod capability;
pub mod commands;
pub mod config;
pub mod config_store;
pub mod context_provider;
pub mod hooks;
pub mod info;
pub mod invoke_output;
pub mod lease;
pub mod lifecycle;
pub mod loader;
pub mod manager;
pub mod manifest;
pub mod permissions;
pub mod providers;
pub mod runtime;
pub mod settings_editor;
pub mod tasks;
pub mod trust;
pub mod validation;
pub mod widgets;
