//! A chat's memory binding: the project its notes and context archives
//! belong to, resolved once from the chat's folder and carried across
//! threads, so that an ambient `cwd` cannot redirect a write. There is one
//! store, plain files under the base directory; the blocking file calls run
//! on a blocking worker. The binding also carries the run's author, which
//! `tool.call` forwards to extensions as `caller`.
use crate::{Result, RuntimeError};
use agent_core::memory::author::Author;
use agent_core::memory::store::{
    self, MemoryDescriptor, MemoryRecord, NewMemoryRecord, ProjectMemoryQuery, ProjectScope,
};
use std::path::{Path, PathBuf};
mod history;

#[derive(Clone, Debug)]
pub struct MemoryBinding {
    base: PathBuf,
    scope: std::result::Result<ProjectScope, String>,
    author: Author,
}
fn error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::Tool(format!("memory: {}", message.into()))
}

/// What `memory_store` (`what` = "note") and `memory_forget` ("forget") say
/// for an error of the plain-file store. When the line was written and only
/// its sync failed, the model must not take it for a failed call: a second
/// `memory_store` makes a second note, and a second `memory_forget` answers
/// "not found".
fn legacy_write_error(e: store::MemoryError, what: &str) -> RuntimeError {
    let store::MemoryError::WrittenUnconfirmed { id, cause } = &e else {
        return error(e.to_string());
    };
    match (id, what) {
        (Some(id), "forget") => error(format!(
            "the forget of {id} was written but the disk did not confirm it ({cause}). The note \
             is forgotten now and searches no longer return it. Do not call memory_forget for it \
             again (it would answer not found); to check, fetch {id} with memory_fetch: not found \
             means it is forgotten."
        )),
        (None, "forget") => error(format!(
            "the forget was written but the disk did not confirm it ({cause}). The note is \
             forgotten now and searches no longer return it. Do not call memory_forget for it \
             again (it would answer not found); use memory_search if you need to check."
        )),
        (Some(id), _) => error(format!(
            "the note was written as {id} but the disk did not confirm it ({cause}). It is in \
             memory now. Do NOT save it again: fetch {id} with memory_fetch to check, and call \
             memory_store again only if the fetch does not return it."
        )),
        (None, _) => error(format!(
            "the note was written but the disk did not confirm it ({cause}). It is in memory \
             now. Do NOT save it again: use memory_search to find it first, and call \
             memory_store again only if the search does not return it."
        )),
    }
}

