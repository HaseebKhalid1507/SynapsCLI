//! /plugins full-screen modal.

pub(crate) mod state;
pub(crate) mod draw;
pub(crate) mod input;

pub(crate) use state::PluginsModalState;
pub(crate) use draw::render;
pub(crate) use input::{handle_event, InputOutcome};
