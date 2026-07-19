//! Phase 5 / Task 32 — project-scoped progressive memory primitives
//! (spec §9.5): stable record IDs, host-resolved project scope, bounded
//! descriptors/snippets, exact fetch, tombstone forget, cross-project
//! fail-closed, and backward-compatible legacy JSONL loading.

use agent_core::memory::store::{
    fetch_exact_in, forget_in, memory_dir_in, query_in, search_project_in, store_record_in,
    MemoryError, MemoryProvenance, MemoryQuery, MemoryRetention, MemorySensitivity,
    NewMemoryRecord, ProjectMemoryQuery, ProjectScope, MAX_SEARCH_LIMIT,
};
use tempfile::TempDir;

fn scope_for(tmp: &TempDir, rel: &str) -> ProjectScope {
    let root = tmp.path().join(rel);
    std::fs::create_dir_all(&root).unwrap();
    ProjectScope::for_root(&root).unwrap()
}

fn new_record(content: &str, tags: &[&str]) -> NewMemoryRecord {
    NewMemoryRecord {
        content: content.to_string(),
        tags: tags.iter().map(|t| t.to_string()).collect(),
        provenance: MemoryProvenance {
            source: "user".into(),
            session: Some("sess-1".into()),
        },
        sensitivity: MemorySensitivity::Normal,
        retention: MemoryRetention::Standard,
    }
}

#[test]
fn stored_records_get_stable_unique_ids_and_round_trip() {
    let tmp = TempDir::new().unwrap();
    let scope = scope_for(&tmp, "proj-a");

    let a = store_record_in(tmp.path(), &scope, new_record("first note", &["alpha"])).unwrap();
    let b = store_record_in(tmp.path(), &scope, new_record("second note", &[])).unwrap();

    let id_a = a.id.clone().expect("stored records carry stable ids");
    let id_b = b.id.clone().expect("stored records carry stable ids");
    assert_ne!(id_a, id_b, "ids must be unique");
    assert!(id_a.starts_with("mem-"), "readable stable prefix: {id_a}");
    assert_eq!(a.project.as_deref(), Some(scope.key()));
    assert_eq!(
        a.provenance.as_ref().unwrap().source,
        "user",
        "provenance persists"
    );

    // Reload from disk: the SAME id comes back (stability across loads).
    let fetched = fetch_exact_in(tmp.path(), &scope, &[&id_a]).unwrap();
    assert_eq!(fetched.len(), 1);
    assert_eq!(fetched[0].id.as_deref(), Some(id_a.as_str()));
    assert_eq!(fetched[0].content, "first note");
    assert_eq!(
        fetched[0].sensitivity,
        Some(MemorySensitivity::Normal),
        "typed sensitivity round-trips"
    );
    assert_eq!(fetched[0].retention, Some(MemoryRetention::Standard));
}

#[test]
fn search_returns_bounded_descriptors_and_snippets_not_bodies() {
    let tmp = TempDir::new().unwrap();
    let scope = scope_for(&tmp, "proj-a");
    let long_body = "sentinel-head ".to_string() + &"body ".repeat(2_000);
    store_record_in(tmp.path(), &scope, new_record(&long_body, &["big"])).unwrap();
    for i in 0..40 {
        store_record_in(tmp.path(), &scope, new_record(&format!("note {i}"), &[])).unwrap();
    }

    let results = search_project_in(tmp.path(), &scope, &ProjectMemoryQuery::default()).unwrap();
    assert!(
        results.len() <= MAX_SEARCH_LIMIT,
        "default search must be bounded ({} > {MAX_SEARCH_LIMIT})",
        results.len()
    );

    let q = ProjectMemoryQuery {
        content_contains: Some("sentinel-head".into()),
        ..Default::default()
    };
    let hits = search_project_in(tmp.path(), &scope, &q).unwrap();
    assert_eq!(hits.len(), 1);
    let descriptor = &hits[0];
    assert!(descriptor.id.starts_with("mem-"));
    assert!(
        descriptor.snippet.len() < long_body.len() / 4,
        "descriptor carries a bounded snippet, never the full body"
    );
    assert!(descriptor.truncated, "snippet truncation is explicit");
    assert_eq!(descriptor.content_bytes, long_body.len());
    assert_eq!(descriptor.project, scope.key());

    // Oversized limits clamp instead of ballooning.
    let greedy = ProjectMemoryQuery {
        limit: Some(10_000),
        ..Default::default()
    };
    let clamped = search_project_in(tmp.path(), &scope, &greedy).unwrap();
    assert!(clamped.len() <= MAX_SEARCH_LIMIT);
}

