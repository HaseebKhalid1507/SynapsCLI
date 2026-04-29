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

pub mod hooks;
pub mod permissions;
pub mod manifest;
pub mod runtime;
pub mod manager;
