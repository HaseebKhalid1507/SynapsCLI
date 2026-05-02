//! Voice integration: sidecar discovery, supervision, and protocol.
//!
//! The actual STT runtime lives in the `local-voice-plugin` from the
//! `synaps-skills` repo. This module owns:
//!
//! - The line-JSON sidecar protocol types (see `protocol`).
//! - Sidecar process lifecycle (forthcoming `manager` module).
//! - The runtime state machine driving the `/voice` command and the
//!   listening indicator (forthcoming `state` module).
//!
//! Voice metadata (display name, modes, endpoint) is declared via the
//! generic Phase 7 capability contract — see
//! `crate::extensions::runtime::process::CapabilityDeclaration`.

pub mod discovery;
pub mod manager;
pub mod protocol;
