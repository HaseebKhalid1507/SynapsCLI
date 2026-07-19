//! Phase 5 / Task 33 — staged in-repo lexical memory index (SQLite declined;
//! see docs/decisions/T33-memory-index-no-sqlite.md).
//!
//! Bounds under test: text/tag/project/timestamp retrieval, bounded
//! pagination with cursors, result-proportional memory (resident-hit
//! stats), crash-safe append/update with kill-during-append recovery, and
//! derived-index rebuildability. Benchmarks (1K/10K/100K) are
//! `--ignored`-gated at the bottom.

use agent_core::memory::index::{
    ensure_index_in, index_dir_in, search_index_in, IndexQuery, SEGMENT_MAX_DOCS,
};
use agent_core::memory::store::{
    forget_in, memory_dir_in, store_record_in, MemoryProvenance, MemoryRetention,
    MemorySensitivity, NewMemoryRecord, ProjectScope, MAX_SEARCH_LIMIT,
};
use tempfile::TempDir;

fn scope_for(tmp: &TempDir, rel: &str) -> ProjectScope {
    let root = tmp.path().join(rel);
    std::fs::create_dir_all(&root).unwrap();
    ProjectScope::for_root(&root).unwrap()
}

fn store(tmp: &TempDir, scope: &ProjectScope, content: &str, tags: &[&str]) -> String {
    store_record_in(
        tmp.path(),
        scope,
        NewMemoryRecord {
            content: content.to_string(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            provenance: MemoryProvenance {
                source: "user".into(),
                session: None,
            },
            sensitivity: MemorySensitivity::Normal,
            retention: MemoryRetention::Standard,
        },
    )
    .unwrap()
    .id
    .unwrap()
}

fn query(terms: &[&str]) -> IndexQuery {
    IndexQuery {
        terms: terms.iter().map(|t| t.to_string()).collect(),
        ..Default::default()
    }
}

#[test]
fn text_tag_and_timestamp_retrieval_over_staged_segments() {
    let tmp = TempDir::new().unwrap();
    let scope = scope_for(&tmp, "proj");
    let budget_id = store(&tmp, &scope, "the context budget engine is live", &["t29"]);
    store(&tmp, &scope, "compaction transition landed", &["t30"]);
    let both_id = store(
        &tmp,
        &scope,
        "budget and compaction interact",
        &["t29", "t30"],
    );

    // Text: token-exact AND semantics.
    let page = search_index_in(tmp.path(), &scope, &query(&["budget"])).unwrap();
    let ids: Vec<&str> = page.hits.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&budget_id.as_str()) && ids.contains(&both_id.as_str()));

    let page = search_index_in(tmp.path(), &scope, &query(&["budget", "compaction"])).unwrap();
    assert_eq!(page.hits.len(), 1, "AND semantics");
    assert_eq!(page.hits[0].id, both_id);

    // Tag predicate.
    let mut q = query(&[]);
    q.tag_prefix = Some("t30".into());
    let page = search_index_in(tmp.path(), &scope, &q).unwrap();
    assert_eq!(page.hits.len(), 2);

    // Timestamp predicate + newest-first ordering.
    let all = search_index_in(tmp.path(), &scope, &query(&[])).unwrap();
    assert!(all
        .hits
        .windows(2)
        .all(|w| w[0].timestamp_ms >= w[1].timestamp_ms));
    let newest_ts = all.hits[0].timestamp_ms;
    let mut q = query(&[]);
    q.until_ms = Some(newest_ts.saturating_sub(1));
    let page = search_index_in(tmp.path(), &scope, &q).unwrap();
    assert!(page.hits.iter().all(|h| h.timestamp_ms < newest_ts));
}

