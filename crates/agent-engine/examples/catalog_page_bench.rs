//! First `CatalogPage` cost against a real sessions dir, the way the daemon
//! serves it: the catalog read on ONE thread (`saved::load`), the fold, one
//! page of 50. Read-only on the dir it is given.
//!
//!   cargo run --release -p synaps-engine --example catalog_page_bench -- \
//!       <sessions_dir> <index.jsonl> [preview_cache.json] [--pad=N] [--cold] [--threads=N]
//!
//! `--pad=N`: build a scratch dir under the temp dir holding every file of
//! `sessions_dir` (hard links, else copies) plus header-only clones of its
//! snapshots under fresh ids until it has `pad_to` snapshots, and bench
//! that instead. The scratch dir is removed at the end.
//!
//! `--scratch=DIR`: where the padded copy goes (default: the temp dir).
//! Put it on the sessions dir's filesystem so hard links work and `--cold`
//! reads a disk, not a tmpfs.
//! `--cold` (unix): before each run, drop every file of the dir, the
//! index and the preview cache from the page cache (`posix_fadvise
//! DONTNEED`; no root needed), so each run reads the disk.
//! `--threads=N`: read the headers on `N` threads instead of the
//! daemon's one, for comparison.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent_core::session_catalog::CatalogPaths;
use agent_engine::daemon::saved;

const RUNS: usize = 7;

fn main() {
    let all: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| all.iter().find_map(|a| a.strip_prefix(name)).map(str::to_string);
    let cold = all.iter().any(|a| a == "--cold");
    let threads: usize = flag("--threads=").map_or(1, |t| t.parse().expect("--threads"));
    let pad_to: Option<usize> = flag("--pad=").map(|n| n.parse().expect("--pad"));
    let mut args = all.iter().filter(|a| !a.starts_with("--")).cloned();
    let sessions_dir: PathBuf = args.next().expect("sessions dir").into();
    let index_path: PathBuf = args.next().expect("index.jsonl").into();
    let cache_path: PathBuf = args
        .next()
        .map(Into::into)
        .unwrap_or_else(|| std::env::temp_dir().join("catalog-page-bench-no-cache.json"));

    let scratch_root: PathBuf = flag("--scratch=").map_or_else(std::env::temp_dir, PathBuf::from);
    let scratch = pad_to.map(|n| pad(&sessions_dir, &scratch_root, n));
    let paths = CatalogPaths {
        sessions_dir: scratch.clone().unwrap_or(sessions_dir),
        index_path,
        cache_path,
    };

    let mut load = Vec::new();
    let mut fold = Vec::new();
    let mut first = Vec::new();
    let mut rows = 0;
    let mut sessions = 0;
    for _ in 0..RUNS {
        if cold {
            evict(&paths);
        }
        let t = Instant::now();
        let entries = if threads == 1 {
            saved::load(&paths).expect("load")
        } else {
            let mut cat = agent_core::session_catalog::load_catalog_with(&paths, threads).expect("load");
            cat.apply_previews(&agent_core::session_catalog::PreviewCache::load(&paths.cache_path));
            cat.entries
        };
        let t_load = t.elapsed();
        let facts = saved::fold(&entries);
        let t_fold = t.elapsed();
        let (page, _) = saved::page(&facts, |_| false, None, 50, false);
        first.push(t.elapsed());
        load.push(t_load);
        fold.push(t_fold - t_load);
        rows = page.len();
        sessions = entries.len();
    }
    let (heads, listable) = {
        let facts = saved::fold(&saved::load(&paths).unwrap());
        let listable = facts.iter().filter(|f| saved::saved_row(f, false).is_some()).count();
        (facts.len(), listable)
    };
    println!(
        "dir: {} ({sessions} sessions, {heads} chain heads, {listable} listable){}",
        paths.sessions_dir.display(),
        if scratch.is_some() { " [padded scratch copy]" } else { "" }
    );
    println!(
        "{threads} thread(s), {RUNS} runs, {}",
        if cold { "page cache dropped before each run" } else { "page cache as found (the first run is the coldest)" }
    );
    println!("  load (headers + index + preview cache): {}", stats(&load));
    println!("  fold:                                   {}", stats(&fold));
    println!("  first page ({rows} rows), total:          {}", stats(&first));
    if let Some(dir) = scratch {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Drop the catalog's files from the page cache.
fn evict(paths: &CatalogPaths) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let mut files: Vec<PathBuf> = std::fs::read_dir(&paths.sessions_dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        files.push(paths.index_path.clone());
        files.push(paths.cache_path.clone());
        for f in files {
            if let Ok(file) = std::fs::File::open(&f) {
                // SAFETY: a valid open fd; advisory only.
                unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
            }
        }
    }
    #[cfg(not(unix))]
    let _ = paths;
}

fn stats(v: &[Duration]) -> String {
    let mut s = v.to_vec();
    s.sort();
    format!("first {:?}, min {:?}, median {:?}, max {:?}", v[0], s[0], s[s.len() / 2], s[s.len() - 1])
}

/// A scratch copy of `src` padded to `n` snapshots (see the module doc).
fn pad(src: &Path, root: &Path, n: usize) -> PathBuf {
    let dst = root.join(format!("catalog-page-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dst).unwrap();
    let mut snapshots = Vec::new();
    for e in std::fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        if !e.file_type().unwrap().is_file() {
            continue;
        }
        let to = dst.join(e.file_name());
        if std::fs::hard_link(e.path(), &to).is_err() {
            std::fs::copy(e.path(), &to).unwrap();
        }
        let name = e.file_name().into_string().unwrap_or_default();
        if name.ends_with(".json") && name != "index.json" {
            snapshots.push(e.path());
        }
    }
    snapshots.sort();
    let have = snapshots.len();
    for i in 0..n.saturating_sub(have) {
        let from = &snapshots[i % have.max(1)];
        let text = std::fs::read(from).unwrap();
        let text = String::from_utf8_lossy(&text);
        let Some(cut) = text.find("\"api_messages\"") else { continue };
        let Ok(mut head) = serde_json::from_str::<serde_json::Value>(&format!("{}\"api_messages\":[]}}", &text[..cut])) else {
            continue;
        };
        let id = format!("20200101-{:06}-{:04x}", i, i % 0x10000);
        head["id"] = id.clone().into();
        // A clone is its own conversation: no chain, no journal.
        if let Some(o) = head.as_object_mut() {
            for k in ["compacted_into", "parent_session", "_journal_generation", "api_messages"] {
                o.remove(k);
            }
        }
        // `api_messages` last, as on disk: the header reader stops at it.
        let head = serde_json::to_string(&head).unwrap();
        let file = format!("{},\"api_messages\":[]}}", head.strip_suffix('}').unwrap());
        std::fs::write(dst.join(format!("{id}.json")), file).unwrap();
    }
    let total = std::fs::read_dir(&dst)
        .unwrap()
        .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".json"))
        .count();
    println!("padded: {have} snapshots + {} header-only clones = {total}", total - have);
    dst
}
