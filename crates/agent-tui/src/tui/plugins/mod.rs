//! /plugins full-screen modal.

pub(crate) mod actions;
pub(crate) mod draw;
pub(crate) mod input;
pub(crate) mod progress;
pub(crate) mod state;

pub(crate) use draw::render;
pub(crate) use input::{handle_event, InputOutcome};
pub(crate) use state::PluginsModalState;