#[test]
fn pagination_is_bounded_with_stable_cursors() {
    let tmp = TempDir::new().unwrap();
    let scope = scope_for(&tmp, "proj");
    for i in 0..60 {
        store(&tmp, &scope, &format!("note number {i} common"), &[]);
    }

    // Oversized limits clamp.
    let mut q = query(&["common"]);
    q.limit = Some(10_000);
    let page = search_index_in(tmp.path(), &scope, &q).unwrap();
    assert!(page.hits.len() <= MAX_SEARCH_LIMIT);

    // Walk pages to exhaustion: no duplicates, no gaps, bounded pages.
    let mut seen = std::collections::HashSet::new();
    let mut cursor = None;
    let mut pages = 0;
    loop {
        let q = IndexQuery {
            terms: vec!["common".into()],
            limit: Some(7),
            cursor: cursor.clone(),
            ..Default::default()
        };
        let page = search_index_in(tmp.path(), &scope, &q).unwrap();
        assert!(page.hits.len() <= 7);
        for hit in &page.hits {
            assert!(
                seen.insert(hit.id.clone()),
                "duplicate across pages: {}",
                hit.id
            );
        }
        pages += 1;
        assert!(pages < 20, "pagination must terminate");
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(seen.len(), 60, "every record reachable exactly once");
}

#[test]
fn resident_memory_is_proportional_to_limit_not_matches() {
    let tmp = TempDir::new().unwrap();
    let scope = scope_for(&tmp, "proj");
    // Far more than one segment, all matching.
    for i in 0..(SEGMENT_MAX_DOCS * 2 + 50) {
        store(
            &tmp,
            &scope,
            &format!("everything matches shared token {i}"),
            &[],
        );
    }
    let mut q = query(&["shared"]);
    q.limit = Some(5);
    let page = search_index_in(tmp.path(), &scope, &q).unwrap();
    assert_eq!(page.hits.len(), 5);
    assert!(
        page.stats.max_resident_hits <= 5,
        "resident hits {} must be bounded by the limit, not the {}+ matches",
        page.stats.max_resident_hits,
        SEGMENT_MAX_DOCS * 2
    );
    assert!(
        page.stats.docs_scanned < SEGMENT_MAX_DOCS * 2 + 50,
        "ts-desc streaming must early-terminate ({} scanned)",
        page.stats.docs_scanned
    );
}

#[test]
fn forgotten_records_leave_the_index_results() {
    let tmp = TempDir::new().unwrap();
    let scope = scope_for(&tmp, "proj");
    let keep = store(&tmp, &scope, "kept forever", &[]);
    let gone = store(&tmp, &scope, "kept briefly", &[]);
    // Index BEFORE the forget, so the tombstone arrives after staging.
    ensure_index_in(tmp.path(), &scope).unwrap();
    forget_in(tmp.path(), &scope, &gone).unwrap();

    let page = search_index_in(tmp.path(), &scope, &query(&["kept"])).unwrap();
    let ids: Vec<&str> = page.hits.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![keep.as_str()],
        "tombstoned record must not resurface"
    );
}

#[test]
fn kill_during_append_recovers_without_corruption() {
    let tmp = TempDir::new().unwrap();
    let scope = scope_for(&tmp, "proj");
    for i in 0..20 {
        store(&tmp, &scope, &format!("stable record {i}"), &[]);
    }
    ensure_index_in(tmp.path(), &scope).unwrap();

    let idx_dir = index_dir_in(tmp.path(), &scope);

    // Simulated kill #1: a torn store append (partial JSONL tail).
    let store_file = memory_dir_in(tmp.path()).join(format!("{}.jsonl", scope.namespace()));
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&store_file)
            .unwrap();
        f.write_all(b"{\"namespace\":\"trunc").unwrap();
    }
    // Simulated kill #2: an orphaned half-written segment tmp file.
    std::fs::write(idx_dir.join("seg-9999.jsonl.tmp"), b"{\"torn\":").unwrap();

    let wide = |terms: &[&str]| IndexQuery {
        terms: terms.iter().map(|t| t.to_string()).collect(),
        limit: Some(MAX_SEARCH_LIMIT),
        ..Default::default()
    };
    let page = search_index_in(tmp.path(), &scope, &wide(&["stable"])).unwrap();
    assert_eq!(page.hits.len(), 20, "torn tails must not corrupt retrieval");

    // A record stored AFTER the torn tail still becomes retrievable.
    store(&tmp, &scope, "post-crash record stable", &[]);
    let page = search_index_in(tmp.path(), &scope, &wide(&["stable"])).unwrap();
    assert_eq!(page.hits.len(), 21);

    // Simulated kill #3: manifest corrupted mid-rewrite → full derived
    // rebuild, identical results.
    std::fs::write(idx_dir.join("manifest.json"), b"{corrupt").unwrap();
    let page = search_index_in(tmp.path(), &scope, &wide(&["stable"])).unwrap();
    assert_eq!(
        page.hits.len(),
        21,
        "invalid manifest must trigger a clean rebuild"
    );
}

