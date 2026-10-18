//! Hermetic config for integration-test binaries that don't otherwise pin
//! one (06 I3).
//!
//! Integration tests link the engine without `cfg(test)`, so several of its
//! constructors still fall back to the ambient config: `ExtensionManager::new`,
//! `ProcessExtension::spawn*` and its inbox, the extension tools' memory
//! check (`MemoryBinding::configured_current()` → `load_config()`). Unpinned,
//! a test binary reads the developer's real `~/.synaps-cli/config`, so with
//! `memory.backend = axel` every manager ran with `exclusive_memory = true`
//! and latched that config's identity and provider keys. CI's clean HOME
//! never saw it.
//!
//! [`pin`] points `SYNAPS_BASE_DIR` at one private temp dir for the whole
//! process, holding an explicit legacy memory config, the first time any
//! test calls it. Call it first thing in every test of a binary that uses
//! it: the binary must not touch `SYNAPS_BASE_DIR` any other way. The dir
//! lives until the process exits.
#![allow(dead_code)]

use std::path::Path;
use std::sync::OnceLock;

static BASE: OnceLock<tempfile::TempDir> = OnceLock::new();

pub fn pin() -> &'static Path {
    BASE.get_or_init(|| {
        let dir = tempfile::Builder::new().prefix("synaps-hermetic-").tempdir().expect("hermetic base dir");
        std::fs::write(dir.path().join("config"), "memory.backend = legacy\n").expect("hermetic config");
        std::env::set_var("SYNAPS_BASE_DIR", dir.path());
        dir
    })
    .path()
}
