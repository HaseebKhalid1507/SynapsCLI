//! Engine — shared business logic for both TUI and headless modes.
//!
//! The engine owns the runtime, session, extensions, and event bus.
//! Renderers (chatui TUI, headless chat) call into the engine for
//! all non-visual operations.

pub mod setup;
pub mod commands;
