//! Past-tab background work. Everything here runs on `spawn_blocking` and
//! talks back through `SessionsMsg::Past` on the modal's channel; nothing
//! blocks the draw loop.

use super::{PastCmd, PastMsg};
use crate::tui::sessions::SessionsMsg;
use agent_core::session_catalog::{
    deep_search, load_catalog, rename_past_session, Catalog, CatalogEntry, CatalogPaths, DeepEvent,
    DeepSearchOptions, PreviewCache,
};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;

type Tx = UnboundedSender<SessionsMsg>;

/// The last catalog this process loaded: a reopen paints it at once while
/// the fresh load runs (headers ~10–50 ms, previews only for changed files).
static LAST: Mutex<Option<Arc<Catalog>>> = Mutex::new(None);

pub(crate) fn last_catalog() -> Option<Arc<Catalog>> {
    LAST.lock().ok().and_then(|g| g.clone())
}

fn remember(cat: &Arc<Catalog>) {
    if let Ok(mut g) = LAST.lock() {
        *g = Some(cat.clone());
    }
}

/// Send at most this often during a preview refresh.
const PROGRESS_EVERY: usize = 25;

/// Load the catalog for `paths`, send it, then refresh the preview cache
/// (only sessions whose files changed), send it again, and save the cache.
/// `stop` (modal closed) ends the refresh early; finished work is still saved.
pub(crate) fn spawn_load(gen: u64, tx: Tx, paths: CatalogPaths, stop: Arc<AtomicBool>) {
    tokio::task::spawn_blocking(move || {
        let send = |msg: PastMsg| tx.send(SessionsMsg::Past { gen, msg }).is_ok();
        let mut cat = match load_catalog(&paths) {
            Ok(c) => c,
            Err(e) => {
                send(PastMsg::Catalog(Err(format!("could not list saved sessions: {e}"))));
                return;
            }
        };
        let mut cache = PreviewCache::load(&paths.cache_path);
        cat.apply_previews(&cache);
        let first = Arc::new(cat.clone());
        remember(&first);
        if !send(PastMsg::Catalog(Ok(first))) {
            return;
        }
        if cache.stale_count(&cat.entries) == 0 {
            return;
        }
        let stats = cache.refresh(&paths.sessions_dir, &cat.entries, &stop, &mut |done, total| {
            if done % PROGRESS_EVERY == 0 || done == total {
                send(PastMsg::PreviewProgress { done, total });
            }
        });
        cat.apply_previews(&cache);
        let full = Arc::new(cat);
        remember(&full);
        send(PastMsg::PreviewProgress { done: 1, total: 1 });
        send(PastMsg::Previews(full));
        if stats.scanned > 0 {
            if let Err(e) = cache.save(&paths.cache_path) {
                tracing::warn!("session preview cache not saved: {e}");
            }
        }
    });
}

/// Run a Past command (deep search / rename) on a blocking task.
pub(crate) fn run(app: &mut crate::tui::app::App, cmd: PastCmd) {
    if tokio::runtime::Handle::try_current().is_err() {
        return; // headless harness: results are fed by the test
    }
    let Some(st) = app.sessions.as_ref() else { return };
    let gen = st.gen;
    let tx = app.sessions_tx.clone();
    let Some(dir) = st.past.paths.as_ref().map(|p| p.sessions_dir.clone()) else { return };
    match cmd {
        PastCmd::Deep { search } => {
            let p = &st.past;
            let Some(d) = p.deep.as_ref().filter(|d| d.search == search && d.running) else {
                return;
            };
            let entries = p.entries.clone();
            let rows = p.deep_targets();
            let phrase = d.phrase.clone();
            // Set by esc, an edit, a restart, or the modal closing.
            let cancel = d.cancel.clone();
            tokio::task::spawn_blocking(move || {
                let targets: Vec<&CatalogEntry> = rows.iter().map(|&i| &entries[i]).collect();
                let mut last_sent = 0usize;
                let summary = deep_search(&dir, &targets, &phrase, &DeepSearchOptions::default(), &cancel, &mut |ev| {
                    let send = match &ev {
                        DeepEvent::Hit(_) => true,
                        DeepEvent::Progress { done, total } => {
                            let due = done - last_sent >= PROGRESS_EVERY || done == total;
                            if due {
                                last_sent = *done;
                            }
                            due
                        }
                    };
                    if send {
                        let _ = tx.send(SessionsMsg::Past { gen, msg: PastMsg::Deep { search, ev } });
                    }
                });
                let _ = tx.send(SessionsMsg::Past { gen, msg: PastMsg::DeepDone { search, summary } });
            });
        }
        PastCmd::Rename { id, name } => {
            tokio::task::spawn_blocking(move || {
                let result = rename_past_session(&dir, &id, name.as_deref()).map_err(|e| e.to_string());
                let _ = tx.send(SessionsMsg::Past { gen, msg: PastMsg::Renamed { id, name, result } });
            });
        }
    }
}
