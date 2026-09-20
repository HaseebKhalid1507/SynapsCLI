#[cfg(not(target_os = "linux"))]
compile_error!("synaps-axel-memory-service requires Linux proc-fd filesystem anchoring");
pub mod contract;
mod forum;
#[path = "../../../crates/agent-core/src/memory/forum.rs"]
pub mod forum_contract;
mod private_path;
pub mod protocol;
pub mod service;

mod capture;
mod database;
mod history;
mod legacy;
mod migration;
mod retention;

mod repository_proof;
mod scope;
