//! Sidecar plugin support: long-running plugin processes that stream
//! events into the host over a JSONL line protocol.
//!
//! This module is **plugin-agnostic**. It hosts whatever a plugin
//! declares itself to be, as long as that plugin fits the sidecar
//! line-protocol contract.
//!
//! It owns:
//! - The line-JSON sidecar protocol types (see [`protocol`]).
//! - Sidecar process lifecycle and supervision (see [`manager`]).
//! - Plugin discovery for sidecar binaries (see [`discovery`]).
//!
//! Capability metadata (display name, kind, permissions, params) is
//! declared via the generic capability contract — see
//! [`crate::extensions::runtime::process::CapabilityDeclaration`].

pub mod discovery;
pub mod manager;
pub mod protocol;
pub mod spawn;
