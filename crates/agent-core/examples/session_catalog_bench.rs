//! Read-only bench of the Past-sessions catalog against a real sessions dir.
//!
//!   cargo run --release -p synaps-core --example session_catalog_bench -- \
//!       <sessions_dir> <index.jsonl> [deep phrase]
//!
//! Never writes to the sessions dir: the preview cache goes to a temp dir.

use agent_core::session_catalog::*;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let sessions_dir = args.next().expect("sessions dir").into();
    let index_path = args.next().expect("index.jsonl").into();
    let phrase = args.next().unwrap_or_else(|| "petri net".into());
    let tmp = std::env::temp_dir().join(format!("catalog-bench-{}", std::process::id()));
    let paths = CatalogPaths {
        sessions_dir,
        index_path,
        cache_path: tmp.join("previews.json"),
    };
    let clock = Clock::local_now();

    let t = Instant::now();
    let mut cat = load_catalog(&paths).expect("load");
    println!(
        "load: {} entries ({} unreadable) in {:?}",
        cat.entries.len(),
        cat.unreadable,
        t.elapsed()
    );
    let known = cat
        .entries
        .iter()
        .filter(|e| e.message_count.is_some())
        .count();
    let with_cwd = cat.entries.iter().filter(|e| e.cwd.is_some()).count();
    println!("  header message_count known: {known}; index cwd joined: {with_cwd}");
    let v = build_view(
        &cat.entries,
        &Query::default(),
        &FacetSelection::default(),
        false,
        Sort::Recent,
        &clock,
    );
    println!(
        "  noise BEFORE previews (header counts only): {:?}",
        v.hidden
    );

    let no = AtomicBool::new(false);
    if std::env::var_os("BENCH_DEEP_ONLY").is_some() {
        // Cold deep scan: nothing has read the bodies yet.
        let targets: Vec<&CatalogEntry> = cat.entries.iter().collect();
        let sum = deep_search(
            &paths.sessions_dir,
            &targets,
            &phrase,
            &DeepSearchOptions::default(),
            &no,
            &mut |_| {},
        );
        println!("deep cold {phrase:?}: {sum:?}");
        return;
    }
    let mut cache = PreviewCache::load(&paths.cache_path);
    let st = cache.refresh(&paths.sessions_dir, &cat.entries, &no, &mut |_, _| {});
    println!("cache cold build: {st:?}");
    let t = Instant::now();
    cache.save(&paths.cache_path).expect("save");
    let size = std::fs::metadata(&paths.cache_path)
        .map(|m| m.len())
        .unwrap_or(0);
    println!("  save: {:?}, {} KB", t.elapsed(), size / 1024);
    let t = Instant::now();
    let mut cache2 = PreviewCache::load(&paths.cache_path);
    let st2 = cache2.refresh(&paths.sessions_dir, &cat.entries, &no, &mut |_, _| {});
    println!("cache warm (load + refresh): {:?} {st2:?}", t.elapsed());
    let (mut agree, mut differ) = (0, 0);
    for e in &cat.entries {
        if let (Some(h), Some(p)) = (e.message_count, cache2.get_fresh(e)) {
            if h == p.message_count {
                agree += 1
            } else {
                differ += 1
            }
        }
    }
    println!("  header vs scanned count: {agree} agree, {differ} differ");
    cat.apply_previews(&cache2);
    if let Some(e) = cat.entries.iter().max_by_key(|e| e.updated_at) {
        println!(
            "  newest {}: {:?} msgs, first {:?}, last {:?}",
            e.id, e.message_count, e.first_user, e.last_user
        );
    }
    let no_first = cat
        .entries
        .iter()
        .filter(|e| e.first_user.is_none())
        .count();
    println!("  sessions without any user text: {no_first}");

    let t = Instant::now();
    let v = build_view(
        &cat.entries,
        &Query::default(),
        &FacetSelection::default(),
        false,
        Sort::Recent,
        &clock,
    );
    println!(
        "view: {} rows in {:?}; hidden {:?}",
        v.rows.len(),
        t.elapsed(),
        v.hidden
    );
    let all = build_view(
        &cat.entries,
        &Query::default(),
        &FacetSelection::default(),
        true,
        Sort::Recent,
        &clock,
    );
    let sandbox_only = cat
        .entries
        .iter()
        .filter(|e| {
            let n = noise_reasons(e);
            n.sandbox && !n.few_messages && !n.junk_id
        })
        .count();
    println!(
        "  shown with hidden: {}; sandbox-only (not also <3): {sandbox_only}",
        all.rows.len()
    );
    println!("  when: {:?}", v.facets.when);
    println!(
        "  projects ({}): {:?}",
        v.facets.projects.len(),
        v.facets
            .projects
            .iter()
            .take(8)
            .map(|f| (&f.label, f.count))
            .collect::<Vec<_>>()
    );
    println!(
        "  models ({}): {:?}",
        v.facets.models.len(),
        v.facets
            .models
            .iter()
            .take(8)
            .map(|f| (&f.label, f.count))
            .collect::<Vec<_>>()
    );
    println!("  named: {}", v.facets.named);
    let t = Instant::now();
    let q = parse_query("cwd:agent model:opus $>1 header", &clock);
    let v2 = build_view(
        &cat.entries,
        &q,
        &FacetSelection::default(),
        false,
        Sort::Cost,
        &clock,
    );
    println!("query view: {} rows in {:?}", v2.rows.len(), t.elapsed());
    let groups = day_groups(&cat.entries, &v.rows, DateKey::Updated, &clock);
    println!(
        "day groups: {} (first: {:?})",
        groups.len(),
        groups
            .iter()
            .take(3)
            .map(|g| (&g.label, g.len))
            .collect::<Vec<_>>()
    );

    let targets: Vec<&CatalogEntry> = all.rows.iter().map(|&i| &cat.entries[i]).collect();
    for (label, opts) in [
        ("deep (default)", DeepSearchOptions::default()),
        (
            "deep (no cap, +tools)",
            DeepSearchOptions {
                include_tool_results: true,
                max_hits: usize::MAX,
                max_hits_per_session: usize::MAX,
                ..Default::default()
            },
        ),
        (
            "deep (no cap)",
            DeepSearchOptions {
                max_hits: usize::MAX,
                max_hits_per_session: usize::MAX,
                ..Default::default()
            },
        ),
    ] {
        let mut hits = 0;
        let sum = deep_search(
            &paths.sessions_dir,
            &targets,
            &phrase,
            &opts,
            &no,
            &mut |e| {
                if let DeepEvent::Hit(_) = e {
                    hits += 1;
                }
            },
        );
        println!("{label} {phrase:?}: {hits} hits; {sum:?}");
    }
    let mut first = None;
    deep_search(
        &paths.sessions_dir,
        &targets,
        "zzqx-never-there",
        &DeepSearchOptions::default(),
        &no,
        &mut |_| {},
    );
    let sum = deep_search(
        &paths.sessions_dir,
        &targets,
        "zzqx-never-there",
        &DeepSearchOptions::default(),
        &no,
        &mut |_| {},
    );
    println!("deep full scan, no matches (warm): {sum:?}");
    let sum = deep_search(
        &paths.sessions_dir,
        &targets,
        "the",
        &DeepSearchOptions::default(),
        &no,
        &mut |e| {
            if let (DeepEvent::Hit(h), None) = (&e, &first) {
                first = Some(h.clone());
            }
        },
    );
    println!("deep 'the' (cap): {sum:?}\n  first: {first:?}");
    let _ = std::fs::remove_dir_all(&tmp);
}