#[test]
fn index_is_derived_state_and_scoped_per_project() {
    let tmp = TempDir::new().unwrap();
    let scope_a = scope_for(&tmp, "proj-a");
    let scope_b = scope_for(&tmp, "proj-b");
    store(&tmp, &scope_a, "alpha only fact", &[]);
    store(&tmp, &scope_b, "beta only fact", &[]);

    let page = search_index_in(tmp.path(), &scope_a, &query(&["fact"])).unwrap();
    assert_eq!(page.hits.len(), 1, "index results are project-scoped");

    // Deleting the whole index directory loses nothing: derived state.
    std::fs::remove_dir_all(index_dir_in(tmp.path(), &scope_a)).unwrap();
    let page = search_index_in(tmp.path(), &scope_a, &query(&["fact"])).unwrap();
    assert_eq!(page.hits.len(), 1, "index rebuilds from the store");
}

// ─── benchmarks (resource-capped, --ignored) ─────────────────────────────────
//
// cargo test -p synaps-core --test memory_index -- --ignored --test-threads=1

fn bench_corpus(n: usize) {
    let tmp = TempDir::new().unwrap();
    let scope = scope_for(&tmp, "bench");
    let t_build = std::time::Instant::now();
    for i in 0..n {
        store(
            &tmp,
            &scope,
            &format!(
                "record {i} lorem ipsum budget engine compaction token{}",
                i % 97
            ),
            &[if i % 2 == 0 { "even" } else { "odd" }],
        );
    }
    let store_ms = t_build.elapsed().as_millis();

    let t_index = std::time::Instant::now();
    ensure_index_in(tmp.path(), &scope).unwrap();
    let index_ms = t_index.elapsed().as_millis();

    let t_query = std::time::Instant::now();
    let mut q = query(&["budget"]);
    q.limit = Some(10);
    let page = search_index_in(tmp.path(), &scope, &q).unwrap();
    let query_ms = t_query.elapsed().as_millis();

    assert_eq!(page.hits.len(), 10);
    assert!(page.stats.max_resident_hits <= 10);
    println!(
        "BENCH memory_index n={n} store_ms={store_ms} index_ms={index_ms} \
         query_ms={query_ms} docs_scanned={} max_resident_hits={}",
        page.stats.docs_scanned, page.stats.max_resident_hits
    );
}

#[test]
#[ignore = "benchmark — run explicitly, resource-capped"]
fn bench_1k_records() {
    bench_corpus(1_000);
}

#[test]
#[ignore = "benchmark — run explicitly, resource-capped"]
fn bench_10k_records() {
    bench_corpus(10_000);
}

#[test]
#[ignore = "benchmark — run explicitly, resource-capped"]
fn bench_100k_records() {
    bench_corpus(100_000);
}

#[test]
#[ignore = "benchmark — run explicitly, resource-capped"]
fn bench_1m_records() {
    bench_corpus(1_000_000);
}
