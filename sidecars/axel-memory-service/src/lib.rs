#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!(
    "synaps-axel-memory-service requires Linux proc-fd or macOS volfs filesystem anchoring"
);
pub mod contract;
mod forum;
#[path = "../../../crates/agent-core/src/memory/forum.rs"]
pub mod forum_contract;
#[path = "../../../crates/agent-core/src/git_program.rs"]
pub mod git_program;
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