#[test]
fn cross_project_search_and_fetch_fail_closed() {
    let tmp = TempDir::new().unwrap();
    let scope_a = scope_for(&tmp, "proj-a");
    let scope_b = scope_for(&tmp, "proj-b");
    assert_ne!(scope_a.key(), scope_b.key());

    let secret = store_record_in(
        tmp.path(),
        &scope_a,
        new_record("project-a private detail", &[]),
    )
    .unwrap();
    let secret_id = secret.id.unwrap();

    // Search from project B never sees project A content.
    let q = ProjectMemoryQuery {
        content_contains: Some("private detail".into()),
        ..Default::default()
    };
    let hits = search_project_in(tmp.path(), &scope_b, &q).unwrap();
    assert!(hits.is_empty(), "cross-project search must fail closed");

    // Exact fetch from project B fails closed without existence disclosure.
    let err = fetch_exact_in(tmp.path(), &scope_b, &[&secret_id]).unwrap_err();
    match err {
        MemoryError::NotFound(id) => assert_eq!(id, secret_id),
        other => panic!("expected fail-closed NotFound, got {other:?}"),
    }

    // Forget from project B cannot tombstone project A's record.
    let err = forget_in(tmp.path(), &scope_b, &secret_id).unwrap_err();
    assert!(matches!(err, MemoryError::NotFound(_)));
    let still_there = fetch_exact_in(tmp.path(), &scope_a, &[&secret_id]).unwrap();
    assert_eq!(still_there.len(), 1);
}

#[test]
fn forget_tombstones_and_search_fetch_exclude_the_record() {
    let tmp = TempDir::new().unwrap();
    let scope = scope_for(&tmp, "proj-a");
    let kept = store_record_in(tmp.path(), &scope, new_record("keep me", &[])).unwrap();
    let gone = store_record_in(tmp.path(), &scope, new_record("forget me", &[])).unwrap();
    let gone_id = gone.id.unwrap();

    forget_in(tmp.path(), &scope, &gone_id).unwrap();

    let results = search_project_in(tmp.path(), &scope, &ProjectMemoryQuery::default()).unwrap();
    assert_eq!(
        results.len(),
        1,
        "tombstoned record must vanish from search"
    );
    assert_eq!(results[0].id, kept.id.clone().unwrap());

    let err = fetch_exact_in(tmp.path(), &scope, &[&gone_id]).unwrap_err();
    assert!(matches!(err, MemoryError::NotFound(_)));

    // Forgetting again fails closed (no re-animation, no double tombstone).
    let err = forget_in(tmp.path(), &scope, &gone_id).unwrap_err();
    assert!(matches!(err, MemoryError::NotFound(_)));

    // The tombstone is append-only: the original line still exists on disk
    // (physical deletion belongs to the retention sweep), but no read path
    // returns it.
    let ns_file = memory_dir_in(tmp.path()).join(format!("{}.jsonl", scope.namespace()));
    let raw = std::fs::read_to_string(ns_file).unwrap();
    assert!(raw.contains("forget me"));
    assert!(raw.contains(&gone_id));
}

#[test]
fn secret_sensitivity_round_trips_on_the_record() {
    let tmp = TempDir::new().unwrap();
    let scope = scope_for(&tmp, "proj-a");
    let mut record = new_record("api key hint", &[]);
    record.sensitivity = MemorySensitivity::Secret;
    record.retention = MemoryRetention::MaxAgeDays(30);
    let stored = store_record_in(tmp.path(), &scope, record).unwrap();
    let id = stored.id.unwrap();
    let fetched = fetch_exact_in(tmp.path(), &scope, &[&id]).unwrap();
    assert_eq!(fetched[0].sensitivity, Some(MemorySensitivity::Secret));
    assert_eq!(fetched[0].retention, Some(MemoryRetention::MaxAgeDays(30)));
}

#[test]
fn legacy_jsonl_records_still_load_and_stay_out_of_project_scope() {
    let tmp = TempDir::new().unwrap();
    let scope = scope_for(&tmp, "proj-a");

    // A pre-T32 line: no id, no project, no provenance/sensitivity/retention.
    let dir = memory_dir_in(tmp.path());
    std::fs::create_dir_all(&dir).unwrap();
    let legacy_line = r#"{"namespace":"session-notes","timestamp_ms":123,"content":"legacy body","tags":["@user"]}"#;
    std::fs::write(dir.join("session-notes.jsonl"), format!("{legacy_line}\n")).unwrap();

    // Legacy namespace query API still returns the record unchanged.
    let legacy = query_in(tmp.path(), "session-notes", &MemoryQuery::default()).unwrap();
    assert_eq!(legacy.len(), 1);
    assert_eq!(legacy[0].content, "legacy body");
    assert_eq!(legacy[0].id, None);
    assert_eq!(legacy[0].project, None);

    // Project-scoped search NEVER surfaces project-less legacy records
    // (fail closed), even if the file is copied into a project namespace.
    std::fs::copy(
        dir.join("session-notes.jsonl"),
        dir.join(format!("{}.jsonl", scope.namespace())),
    )
    .unwrap();
    let hits = search_project_in(tmp.path(), &scope, &ProjectMemoryQuery::default()).unwrap();
    assert!(
        hits.is_empty(),
        "project-less legacy records must not leak into project scope"
    );
}

#[test]
fn project_scope_is_canonical_over_path_spellings() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("proj-a");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    let direct = ProjectScope::for_root(&root).unwrap();
    let dotted = ProjectScope::for_root(&root.join("sub").join("..")).unwrap();
    assert_eq!(
        direct.key(),
        dotted.key(),
        "path spellings must canonicalize to one project identity"
    );
    assert!(direct.namespace().starts_with("project-"));
}
