//! Phase 5 / Task 33 — staged in-repo lexical memory index (SQLite declined;
//! see docs/decisions/T33-memory-index-no-sqlite.md).
//!
//! Bounds under test: text/tag/project/timestamp retrieval, bounded
//! pagination with cursors, result-proportional memory (resident-hit
//! stats), crash-safe append/update with kill-during-append recovery, and
//! derived-index rebuildability. Benchmarks (1K/10K/100K) are
//! `--ignored`-gated at the bottom.

use agent_core::memory::index::{
    ensure_index_in, index_dir_in, search_index_in, IndexQuery, MAX_SEGMENTS, SEGMENT_MAX_DOCS,
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

// ─── CP-13 fix1: sensitivity, integrity, bounded state ───────────────────────

fn store_with_sensitivity(
    tmp: &TempDir,
    scope: &ProjectScope,
    content: &str,
    sensitivity: MemorySensitivity,
) -> String {
    store_record_in(
        tmp.path(),
        scope,
        NewMemoryRecord {
            content: content.to_string(),
            tags: vec!["classified".into()],
            provenance: MemoryProvenance {
                source: "user".into(),
                session: None,
            },
            sensitivity,
            retention: MemoryRetention::Standard,
        },
    )
    .unwrap()
    .id
    .unwrap()
}

fn raw_index_bytes(tmp: &TempDir, scope: &ProjectScope) -> String {
    let dir = index_dir_in(tmp.path(), scope);
    let mut out = String::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            out.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
        }
    }
    out
}

/// CP-13 fix1 I7: secret bodies contribute NO terms to the persisted
/// index; ordinary terms are content-derived and must live in private
/// files, never claimed content-free.
#[test]
fn secret_bodies_are_never_indexed_into_segment_files() {
    let tmp = TempDir::new().unwrap();
    let scope = scope_for(&tmp, "proj");
    let secret_id = store_with_sensitivity(
        &tmp,
        &scope,
        "SECRETTERM4af1 credential material",
        MemorySensitivity::Secret,
    );
    store_with_sensitivity(
        &tmp,
        &scope,
        "NORMALTERM9be2 ordinary note",
        MemorySensitivity::Normal,
    );
    ensure_index_in(tmp.path(), &scope).unwrap();

    let raw = raw_index_bytes(&tmp, &scope);
    assert!(
        !raw.to_lowercase().contains("secretterm4af1"),
        "secret body terms must never persist in index files"
    );
    assert!(
        raw.to_lowercase().contains("normalterm9be2"),
        "ordinary content-derived terms are indexed (and private-moded)"
    );

    // The secret record stays reachable by metadata (tags/time), not body.
    let mut q = query(&[]);
    q.tag_prefix = Some("classified".into());
    let page = search_index_in(tmp.path(), &scope, &q).unwrap();
    assert!(page.hits.iter().any(|h| h.id == secret_id));
    let page = search_index_in(tmp.path(), &scope, &query(&["secretterm4af1"])).unwrap();
    assert!(
        page.hits.is_empty(),
        "term probes must not find secret bodies"
    );
}

#[cfg(unix)]
#[test]
fn index_artifacts_are_private_moded() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let scope = scope_for(&tmp, "proj");
    store(&tmp, &scope, "some content", &[]);
    ensure_index_in(tmp.path(), &scope).unwrap();
    let dir = index_dir_in(tmp.path(), &scope);
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o700, "index dir is content-derived — 0700");
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            assert_eq!(mode(&path), 0o600, "{} must be 0600", path.display());
        }
    }
}

/// CP-13 fix1 I8: the manifest carries length + checksum per segment;
/// missing, truncated, or same-length-corrupted segments trigger a
/// verified rebuild instead of silently wrong results.
#[test]
fn missing_truncated_or_corrupted_segments_trigger_verified_rebuild() {
    let tmp = TempDir::new().unwrap();
    let scope = scope_for(&tmp, "proj");
    for i in 0..30 {
        store(&tmp, &scope, &format!("integrity record {i}"), &[]);
    }
    ensure_index_in(tmp.path(), &scope).unwrap();
    let dir = index_dir_in(tmp.path(), &scope);
    let segment = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("seg-"))
        })
        .expect("a segment exists");
    let wide = IndexQuery {
        terms: vec!["integrity".into()],
        limit: Some(MAX_SEARCH_LIMIT),
        ..Default::default()
    };

    // Missing segment.
    let original = std::fs::read(&segment).unwrap();
    std::fs::remove_file(&segment).unwrap();
    let page = search_index_in(tmp.path(), &scope, &wide).unwrap();
    assert_eq!(
        page.hits.len(),
        25,
        "verified rebuild after a missing segment"
    );

    // Truncated segment (length mismatch).
    let segment = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("seg-"))
        })
        .unwrap();
    let bytes = std::fs::read(&segment).unwrap();
    std::fs::write(&segment, &bytes[..bytes.len() / 2]).unwrap();
    let page = search_index_in(tmp.path(), &scope, &wide).unwrap();
    assert_eq!(page.hits.len(), 25, "verified rebuild after truncation");

    // Same-length corruption (checksum mismatch).
    let mut corrupted = std::fs::read(&segment).unwrap();
    if corrupted.is_empty() {
        corrupted = original;
    }
    let mid = corrupted.len() / 2;
    corrupted[mid] = corrupted[mid].wrapping_add(1);
    std::fs::write(&segment, &corrupted).unwrap();
    let page = search_index_in(tmp.path(), &scope, &wide).unwrap();
    assert_eq!(page.hits.len(), 25, "verified rebuild after corruption");
}

/// CP-13 fix1 M1: open segment state is bounded — many staging cycles
/// compact into at most MAX_SEGMENTS, and search reports how many
/// segments it actually opened.
#[test]
fn open_segment_state_is_bounded_by_segment_compaction() {
    let tmp = TempDir::new().unwrap();
    let scope = scope_for(&tmp, "proj");
    // 40 staging cycles of 2 docs each → 40 raw segments without
    // compaction.
    for cycle in 0..40 {
        store(&tmp, &scope, &format!("cycle {cycle} first shared"), &[]);
        store(&tmp, &scope, &format!("cycle {cycle} second shared"), &[]);
        ensure_index_in(tmp.path(), &scope).unwrap();
    }
    let status = ensure_index_in(tmp.path(), &scope).unwrap();
    assert!(
        status.segments <= MAX_SEGMENTS,
        "staged segments must compact: {} > {MAX_SEGMENTS}",
        status.segments
    );

    let mut q = query(&["shared"]);
    q.limit = Some(MAX_SEARCH_LIMIT);
    let page = search_index_in(tmp.path(), &scope, &q).unwrap();
    assert_eq!(page.hits.len(), MAX_SEARCH_LIMIT);
    assert!(
        page.stats.segments_open <= MAX_SEGMENTS,
        "search must never hold more than MAX_SEGMENTS open readers"
    );

    // Nothing was lost across compactions.
    let mut seen = std::collections::HashSet::new();
    let mut cursor = None;
    loop {
        let q = IndexQuery {
            terms: vec!["shared".into()],
            limit: Some(MAX_SEARCH_LIMIT),
            cursor: cursor.clone(),
            ..Default::default()
        };
        let page = search_index_in(tmp.path(), &scope, &q).unwrap();
        for hit in &page.hits {
            seen.insert(hit.id.clone());
        }
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(seen.len(), 80, "compaction must preserve every live doc");
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
         query_ms={query_ms} docs_scanned={} max_resident_hits={} segments_open={}",
        page.stats.docs_scanned, page.stats.max_resident_hits, page.stats.segments_open
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
