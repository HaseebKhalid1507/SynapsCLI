//! /plugins full-screen modal.
// Task 13 wires render/handle_event into main.rs; until then these re-exports are unused.
#![allow(unused_imports, dead_code)]

pub(crate) mod state;
pub(crate) mod draw;
pub(crate) mod input;

pub(crate) use state::PluginsModalState;
pub(crate) use draw::render;
pub(crate) use input::{handle_event, InputOutcome};