impl MemoryBinding {
    /// The binding of a chat that runs in `cwd`: the base directory, and the
    /// project of that folder. Without a folder, or with one that has no
    /// project, the binding has none and every call through it is refused;
    /// no project is guessed from the process folder. (Under the daemon a
    /// chat's folder differs from the process folder.)
    pub fn for_cwd(cwd: Option<PathBuf>) -> Self {
        let scope = cwd
            .ok_or_else(|| "workspace unavailable".to_owned())
            .and_then(|cwd| {
                ProjectScope::discover(&cwd).map_err(|_| "project scope unavailable".to_owned())
            });
        Self::new(agent_core::config::base_dir(), scope)
    }
    /// Tests only: the binding of a chat that runs in the process folder,
    /// which is what a runtime holds until it is bound.
    #[cfg(test)]
    pub(crate) fn for_process_cwd() -> Self {
        Self::for_cwd(std::env::current_dir().ok())
    }
    fn new(base: PathBuf, scope: std::result::Result<ProjectScope, String>) -> Self {
        Self {
            base,
            scope,
            author: Author::fresh(),
        }
    }
    /// Immutable execution provenance, not a saved conversation identity or
    /// an authorization grant. Ordinary clones retain this exact author.
    /// Forwarded to extensions as `caller` on `tool.call`; an extension's
    /// forum records it as the run's author.
    pub fn author(&self) -> &Author {
        &self.author
    }
    /// New worker execution: fork only the author, retaining the captured
    /// base and project (including a scope error). Resumed workers get a
    /// new actor too; nothing is rediscovered.
    pub fn for_worker(&self) -> Self {
        Self {
            author: self.author.child(),
            ..self.clone()
        }
    }
    pub fn scope(&self) -> Result<&ProjectScope> {
        self.scope
            .as_ref()
            .map_err(|reason| error(format!("host project scope unavailable: {reason}")))
    }
    pub fn base(&self) -> &Path {
        &self.base
    }
    pub async fn search(&self, q: ProjectMemoryQuery) -> Result<Vec<MemoryDescriptor>> {
        let scope = self.scope()?.clone();
        let base = self.base.clone();
        tokio::task::spawn_blocking(move || store::search_project_in(&base, &scope, &q))
            .await
            .map_err(|_| error("legacy search worker failed"))?
            .map_err(|e| error(e.to_string()))
    }
    pub async fn fetch(&self, ids: &[&str]) -> Result<Vec<MemoryRecord>> {
        let scope = self.scope()?.clone();
        if ids.is_empty() || ids.len() > 25 || ids.iter().any(|id| id.is_empty() || id.len() > 128)
        {
            return Err(error("fetch requires 1..=25 bounded exact IDs"));
        }
        let base = self.base.clone();
        let ids = ids.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        tokio::task::spawn_blocking(move || {
            store::fetch_exact_in(
                &base,
                &scope,
                &ids.iter().map(String::as_str).collect::<Vec<_>>(),
            )
        })
        .await
        .map_err(|_| error("legacy fetch worker failed"))?
        .map_err(|e| error(e.to_string()))
    }
    pub async fn store(&self, new: NewMemoryRecord) -> Result<MemoryRecord> {
        let scope = self.scope()?.clone();
        if new.content.len() > store::MAX_CONTENT_BYTES {
            return Err(error("content exceeds 16384 bytes"));
        }
        let base = self.base.clone();
        tokio::task::spawn_blocking(move || store::store_record_in(&base, &scope, new))
            .await
            .map_err(|_| error("legacy store worker failed"))?
            .map_err(|e| legacy_write_error(e, "note"))
    }
    pub async fn forget(&self, id: &str) -> Result<()> {
        let scope = self.scope()?.clone();
        if id.is_empty() || id.len() > 128 {
            return Err(error("invalid exact ID"));
        }
        let base = self.base.clone();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || store::forget_in(&base, &scope, &id))
            .await
            .map_err(|_| error("legacy forget worker failed"))?
            .map_err(|e| legacy_write_error(e, "forget"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_that_was_not_confirmed_tells_the_model_to_search_before_saving_again() {
        let unconfirmed = |id: Option<&str>| store::MemoryError::WrittenUnconfirmed {
            id: id.map(str::to_string),
            cause: "EIO".into(),
        };
        let stored = legacy_write_error(unconfirmed(Some("mem-abc")), "note").to_string();
        assert!(stored.contains("memory: the note was written as mem-abc"), "{stored}");
        assert!(stored.contains("EIO"), "{stored}");
        assert!(
            stored.contains("Do NOT save it again: fetch mem-abc with memory_fetch to check"),
            "{stored}"
        );
        let forgotten = legacy_write_error(unconfirmed(Some("mem-abc")), "forget").to_string();
        assert!(forgotten.contains("memory: the forget of mem-abc was written"), "{forgotten}");
        assert!(forgotten.contains("Do not call memory_forget for it again"), "{forgotten}");
        assert!(forgotten.contains("fetch mem-abc with memory_fetch"), "{forgotten}");
        // Without an id there is nothing exact to fetch: search.
        let stored = legacy_write_error(unconfirmed(None), "note").to_string();
        assert!(stored.contains("memory: the note was written but"), "{stored}");
        assert!(stored.contains("Do NOT save it again: use memory_search"), "{stored}");
        let forgotten = legacy_write_error(unconfirmed(None), "forget").to_string();
        assert!(forgotten.contains("memory: the forget was written"), "{forgotten}");
        // Any other store error is passed on as it was.
        let other = legacy_write_error(store::MemoryError::NotFound("mem-1".into()), "forget");
        assert_eq!(
            other.to_string(),
            error(store::MemoryError::NotFound("mem-1".into()).to_string()).to_string()
        );
    }

    /// A binding made with no folder, or with a deleted one, has no project
    /// and names why; every note call through it is refused before a file
    /// is touched.
    #[tokio::test]
    async fn a_binding_without_a_folder_refuses_every_call() {
        let gone = tempfile::tempdir().unwrap();
        let gone_path = gone.path().join("deleted");
        for (cwd, why) in [
            (None, "workspace unavailable"),
            (Some(gone_path), "project scope unavailable"),
        ] {
            let binding = MemoryBinding::for_cwd(cwd);
            let refused = binding.scope().unwrap_err().to_string();
            assert_eq!(
                refused,
                error(format!("host project scope unavailable: {why}")).to_string()
            );
            assert!(refused.ends_with(&format!("memory: host project scope unavailable: {why}")));
            assert!(binding.search(Default::default()).await.is_err());
            assert!(binding.fetch(&["mem-unknown"]).await.is_err());
            assert!(binding.forget("mem-unknown").await.is_err());
        }
        assert_eq!(std::fs::read_dir(gone.path()).unwrap().count(), 0);
    }
}

#[cfg(test)]
mod worker_tests {
    use super::*;

    fn assert_same_storage(parent: &MemoryBinding, child: &MemoryBinding) {
        assert_eq!(child.base, parent.base);
        assert_eq!(child.scope, parent.scope);
    }

    #[test]
    fn author_is_fresh_but_clones_keep_identity() {
        let dir = tempfile::tempdir().unwrap();
        let scope = ProjectScope::for_root(dir.path()).unwrap();
        let parent = MemoryBinding::new(dir.path().into(), Ok(scope.clone()));
        let fresh = MemoryBinding::new(dir.path().into(), Ok(scope));
        parent.author().validate().unwrap();
        assert!(parent.author().parent.is_none());
        assert_ne!(parent.author().actor, fresh.author().actor);
        assert_ne!(parent.author().group, fresh.author().group);
        let clone = parent.clone();
        assert_eq!(clone.author(), parent.author());
        assert_same_storage(&parent, &clone);
        // A worker's clones keep the worker's author, not the parent's.
        let worker = parent.for_worker();
        assert_eq!(worker.clone().author(), worker.author());
        assert_ne!(worker.clone().author(), parent.author());
    }

    #[test]
    fn worker_forks_only_author_even_with_bad_scope() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let scope = ProjectScope::for_root(&root).unwrap();
        for bad_scope in [false, true] {
            let selected_scope = if bad_scope {
                Err("captured project discovery failure".into())
            } else {
                Ok(scope.clone())
            };
            let parent = MemoryBinding::new(root.clone(), selected_scope);
            let first = parent.for_worker();
            let second = parent.for_worker();
            for worker in [&first, &second] {
                assert_same_storage(&parent, worker);
                assert_eq!(worker.scope().is_err(), bad_scope);
                worker.author().validate().unwrap();
                assert_ne!(worker.author().actor, parent.author().actor);
                assert_eq!(worker.author().group, parent.author().group);
                assert_eq!(
                    worker.author().parent.as_deref(),
                    Some(parent.author().actor.as_str())
                );
                assert_eq!(worker.clone().author(), worker.author());
            }
            assert_ne!(first.author().actor, second.author().actor);
            let grandchild = first.for_worker();
            assert_same_storage(&first, &grandchild);
            assert_eq!(grandchild.author().group, parent.author().group);
            assert_eq!(
                grandchild.author().parent.as_deref(),
                Some(first.author().actor.as_str())
            );
            assert!(parent.author().parent.is_none());
        }
        assert!(!root.join("memory").exists());
    }
}
