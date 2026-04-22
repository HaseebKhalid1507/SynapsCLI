//! Chain pointer management — named bookmarks that auto-advance on compaction.
//!
//! A chain is just a file containing a session ID. When a session is compacted,
//! any chain pointing at the old session head is advanced to the new session.
//! All I/O is sync (std::fs) — chain files are tiny (<100 bytes).

use serde::{Deserialize, Serialize};
use std::io;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainPointer {
    pub head: String,
}

#[derive(Debug, Clone)]
pub struct NamedChain {
    pub name: String,
    pub head: String,
}

pub fn chains_dir() -> PathBuf {
    crate::config::get_active_config_dir().join("chains")
}

pub fn chain_path(name: &str) -> PathBuf {
    chains_dir().join(format!("{}.json", name))
}

fn validate_chain_name(name: &str) -> io::Result<()> {
    crate::session::validate_name(name).map_err(io::Error::other)
}

pub fn load_chain(name: &str) -> io::Result<ChainPointer> {
    validate_chain_name(name)?;
    let path = chain_path(name);
    let content = std::fs::read_to_string(&path)?;
    serde_json::from_str(&content).map_err(io::Error::other)
}

pub fn save_chain(name: &str, head: &str) -> io::Result<()> {
    validate_chain_name(name)?;

    // Soft guard: warn if a session uses the same name.
    if let Ok(sessions) = crate::session::list_sessions() {
        if sessions.iter().any(|s| s.name.as_deref() == Some(name)) {
            tracing::warn!(
                "chain name '{}' also used by a session — resolver prefers chains",
                name
            );
        }
    }

    let dir = chains_dir();
    std::fs::create_dir_all(&dir)?;
    let path = chain_path(name);
    let tmp = path.with_extension("tmp");
    let ptr = ChainPointer { head: head.to_string() };
    let json = serde_json::to_string(&ptr).map_err(io::Error::other)?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
}

pub fn delete_chain(name: &str) -> io::Result<()> {
    validate_chain_name(name)?;
    let path = chain_path(name);
    std::fs::remove_file(path)
}

pub fn list_chains() -> io::Result<Vec<NamedChain>> {
    let dir = chains_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") {
            let name = match path.file_stem().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let ptr: ChainPointer = match serde_json::from_str(&content) {
                Ok(p) => p,
                Err(_) => continue,
            };
            out.push(NamedChain { name, head: ptr.head });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub fn find_chain_by_head(session_id: &str) -> io::Result<Option<NamedChain>> {
    Ok(list_chains()?.into_iter().find(|c| c.head == session_id))
}

pub fn find_all_chains_by_head(session_id: &str) -> io::Result<Vec<NamedChain>> {
    Ok(list_chains()?.into_iter().filter(|c| c.head == session_id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_tmp_home<F: FnOnce()>(f: F) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("synaps-chain-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let prev_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", &tmp); }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        match prev_home {
            Some(v) => unsafe { std::env::set_var("HOME", v); },
            None => unsafe { std::env::remove_var("HOME"); },
        }
        let _ = std::fs::remove_dir_all(&tmp);
        if let Err(e) = result { std::panic::resume_unwind(e); }
    }

    #[test]
    fn chain_pointer_roundtrip() {
        let p = ChainPointer { head: "abc".into() };
        let s = serde_json::to_string(&p).unwrap();
        let back: ChainPointer = serde_json::from_str(&s).unwrap();
        assert_eq!(back.head, "abc");
    }

    #[test]
    fn invalid_names_rejected() {
        assert!(load_chain("").is_err());
        assert!(load_chain("UPPER").is_err());
        assert!(load_chain("has space").is_err());
        assert!(save_chain("bad name", "id").is_err());
        assert!(delete_chain("").is_err());
    }

    #[test]
    fn save_load_delete_list() {
        with_tmp_home(|| {
            assert!(list_chains().unwrap().is_empty());
            save_chain("work", "session-1").unwrap();
            save_chain("play", "session-2").unwrap();

            let ptr = load_chain("work").unwrap();
            assert_eq!(ptr.head, "session-1");

            let all = list_chains().unwrap();
            assert_eq!(all.len(), 2);
            assert_eq!(all[0].name, "play"); // sorted
            assert_eq!(all[1].name, "work");

            let found = find_chain_by_head("session-2").unwrap();
            assert_eq!(found.unwrap().name, "play");

            save_chain("also-play", "session-2").unwrap();
            let multi = find_all_chains_by_head("session-2").unwrap();
            assert_eq!(multi.len(), 2);

            delete_chain("play").unwrap();
            assert!(load_chain("play").is_err());
        });
    }

    #[test]
    fn overwrite_updates_head() {
        with_tmp_home(|| {
            save_chain("c", "one").unwrap();
            save_chain("c", "two").unwrap();
            assert_eq!(load_chain("c").unwrap().head, "two");
        });
    }
}
