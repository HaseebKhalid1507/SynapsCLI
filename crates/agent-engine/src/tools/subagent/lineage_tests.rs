//! Slice 3 obligations (A*): lineage validation against the store, owned
//! history retrieval, Axel capability negotiation and record-file paths.
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::*;
use crate::memory_backend::MemoryBinding;
use crate::runtime::continuation::MARKER;
use crate::tools::subagent::record::{
    Candidate, FinalRecord, HistoryDigest, Resolution, WriterState,
};
use crate::tools::subagent::record_file::{self, RecordFileError, MAX_RECORD_FILE_BYTES};
use crate::tools::subagent::retrieval::owned_history_tools;
use crate::tools::subagent::test_fixtures::*;
use agent_core::config::{MemoryBackendConfig, MemoryBackendKind};
use agent_core::context_archive::logical_digest;
use agent_core::memory::store::ProjectScope;

fn axel_id(n: u8) -> ArchiveId {
    ArchiveId::parse(&format!("{n:02x}").repeat(16)).unwrap()
}

fn entry(id: &ArchiveId, owner: &str, live: bool) -> Value {
    json!({"id": id.as_str(), "logical_id": logical_digest(owner).unwrap(), "live": live})
}

fn row(id: &ArchiveId, snippet: &str) -> Value {
    json!({"id": id.as_str(), "message_count": 1, "source_message_count": 1, "snippet": snippet})
}

fn fetched(text: &str) -> Value {
    json!([{"source_index": 0, "block_indices": [],
            "message": {"role": "assistant", "content": text}}])
}

/// Own window 0x11 of run 1; sibling 0x22 (run 2); parent session 0x33.
fn axel_state(capabilities: Value) -> Value {
    let (own, sibling, parent) = (axel_id(0x11), axel_id(0x22), axel_id(0x33));
    json!({
        "capabilities": capabilities,
        "entries": [
            entry(&own, &worker_logical_id(&run(1)), true),
            entry(&sibling, &worker_logical_id(&run(2)), true),
            entry(&parent, "parent-session", true)
        ],
        "rows": [
            row(&parent, "NEEDLE parent"),
            row(&sibling, "NEEDLE sibling"),
            row(&own, "NEEDLE own")
        ],
        "fetch": {
            own.as_str(): fetched("NEEDLE own body"),
            sibling.as_str(): fetched("NEEDLE sibling body"),
            parent.as_str(): fetched("NEEDLE parent body")
        }
    })
}

// ── Binding and scope digest ────────────────────────────────────────────────

#[tokio::test]
async fn a11_binding_must_match_current_backend_and_scope() {
    use sha2::{Digest as _, Sha256};
    let store = legacy();
    let key = store.binding.scope().unwrap().key().to_owned();
    let mut hasher = Sha256::new();
    hasher.update(b"synaps.worker-binding.scope.v1\0");
    hasher.update(key.as_bytes());
    let expected: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(scope_digest(&key), format!("sha256:{expected}"));
    assert_ne!(scope_digest(&key), scope_digest("p0123456789abcdef"));
    let mine = current(&store.binding);
    assert_eq!(mine.backend, BackendKind::Legacy);
    assert_eq!(mine.scope_digest, scope_digest(&key));

    let a = run(1);
    let window = seal_run(&store.binding, &a, "NEEDLE own").await;
    // Same store, another repository scope.
    let other_repo = legacy_binding(&store.base, "project-b");
    let rec = record(
        &a,
        current(&other_repo),
        vec![],
        std::slice::from_ref(&window),
        None,
    );
    assert_eq!(
        refusal(&rec, &store.binding).await,
        Refusal::BindingMismatch
    );
    // Same scope digest, other backend kind.
    let mut axel = mine.clone();
    axel.backend = BackendKind::Axel;
    let rec = record(&a, axel, vec![], std::slice::from_ref(&window), None);
    assert_eq!(
        refusal(&rec, &store.binding).await,
        Refusal::BindingMismatch
    );
    // An unavailable backend has no binding to compare against.
    let unavailable = MemoryBinding::for_test(
        store.base.clone(),
        ProjectScope::for_root(&store.base).unwrap(),
        &MemoryBackendConfig {
            kind: MemoryBackendKind::Unavailable,
            executable: None,
            brain: None,
            user_scope: false,
        },
    );
    assert_eq!(
        Binding::of_memory(&unavailable),
        Err(Refusal::BackendUnavailable)
    );
    let rec = record(&a, mine, vec![], &[window], None);
    assert_eq!(
        refusal(&rec, &unavailable).await,
        Refusal::BackendUnavailable
    );
}

// ── Successor lineage ───────────────────────────────────────────────────────

#[tokio::test]
async fn a9_successor_lineage_a_b_c_with_zero_rollover_middle() {
    let store = legacy();
    let bind = current(&store.binding);
    let (a, b, c) = (run(1), run(2), run(3));
    let a1 = seal_run(&store.binding, &a, "A window one").await;
    let a2 = seal_run(&store.binding, &a, "A window two").await;
    let rec_a = record(&a, bind.clone(), vec![], &[a1.clone(), a2.clone()], None);
    rec_a.validate(RecordKind::Final).unwrap();

    // B resumes A's head checkpoint and never rolls over.
    let mut head_a = rec_a.clone();
    head_a.kind = RecordKind::Head;
    let from_a = successor_lineage(&head_a, RecordKind::Head).unwrap();
    assert_eq!(
        from_a.predecessor,
        Predecessor {
            run_id: a.clone(),
            kind: RecordKind::Head,
            generation: rec_a.generation,
            digest: rec_a.history_digest.clone(),
        }
    );
    assert_eq!(
        from_a.inherited_windows,
        inherited(&a, &[a1.clone(), a2.clone()], 1)
    );
    assert_eq!(from_a.window_ordinal, 3);
    let mut rec_b = rec_a.clone();
    rec_b.run_id = b.clone();
    rec_b.predecessor = Some(from_a.predecessor.clone());
    rec_b.inherited_windows = from_a.inherited_windows.clone();
    rec_b.windows = vec![];
    rec_b.rollovers_this_run = 0;
    rec_b.window_ordinal = from_a.window_ordinal;
    rec_b.generation = 0;
    rec_b.validate(RecordKind::Final).unwrap();

    // C resumes B's final: inherited first (A's block), ordinals unchanged,
    // then C rolls over once.
    let from_b = successor_lineage(&rec_b, RecordKind::Final).unwrap();
    assert_eq!(from_b.predecessor.run_id, b);
    assert_eq!(from_b.predecessor.kind, RecordKind::Final);
    assert_eq!(
        from_b.inherited_windows,
        inherited(&a, &[a1.clone(), a2.clone()], 1)
    );
    let c1 = seal_run(&store.binding, &c, "C window three").await;
    let rec_c = record(
        &c,
        bind.clone(),
        from_b.inherited_windows.clone(),
        std::slice::from_ref(&c1),
        Some(from_b.predecessor.clone()),
    );
    assert_eq!(rec_c.windows[0].window, 3);
    rec_c.validate(RecordKind::Final).unwrap();

    // A resumed C keeps A's block before C's own block.
    let from_c = successor_lineage(&rec_c, RecordKind::Final).unwrap();
    let mut expected = inherited(&a, &[a1.clone(), a2.clone()], 1);
    expected.extend(inherited(&c, std::slice::from_ref(&c1), 3));
    assert_eq!(from_c.inherited_windows, expected);
    assert_eq!(from_c.window_ordinal, 4);

    let g = grant(&rec_c, &store.binding).await;
    let mut allow = vec![a1.as_str(), a2.as_str(), c1.as_str()];
    allow.sort_unstable();
    assert_eq!(g.allow(), allow.as_slice());
    assert_eq!(g.run_id(), &c);

    // An invalid predecessor yields no lineage.
    let mut broken = rec_c.clone();
    broken
        .history
        .push(json!({"role": "user", "content": "tampered"}));
    assert!(successor_lineage(&broken, RecordKind::Final).is_err());
    assert!(successor_lineage(&rec_c, RecordKind::Head).is_err());
}

// ── Admission ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn a4_full_lineage_admitted_with_rollover_off() {
    // Admission has no rollover-mode input: a shortened successor gets its
    // whole lineage whether or not it may roll over again.
    let store = legacy();
    let bind = current(&store.binding);
    let (a, c) = (run(1), run(3));
    let a1 = seal_run(&store.binding, &a, "NEEDLE alpha one").await;
    let a2 = seal_run(&store.binding, &a, "NEEDLE alpha two").await;
    let c1 = seal_run(&store.binding, &c, "NEEDLE gamma").await;
    let pred = record(&a, bind.clone(), vec![], &[a1.clone(), a2.clone()], None);
    let lineage = successor_lineage(&pred, RecordKind::Final).unwrap();
    let rec = record(
        &c,
        bind,
        lineage.inherited_windows,
        std::slice::from_ref(&c1),
        Some(lineage.predecessor),
    );
    let g = grant(&rec, &store.binding).await;
    let out = search(&g, json!({"source": "history", "query": "needle"}))
        .await
        .unwrap();
    for id in [&a1, &a2, &c1] {
        assert!(out.contains(&ctx(id)), "{out}");
    }
    let body = fetch(&g, json!({"ids": [ctx(&a1)]})).await.unwrap();
    assert!(body.contains("NEEDLE alpha one"), "{body}");

    // A successor that inherited windows but has none of its own is still
    // shortened and still admitted.
    let lineage = successor_lineage(&rec, RecordKind::Final).unwrap();
    let d = record(
        &run(4),
        current(&store.binding),
        lineage.inherited_windows,
        &[],
        Some(lineage.predecessor),
    );
    assert_eq!(grant(&d, &store.binding).await.allow().len(), 3);

    // Nothing sealed: full history, no grant needed.
    let plain = record(&run(5), current(&store.binding), vec![], &[], None);
    assert!(matches!(
        admit_record(&plain, &store.binding).await,
        Ok(ResumeRetrieval::FullHistory)
    ));
}

#[tokio::test]
async fn a5_foreign_and_tombstoned_refs_refuse_absent_refs_in_legacy_are_not_held() {
    let store = legacy();
    let bind = current(&store.binding);
    let (a, b, c) = (run(1), run(2), run(3));
    let own = seal_run(&store.binding, &a, "own").await;
    let sibling = seal_run(&store.binding, &b, "sibling").await;
    let session = seal(&store.binding, "parent-session", "parent").await;

    // A sibling's or the parent's window listed as this run's own.
    for foreign in [&sibling, &session] {
        let rec = record(
            &a,
            bind.clone(),
            vec![],
            &[own.clone(), foreign.clone()],
            None,
        );
        assert_eq!(
            refusal(&rec, &store.binding).await,
            Refusal::Foreign(foreign.clone())
        );
    }
    // An inherited window attributed to the wrong run of the chain.
    let rec = record(
        &c,
        bind.clone(),
        inherited(&a, &[own.clone(), sibling.clone()], 1),
        &[],
        Some(Predecessor {
            run_id: a.clone(),
            kind: RecordKind::Final,
            generation: 1,
            digest: HistoryDigest::of(&[]).unwrap(),
        }),
    );
    assert_eq!(
        refusal(&rec, &store.binding).await,
        Refusal::Foreign(sibling.clone())
    );
    // Absent from this store's answer (never sealed here, or another
    // repository's store): the legacy store never evicts, so an absent
    // window was never here. Refused as not held, counts only, and nothing
    // of the other store becomes retrievable.
    let elsewhere = seal_run(&legacy_binding(&store.base, "project-b"), &a, "elsewhere").await;
    for absent in [axel_id(0x5a), elsewhere] {
        let rec = record(
            &a,
            bind.clone(),
            vec![],
            &[own.clone(), absent.clone()],
            None,
        );
        let refused = refusal(&rec, &store.binding).await;
        assert_eq!(refused, Refusal::NotHeld { absent: 1, listed: 2 });
        let text = refused.to_string();
        assert!(!text.contains(absent.as_str()) && !text.contains(own.as_str()), "{text}");
    }
    // Forgotten: refused, never re-sealed.
    let gone = seal_run(&store.binding, &a, "forgotten").await;
    store.binding.history_forget(gone.as_str()).await.unwrap();
    let rec = record(&a, bind.clone(), vec![], &[own.clone(), gone.clone()], None);
    assert_eq!(
        refusal(&rec, &store.binding).await,
        Refusal::Tombstoned(gone.clone())
    );
    assert!(store
        .binding
        .history_inspect(
            &store
                .binding
                .owned_history_support()
                .await
                .unwrap()
                .unwrap(),
            &[gone.as_str().to_owned()]
        )
        .await
        .unwrap()
        .entries
        .iter()
        .all(|e| !e.live));
    // The valid record alone is admitted.
    let rec = record(&a, bind, vec![], std::slice::from_ref(&own), None);
    assert_eq!(grant(&rec, &store.binding).await.allow(), [own.as_str()]);
}

#[tokio::test]
async fn a10_marker_must_match_the_latest_window() {
    let store = legacy();
    let bind = current(&store.binding);
    let a = run(1);
    let w1 = seal_run(&store.binding, &a, "one").await;
    let w2 = seal_run(&store.binding, &a, "two").await;
    let base = record(&a, bind.clone(), vec![], &[w1.clone(), w2.clone()], None);
    grant(&base, &store.binding).await;
    let tail = json!({"role": "assistant", "content": "working"});
    let cases = vec![
        // No marker although windows were sealed.
        vec![json!({"role": "user", "content": "task"}), tail.clone()],
        // Marker of an older window.
        vec![marker_message(&w1, 3), tail.clone()],
        // Wrong lineage-wide window number.
        vec![marker_message(&w2, 2), tail.clone()],
        // Two markers.
        vec![marker_message(&w2, 3), marker_message(&w2, 3), tail.clone()],
        // Unknown schema.
        vec![
            json!({"role": "user", "_synaps_context": {"schema": "other/1", "archive": w2.as_str(), "window": 3}, "content": "x"}),
            tail.clone(),
        ],
        // A pinned first-request stub outside the lineage.
        vec![
            json!({"role": "user", "_synaps_context": {"schema": MARKER, "archive": w2.as_str(), "window": 3,
                "pinned_stub": {"archive": "ab".repeat(16), "tokens": 9, "preview": "p"}}, "content": "x"}),
            tail.clone(),
        ],
        // A malformed pinned stub.
        vec![
            json!({"role": "user", "_synaps_context": {"schema": MARKER, "archive": w2.as_str(), "window": 3,
                "pinned_stub": {"archive": 7}}, "content": "x"}),
            tail.clone(),
        ],
    ];
    for history in cases {
        let rec = with_history(base.clone(), history.clone());
        assert!(
            matches!(refusal(&rec, &store.binding).await, Refusal::Marker(_)),
            "{history:?}"
        );
    }
    // A pinned stub inside the lineage is fine.
    let ok = with_history(
        base.clone(),
        vec![
            json!({"role": "user", "_synaps_context": {"schema": MARKER, "archive": w2.as_str(), "window": 3,
                "pinned_stub": {"archive": w1.as_str(), "tokens": 9, "preview": "p"}}, "content": "x"}),
            tail.clone(),
        ],
    );
    grant(&ok, &store.binding).await;
    // A marker without any sealed window.
    let plain = record(&a, bind, vec![], &[], None);
    let forged = with_history(plain, vec![marker_message(&w2, 1), tail]);
    assert!(matches!(
        refusal(&forged, &store.binding).await,
        Refusal::Marker(_)
    ));
}

#[tokio::test]
async fn a14_legacy_archive_resumes_only_unshortened() {
    let full: SubagentArchive = serde_json::from_value(json!({
        "version": 1,
        "meta": {"agent_name": "w", "model": "m", "system_prompt": "s", "timeout_secs": 1},
        "status": "completed",
        "output": "",
        "history": [{"role": "user", "content": "task"}]
    }))
    .unwrap();
    assert!(matches!(
        admit_legacy(&full),
        Ok(ResumeRetrieval::FullHistory)
    ));
    let shortened: SubagentArchive = serde_json::from_value(json!({
        "version": 1,
        "meta": {"agent_name": "w", "model": "m", "system_prompt": "s", "timeout_secs": 1},
        "status": "completed",
        "output": "",
        "history": [marker_message(&axel_id(1), 2)]
    }))
    .unwrap();
    assert_eq!(
        admit_legacy(&shortened).unwrap_err(),
        Refusal::LegacyShortened
    );
}

#[tokio::test]
async fn admission_revalidates_the_record() {
    let store = legacy();
    let a = run(1);
    let w = seal_run(&store.binding, &a, "one").await;
    let mut rec = record(&a, current(&store.binding), vec![], &[w], None);
    rec.history
        .push(json!({"role": "user", "content": "tampered"}));
    assert!(matches!(
        refusal(&rec, &store.binding).await,
        Refusal::Invalid(_)
    ));
}

// ── Retrieval: legacy ───────────────────────────────────────────────────────

#[tokio::test]
async fn a1_sibling_session_project_and_note_ids_rejected_legacy() {
    let store = legacy();
    let (a, b) = (run(1), run(2));
    let own = seal_run(&store.binding, &a, "NEEDLE own").await;
    let sibling = seal_run(&store.binding, &b, "NEEDLE sibling").await;
    let session = seal(&store.binding, "parent-session", "NEEDLE parent").await;
    let project = seal_run(
        &legacy_binding(&store.base, "project-b"),
        &a,
        "NEEDLE other",
    )
    .await;
    let rec = record(
        &a,
        current(&store.binding),
        vec![],
        std::slice::from_ref(&own),
        None,
    );
    let g = grant(&rec, &store.binding).await;

    let [s, f] = owned_history_tools(g.clone());
    assert_eq!((s.name(), f.name()), ("memory_search", "memory_fetch"));
    assert_eq!(s.origin(), crate::tools::ToolOrigin::Builtin);
    assert_eq!(f.origin(), crate::tools::ToolOrigin::Builtin);

    let out = search(&g, json!({"source": "history", "query": "needle"}))
        .await
        .unwrap();
    assert!(out.contains(&ctx(&own)), "{out}");
    for other in [&sibling, &session, &project] {
        assert!(!out.contains(other.as_str()), "{out}");
    }
    assert!(!out.contains("sibling") && !out.contains("parent") && !out.contains("other"));

    for denied in [
        json!({"ids": [ctx(&sibling)]}),
        json!({"ids": [ctx(&session)]}),
        json!({"ids": [ctx(&project)]}),
        json!({"ids": ["mem-0123456789"]}),
        json!({"ids": [own.as_str()]}),
        json!({"ids": [ctx(&own).to_uppercase()]}),
        json!({"ids": [ctx(&own), ctx(&sibling)]}),
        json!({"ids": [ctx(&own), ctx(&own)]}),
        json!({"ids": [ctx(&own), "mem-0123456789"]}),
        json!({"ids": []}),
        json!({"ids": ctx(&own)}),
        json!({}),
    ] {
        assert!(fetch(&g, denied.clone()).await.is_err(), "{denied}");
    }
    let body = fetch(&g, json!({"ids": [ctx(&own)]})).await.unwrap();
    assert!(body.contains("NEEDLE own"), "{body}");
    assert!(body.starts_with(crate::tools::memory::LOWER_AUTHORITY_HEADER));
}

#[tokio::test]
async fn a2_filter_applies_before_limit_and_snippets() {
    let store = legacy();
    let (a, b) = (run(1), run(2));
    // Foreign windows take the earliest slots and all match.
    for n in 0..6 {
        seal_run(&store.binding, &b, &format!("NEEDLE sibling {n}")).await;
        seal(
            &store.binding,
            "parent-session",
            &format!("NEEDLE parent {n}"),
        )
        .await;
    }
    let own = seal_run(&store.binding, &a, "NEEDLE own").await;
    let rec = record(
        &a,
        current(&store.binding),
        vec![],
        std::slice::from_ref(&own),
        None,
    );
    let g = grant(&rec, &store.binding).await;
    let out = search(
        &g,
        json!({"source": "history", "query": "needle", "limit": 1}),
    )
    .await
    .unwrap();
    assert!(out.contains(&ctx(&own)), "{out}");
    assert!(out.contains("NEEDLE own"), "{out}");
    assert_eq!(out.matches("ctx-").count(), 1, "{out}");
    // The ordinary project-wide search would have filled the limit with
    // foreign windows first.
    let wide = store.binding.history_search("needle", 1).await.unwrap();
    assert_ne!(wide[0].id, own.as_str());
    // An empty allowlist finds nothing without consulting the store.
    let support = store
        .binding
        .owned_history_support()
        .await
        .unwrap()
        .unwrap();
    assert!(store
        .binding
        .history_search_in(&support, "needle", 8, &[])
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a3_omitted_source_notes_user_scope_and_project_rejected() {
    let store = legacy();
    let a = run(1);
    let own = seal_run(&store.binding, &a, "NEEDLE own").await;
    let rec = record(
        &a,
        current(&store.binding),
        vec![],
        std::slice::from_ref(&own),
        None,
    );
    let g = grant(&rec, &store.binding).await;
    let key = store.binding.scope().unwrap().key().to_owned();
    for denied in [
        json!({"query": "needle"}),
        json!({"source": null, "query": "needle"}),
        json!({"source": "notes", "query": "needle"}),
        json!({"source": "History", "query": "needle"}),
        json!({"source": "history", "scope": "user"}),
        json!({"source": "history", "scope": 1}),
        json!({"source": "history", "project": key}),
        json!({"source": "history", "project": "p0123456789abcdef"}),
        json!({"source": "history", "tag_prefix": "t"}),
        json!({"source": "history", "query": 7}),
        json!({"source": "history", "query": "x".repeat(513)}),
        json!({"source": "history", "limit": "8"}),
    ] {
        assert!(search(&g, denied.clone()).await.is_err(), "{denied}");
    }
    assert!(
        search(&g, json!({"source": "history", "scope": "repository"}))
            .await
            .unwrap()
            .contains(&ctx(&own))
    );
    for denied in [
        json!({"ids": [ctx(&own)], "scope": "user"}),
        json!({"ids": [ctx(&own)], "project": key}),
        json!({"ids": [ctx(&own)], "source": "notes"}),
    ] {
        assert!(fetch(&g, denied.clone()).await.is_err(), "{denied}");
    }
}

/// Hold the legacy store's cross-process lock as another process would (a
/// separate open file description), until dropped.
struct ForeignLock(std::fs::File);

impl ForeignLock {
    fn hold(base: &Path) -> Self {
        use std::os::fd::AsRawFd as _;
        let scopes: Vec<_> = std::fs::read_dir(base.join("context-archives"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        let [scope] = scopes.as_slice() else {
            panic!("expected one archive scope: {scopes:?}");
        };
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(scope.join("lock"))
            .unwrap();
        // SAFETY: a valid descriptor owned by `file`.
        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        Self(file)
    }
}

impl Drop for ForeignLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd as _;
        // SAFETY: a valid descriptor owned by `self.0`.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[tokio::test]
async fn a15_held_foreign_lock_refuses_admission_and_retrieval_without_fallback() {
    let store = legacy();
    let a = run(1);
    let own = seal_run(&store.binding, &a, "NEEDLE own").await;
    seal(&store.binding, "parent-session", "NEEDLE parent").await;
    let rec = record(
        &a,
        current(&store.binding),
        vec![],
        std::slice::from_ref(&own),
        None,
    );
    let g = grant(&rec, &store.binding).await;
    let lock = ForeignLock::hold(&store.base);
    // Admission: refused, no grant.
    match refusal(&rec, &store.binding).await {
        Refusal::Lookup(why) => assert!(why.contains("archive busy"), "{why}"),
        other => panic!("expected a lookup refusal, got {other:?}"),
    }
    // Retrieval with an existing grant: an error, never results from another
    // source.
    for result in [
        search(&g, json!({"source": "history", "query": "needle"})).await,
        fetch(&g, json!({"ids": [ctx(&own)]})).await,
    ] {
        let error = result.unwrap_err().to_string();
        assert!(error.contains("archive busy"), "{error}");
    }
    drop(lock);
    let g = grant(&rec, &store.binding).await;
    let out = search(&g, json!({"source": "history", "query": "needle"}))
        .await
        .unwrap();
    assert!(out.contains(&ctx(&own)) && !out.contains("parent"), "{out}");
}

// ── Retrieval and negotiation: Axel ─────────────────────────────────────────

#[tokio::test]
async fn a1_sibling_session_and_note_ids_rejected_axel() {
    let fake = FakeAxel::new(axel_state(owned_capabilities()));
    let (own, sibling, parent) = (axel_id(0x11), axel_id(0x22), axel_id(0x33));
    let rec = record(
        &run(1),
        current(&fake.binding),
        vec![],
        std::slice::from_ref(&own),
        None,
    );
    let g = grant(&rec, &fake.binding).await;
    assert_eq!(fake.ops(), ["capabilities", "history_inspect"]);
    assert_eq!(
        fake.requests()[1]["payload"],
        json!({"ids": [own.as_str()]})
    );

    let out = search(&g, json!({"source": "history", "query": "needle"}))
        .await
        .unwrap();
    assert!(out.contains(&ctx(&own)), "{out}");
    assert!(!out.contains(sibling.as_str()) && !out.contains(parent.as_str()));
    let sent = fake.requests().last().unwrap().clone();
    assert_eq!(sent["op"], "history_search");
    assert_eq!(sent["payload"]["ids"], json!([own.as_str()]));

    let before = fake.requests().len();
    for denied in [
        json!({"ids": [ctx(&sibling)]}),
        json!({"ids": [ctx(&parent)]}),
        json!({"ids": ["mem-0123456789"]}),
        json!({"ids": [ctx(&own), ctx(&sibling)]}),
        json!({"ids": [ctx(&own)], "scope": "user"}),
    ] {
        assert!(fetch(&g, denied.clone()).await.is_err(), "{denied}");
    }
    for denied in [
        json!({"query": "needle"}),
        json!({"source": "notes"}),
        json!({"source": "history", "scope": "user"}),
    ] {
        assert!(search(&g, denied.clone()).await.is_err(), "{denied}");
    }
    // Every refusal happened before the service was asked.
    assert_eq!(fake.requests().len(), before);
    let body = fetch(&g, json!({"ids": [ctx(&own)]})).await.unwrap();
    assert!(body.contains("NEEDLE own body"), "{body}");
}

#[tokio::test]
async fn a12_axel_ids_outside_the_allowlist_are_refused() {
    let fake = FakeAxel::new(axel_state(owned_capabilities()));
    let own = axel_id(0x11);
    let rec = record(
        &run(1),
        current(&fake.binding),
        vec![],
        std::slice::from_ref(&own),
        None,
    );
    let g = grant(&rec, &fake.binding).await;
    // A service that ignores the filter returns foreign rows: refused, never
    // passed through or post-filtered.
    let mut state = axel_state(owned_capabilities());
    state["ignore_ids"] = json!(true);
    fake.set(state);
    assert!(search(&g, json!({"source": "history", "query": "needle"}))
        .await
        .is_err());
    let support = fake.binding.owned_history_support().await.unwrap().unwrap();
    assert!(fake
        .binding
        .history_inspect(&support, &[own.as_str().to_owned()])
        .await
        .is_err());
    assert!(matches!(
        refusal(&rec, &fake.binding).await,
        Refusal::Lookup(_)
    ));
    // An empty allowlist sends nothing at all.
    let before = fake.requests().len();
    assert!(fake
        .binding
        .history_search_in(&support, "needle", 8, &[])
        .await
        .unwrap()
        .is_empty());
    assert_eq!(fake.requests().len(), before);
}

#[tokio::test]
async fn a12_malformed_or_out_of_bounds_axel_replies_refuse_the_whole_call() {
    let fake = FakeAxel::new(axel_state(owned_capabilities()));
    let own = axel_id(0x11);
    let rec = record(
        &run(1),
        current(&fake.binding),
        vec![],
        std::slice::from_ref(&own),
        None,
    );
    let g = grant(&rec, &fake.binding).await;
    let good = row(&own, "NEEDLE own");
    let with = |key: &str, value: Value| {
        let mut r = good.clone();
        r[key] = value;
        r
    };
    for bad in [
        json!([good.clone(), good.clone()]),
        json!([with("snippet", json!("x".repeat(257)))]),
        json!([with("message_count", json!(4097))]),
        json!([with("source_message_count", json!(4097))]),
        json!([with("id", json!("AB".repeat(16)))]),
        json!([with("id", json!(7))]),
        json!([with("message_count", json!(-1))]),
        json!([{"id": own.as_str()}]),
        json!([1]),
        json!({"rows": [good.clone()]}),
        json!(null),
    ] {
        let mut state = axel_state(owned_capabilities());
        state["search_reply"] = bad.clone();
        fake.set(state);
        assert!(
            search(&g, json!({"source": "history", "query": "needle"}))
                .await
                .is_err(),
            "{bad}"
        );
    }
    let owner = logical_digest(&worker_logical_id(&run(1))).unwrap();
    let good = json!({"id": own.as_str(), "logical_id": owner, "live": true});
    let with = |key: &str, value: Value| {
        let mut e = good.clone();
        e[key] = value;
        e
    };
    let support = fake.binding.owned_history_support().await.unwrap().unwrap();
    for bad in [
        json!([good.clone(), good.clone()]),
        json!([with("logical_id", json!(owner.to_uppercase()))]),
        json!([with("logical_id", json!(&owner[1..]))]),
        json!([with("logical_id", json!(format!("{}g", &owner[1..])))]),
        json!([with("logical_id", json!(worker_logical_id(&run(1))))]),
        json!([with("live", json!("yes"))]),
        json!([with("extra", json!(1))]),
        json!([{"id": own.as_str(), "live": true}]),
        json!([1]),
        json!({"entries": [good.clone()]}),
        json!(null),
    ] {
        let mut state = axel_state(owned_capabilities());
        state["inspect_reply"] = bad.clone();
        fake.set(state);
        assert!(
            fake.binding
                .history_inspect(&support, &[own.as_str().to_owned()])
                .await
                .is_err(),
            "{bad}"
        );
        assert!(
            matches!(refusal(&rec, &fake.binding).await, Refusal::Lookup(_)),
            "{bad}"
        );
    }
    // The well-formed reply is accepted by the same path.
    let mut state = axel_state(owned_capabilities());
    state["inspect_reply"] = json!([good]);
    fake.set(state);
    grant(&rec, &fake.binding).await;
}

#[tokio::test]
async fn a13_support_is_bound_to_the_binding_that_negotiated_it() {
    let owned = FakeAxel::new(axel_state(owned_capabilities()));
    let older = FakeAxel::new(axel_state(older_capabilities()));
    let other_owned = FakeAxel::new(axel_state(owned_capabilities()));
    let ids = [axel_id(0x11).as_str().to_owned()];
    let support = owned
        .binding
        .owned_history_support()
        .await
        .unwrap()
        .unwrap();
    assert!(older
        .binding
        .owned_history_support()
        .await
        .unwrap()
        .is_none());
    let older_before = older.requests().len();
    // Same backend kind, other service: refused before any request, whether
    // or not that service has the capability.
    for fake in [&older, &other_owned] {
        assert!(fake
            .binding
            .history_search_in(&support, "", 8, &ids)
            .await
            .is_err());
        assert!(fake.binding.history_inspect(&support, &ids).await.is_err());
    }
    assert_eq!(older.requests().len(), older_before);
    assert!(other_owned.requests().is_empty());
    // A clone of the negotiating binding is the same binding.
    let clone = owned.binding.clone();
    assert_eq!(
        clone.history_inspect(&support, &ids).await.unwrap().entries.len(),
        1
    );
    // A support negotiated anew by another binding is a different value.
    let other = other_owned
        .binding
        .owned_history_support()
        .await
        .unwrap()
        .unwrap();
    assert_ne!(support, other);
    assert_eq!(
        support,
        owned
            .binding
            .owned_history_support()
            .await
            .unwrap()
            .unwrap()
    );

    // Legacy: two bindings of one store are still two bindings.
    let store = legacy();
    let twin = legacy_binding(&store.base, "project-a");
    let a = run(1);
    let own = seal_run(&store.binding, &a, "NEEDLE own").await;
    let legacy_support = store
        .binding
        .owned_history_support()
        .await
        .unwrap()
        .unwrap();
    let own_ids = [own.as_str().to_owned()];
    assert!(twin
        .history_search_in(&legacy_support, "", 8, &own_ids)
        .await
        .is_err());
    assert!(twin
        .history_inspect(&legacy_support, &own_ids)
        .await
        .is_err());
    assert_eq!(
        store
            .binding
            .history_inspect(&legacy_support, &own_ids)
            .await
            .unwrap()
            .entries
            .len(),
        1
    );
}

#[tokio::test]
async fn a5_axel_foreign_and_tombstoned_refs_refuse() {
    let mut state = axel_state(owned_capabilities());
    let gone = axel_id(0x44);
    state["entries"]
        .as_array_mut()
        .unwrap()
        .push(entry(&gone, &worker_logical_id(&run(1)), false));
    let fake = FakeAxel::new(state);
    let (own, sibling, parent) = (axel_id(0x11), axel_id(0x22), axel_id(0x33));
    let evicted = axel_id(0x55);
    let bind = current(&fake.binding);
    for (listed, expected) in [
        (sibling.clone(), Refusal::Foreign(sibling.clone())),
        (parent.clone(), Refusal::Foreign(parent.clone())),
        (gone.clone(), Refusal::Tombstoned(gone.clone())),
    ] {
        let rec = record(
            &run(1),
            bind.clone(),
            vec![],
            &[own.clone(), listed.clone()],
            None,
        );
        assert_eq!(refusal(&rec, &fake.binding).await, expected);
        // An evicted window listed before it changes nothing (B31).
        let rec = record(
            &run(1),
            bind.clone(),
            vec![],
            &[evicted.clone(), own.clone(), listed],
            None,
        );
        assert_eq!(refusal(&rec, &fake.binding).await, expected);
    }
}

// ── Evicted windows (B31) ───────────────────────────────────────────────────

/// [`axel_state`] plus run 1's windows 0x12 and 0x13. Windows 0x10 and 0x14
/// of run 1 are evicted: the store answers without them.
fn axel_state_with_windows() -> Value {
    let mut state = axel_state(owned_capabilities());
    let entries = state["entries"].as_array_mut().unwrap();
    for n in [0x12, 0x13] {
        entries.push(entry(&axel_id(n), &worker_logical_id(&run(1)), true));
    }
    state
}

#[tokio::test]
async fn b31_an_evicted_older_window_is_admitted_without_it() {
    let fake = FakeAxel::new(axel_state_with_windows());
    let (evicted, w2, w3) = (axel_id(0x10), axel_id(0x11), axel_id(0x12));
    let rec = record(
        &run(1),
        current(&fake.binding),
        vec![],
        &[evicted.clone(), w2.clone(), w3.clone()],
        None,
    );
    let (g, gone) = owned(&rec, &fake.binding).await;
    assert_eq!(gone, std::slice::from_ref(&evicted));
    assert_eq!(g.allow(), [w2.as_str(), w3.as_str()]);
    assert_eq!(g.run_id(), &run(1));
    // One inspection asked about every listed window.
    assert_eq!(fake.ops(), ["capabilities", "history_inspect"]);
    assert_eq!(
        fake.requests()[1]["payload"],
        json!({"ids": [evicted.as_str(), w2.as_str(), w3.as_str()]})
    );
    // The evicted window is refused before the service is asked.
    let before = fake.requests().len();
    assert!(fetch(&g, json!({"ids": [ctx(&evicted)]})).await.is_err());
    assert_eq!(fake.requests().len(), before);
    assert_eq!(
        evicted_notice(gone.len()).as_deref(),
        Some(
            "[1 earlier context window of this worker is no longer in memory (removed to make \
             room, or outside this project's memory scope). Continue from the summary and \
             retained messages you have; that window's history can't be fetched.]"
        )
    );
}

#[tokio::test]
async fn b31_the_latest_window_evicted_is_admitted_with_a_notice() {
    let fake = FakeAxel::new(axel_state_with_windows());
    let (w1, w2, latest) = (axel_id(0x11), axel_id(0x12), axel_id(0x14));
    let rec = record(
        &run(1),
        current(&fake.binding),
        vec![],
        &[w1.clone(), w2.clone(), latest.clone()],
        None,
    );
    // The marker check is the record's own: its history still names the
    // latest window.
    assert!(history_matches_lineage(&rec));
    let (g, gone) = owned(&rec, &fake.binding).await;
    assert_eq!(gone, std::slice::from_ref(&latest));
    assert_eq!(g.allow(), [w1.as_str(), w2.as_str()]);
    assert!(!g.allows(latest.as_str()));
    let notice = evicted_notice(gone.len()).unwrap();
    assert!(
        notice.starts_with("[1 earlier context window of this worker is no longer in memory"),
        "{notice}"
    );
    assert_eq!(evicted_notice(0), None);
    let many = evicted_notice(3).unwrap();
    assert!(
        many.starts_with("[3 earlier context windows of this worker are no longer in memory")
            && many.contains("those windows' history can't be fetched"),
        "{many}"
    );
}

#[tokio::test]
async fn b31_every_window_evicted_is_admitted_with_an_empty_grant() {
    let fake = FakeAxel::new(axel_state(owned_capabilities()));
    let (a1, a2, c1) = (axel_id(0x0a), axel_id(0x0b), axel_id(0x0c));
    let pred = record(
        &run(1),
        current(&fake.binding),
        vec![],
        &[a1.clone(), a2.clone()],
        None,
    );
    let lineage = successor_lineage(&pred, RecordKind::Final).unwrap();
    let rec = record(
        &run(3),
        current(&fake.binding),
        lineage.inherited_windows,
        std::slice::from_ref(&c1),
        Some(lineage.predecessor),
    );
    // Inherited and own windows alike.
    let (g, gone) = owned(&rec, &fake.binding).await;
    assert_eq!(gone, [a1, a2, c1]);
    assert!(g.allow().is_empty());
    assert_eq!(g.run_id(), &run(3));
    // Never full history: the owned tools stay and ask the service nothing.
    let before = fake.requests().len();
    let out = search(&g, json!({"source": "history", "query": "needle"}))
        .await
        .unwrap();
    assert!(!out.contains("ctx-"), "{out}");
    assert!(fetch(&g, json!({"ids": [ctx(&axel_id(0x11))]})).await.is_err());
    assert_eq!(fake.requests().len(), before);
    // A successful empty answer evicts every listed window too.
    let mut state = axel_state(owned_capabilities());
    state["inspect_reply"] = json!([]);
    fake.set(state);
    let own = record(&run(1), current(&fake.binding), vec![], &[axel_id(0x11)], None);
    let (g, gone) = owned(&own, &fake.binding).await;
    assert_eq!(gone, [axel_id(0x11)]);
    assert!(g.allow().is_empty());
}

#[tokio::test]
async fn b31_a_failed_inspection_is_a_lookup_refusal_not_eviction() {
    let fake = FakeAxel::new(axel_state(owned_capabilities()));
    let rec = record(
        &run(1),
        current(&fake.binding),
        vec![],
        &[axel_id(0x10), axel_id(0x11)],
        None,
    );
    for bad in [json!(null), json!({"entries": []}), json!([1])] {
        let mut state = axel_state(owned_capabilities());
        state["inspect_reply"] = bad.clone();
        fake.set(state);
        assert!(
            matches!(refusal(&rec, &fake.binding).await, Refusal::Lookup(_)),
            "{bad}"
        );
    }
    // A service that cannot answer at all.
    std::fs::remove_file(fake.dir.join("state.json")).unwrap();
    assert!(matches!(
        refusal(&rec, &fake.binding).await,
        Refusal::Lookup(_)
    ));
}

// ── Evicted vs never held (B31 follow-up b) ─────────────────────────────────

/// [`axel_state_with_windows`] on a `history_owned/2` service whose brain
/// reports `evictions` (`None`: the key is absent, a brain that predates
/// the count).
fn axel_state_v2(evictions: Option<u64>) -> Value {
    let mut state = axel_state_with_windows();
    state["capabilities"] = owned_capabilities_v2();
    if let Some(n) = evictions {
        state["evictions"] = json!(n);
    }
    state
}

/// Window 0x10 of run 1 is absent; 0x11 and 0x12 are live.
fn absent_first(binding: &MemoryBinding) -> WorkerRecord {
    record(
        &run(1),
        current(binding),
        vec![],
        &[axel_id(0x10), axel_id(0x11), axel_id(0x12)],
        None,
    )
}

#[tokio::test]
async fn b31b_absent_windows_in_a_store_that_never_evicted_are_not_held() {
    let fake = FakeAxel::new(axel_state_v2(Some(0)));
    let rec = absent_first(&fake.binding);
    let refused = refusal(&rec, &fake.binding).await;
    assert_eq!(refused, Refusal::NotHeld { absent: 1, listed: 3 });
    // The host asked for version 2, once, about every listed window.
    assert_eq!(fake.ops(), ["capabilities", "history_inspect"]);
    assert_eq!(
        fake.requests()[1]["payload"],
        json!({"ids": [axel_id(0x10).as_str(), axel_id(0x11).as_str(), axel_id(0x12).as_str()],
               "version": 2})
    );
    // Counts only: no IDs, paths or content.
    assert_eq!(
        refused.to_string(),
        "this memory store holds 1 of this worker's 3 windows nowhere and has never evicted \
         history: it is not the store the worker ran against (brain wiped, replaced or \
         misconfigured)"
    );
    // Every window absent: the same refusal, never an empty grant.
    let none = record(&run(1), current(&fake.binding), vec![], &[axel_id(0x0a)], None);
    assert_eq!(
        refusal(&none, &fake.binding).await,
        Refusal::NotHeld { absent: 1, listed: 1 }
    );
}

#[tokio::test]
async fn b31b_absent_windows_in_a_store_that_has_evicted_are_evicted() {
    let fake = FakeAxel::new(axel_state_v2(Some(7)));
    let rec = absent_first(&fake.binding);
    let (g, gone) = owned(&rec, &fake.binding).await;
    assert_eq!(gone, [axel_id(0x10)]);
    assert_eq!(g.allow(), [axel_id(0x11).as_str(), axel_id(0x12).as_str()]);
    let retrieval = admit_record(&rec, &fake.binding).await.unwrap();
    assert_eq!(evicted_count(&retrieval), 1);
}

#[tokio::test]
async fn b31b_an_unknown_eviction_count_keeps_the_eviction_reading() {
    // A v2 brain that predates the count answers null.
    let fake = FakeAxel::new(axel_state_v2(None));
    let rec = absent_first(&fake.binding);
    let (_, gone) = owned(&rec, &fake.binding).await;
    assert_eq!(gone, [axel_id(0x10)]);
    // A v1 service is never asked for the count, whatever its brain holds.
    let mut state = axel_state_with_windows();
    state["evictions"] = json!(0);
    fake.set(state);
    let before = fake.requests().len();
    let (_, gone) = owned(&rec, &fake.binding).await;
    assert_eq!(gone, [axel_id(0x10)]);
    let asked = &fake.requests()[before + 1];
    assert_eq!(asked["op"], "history_inspect");
    assert!(asked["payload"].get("version").is_none(), "{asked}");
}

#[tokio::test]
async fn b31b_all_windows_live_in_a_store_that_never_evicted_are_admitted() {
    let fake = FakeAxel::new(axel_state_v2(Some(0)));
    let rec = record(
        &run(1),
        current(&fake.binding),
        vec![],
        &[axel_id(0x11), axel_id(0x12), axel_id(0x13)],
        None,
    );
    let g = grant(&rec, &fake.binding).await;
    assert_eq!(g.allow().len(), 3);
    let retrieval = admit_record(&rec, &fake.binding).await.unwrap();
    assert_eq!(evicted_count(&retrieval), 0);
    // Foreign and tombstoned windows still refuse first.
    let foreign = record(
        &run(1),
        current(&fake.binding),
        vec![],
        &[axel_id(0x10), axel_id(0x22)],
        None,
    );
    assert_eq!(
        refusal(&foreign, &fake.binding).await,
        Refusal::Foreign(axel_id(0x22))
    );
}

#[tokio::test]
async fn b31b_inspections_report_the_store_eviction_count() {
    let ids = [axel_id(0x11).as_str().to_owned(), axel_id(0x10).as_str().to_owned()];
    // Axel version 2: the count as the service reports it.
    for (evictions, expected) in [(Some(0), Some(0)), (Some(4), Some(4)), (None, None)] {
        let fake = FakeAxel::new(axel_state_v2(evictions));
        let support = fake.binding.owned_history_support().await.unwrap().unwrap();
        let inspection = fake.binding.history_inspect(&support, &ids).await.unwrap();
        assert_eq!(inspection.entries.len(), 1);
        assert_eq!(inspection.evictions, expected);
    }
    // Axel version 1: unknown.
    let fake = FakeAxel::new(axel_state_with_windows());
    let support = fake.binding.owned_history_support().await.unwrap().unwrap();
    let inspection = fake.binding.history_inspect(&support, &ids).await.unwrap();
    assert_eq!((inspection.entries.len(), inspection.evictions), (1, None));
    // A malformed version 2 answer is a failed inspection, never eviction.
    let fake = FakeAxel::new(axel_state_v2(Some(0)));
    let support = fake.binding.owned_history_support().await.unwrap().unwrap();
    for bad in [
        json!([]),
        json!({"entries": []}),
        json!({"entries": [], "evictions": -1}),
        json!({"entries": [], "evictions": 0, "extra": 1}),
    ] {
        let mut state = axel_state_v2(Some(0));
        state["inspect_reply"] = bad.clone();
        fake.set(state);
        // A version 2 reply always carries the count (or null): a missing
        // key is malformed, not an unknown count.
        let result = fake.binding.history_inspect(&support, &ids).await;
        assert!(result.is_err(), "{bad}");
    }
    // The legacy store never evicts.
    let store = legacy();
    let own = seal_run(&store.binding, &run(1), "own").await;
    let support = store.binding.owned_history_support().await.unwrap().unwrap();
    let inspection = store
        .binding
        .history_inspect(&support, &[own.as_str().to_owned(), axel_id(0x10).as_str().to_owned()])
        .await
        .unwrap();
    assert_eq!((inspection.entries.len(), inspection.evictions), (1, Some(0)));
}

#[test]
fn b31a_the_parent_note_counts_evicted_windows_only() {
    assert_eq!(evicted_count(&ResumeRetrieval::FullHistory), 0);
    assert_eq!(evicted_parent_note(0), None);
    assert_eq!(
        evicted_parent_note(1).as_deref(),
        Some(
            "1 earlier context window of the resumed worker is no longer in memory; it \
             continues from its summary"
        )
    );
    assert_eq!(
        evicted_parent_note(3).as_deref(),
        Some(
            "3 earlier context windows of the resumed worker are no longer in memory; it \
             continues from its summary"
        )
    );
}

#[tokio::test]
async fn a8_older_axel_refuses_shortened_resume_and_worker_rollover() {
    let fake = FakeAxel::new(axel_state(older_capabilities()));
    let own = axel_id(0x11);
    let rec = record(&run(1), current(&fake.binding), vec![], &[own], None);
    assert_eq!(
        refusal(&rec, &fake.binding).await,
        Refusal::RetrievalUnavailable
    );
    assert!(!worker_rollover_supported(&fake.binding).await);
    assert!(fake
        .binding
        .owned_history_support()
        .await
        .unwrap()
        .is_none());
    // Nothing but the capability query reached the service.
    assert!(fake.ops().iter().all(|op| op == "capabilities"));
    // An unshortened record needs no retrieval and is admitted.
    let plain = record(&run(1), current(&fake.binding), vec![], &[], None);
    assert!(matches!(
        admit_record(&plain, &fake.binding).await,
        Ok(ResumeRetrieval::FullHistory)
    ));

    // Unknown versions, a small ID bound or another logical-ID derivation
    // are not the negotiated contract either.
    for broken in [
        json!({"versions": [2], "max_ids": 128}),
        json!({"versions": [1], "max_ids": 64}),
        json!({"version": 1, "max_ids": 128}),
        json!(1),
    ] {
        let mut caps = owned_capabilities();
        caps["history_owned"] = broken.clone();
        fake.set(axel_state(caps));
        assert!(
            fake.binding
                .owned_history_support()
                .await
                .unwrap()
                .is_none(),
            "{broken}"
        );
    }
    let mut caps = owned_capabilities();
    caps["history_logical_id"] = json!("sha256_plain");
    fake.set(axel_state(caps));
    assert!(fake
        .binding
        .owned_history_support()
        .await
        .unwrap()
        .is_none());
    let mut caps = owned_capabilities();
    caps["operations"] = json!(["history_search", "history_fetch"]);
    fake.set(axel_state(caps));
    assert!(fake
        .binding
        .owned_history_support()
        .await
        .unwrap()
        .is_none());

    // The current contract enables it; legacy always filters in-process.
    fake.set(axel_state(owned_capabilities()));
    assert!(worker_rollover_supported(&fake.binding).await);
    assert!(worker_rollover_supported(&legacy().binding).await);

    // A support token from one backend kind is refused by another.
    let store = legacy();
    let legacy_support = store
        .binding
        .owned_history_support()
        .await
        .unwrap()
        .unwrap();
    let axel_support = fake.binding.owned_history_support().await.unwrap().unwrap();
    assert_ne!(legacy_support, axel_support);
    assert!(fake
        .binding
        .history_search_in(&legacy_support, "", 8, &[axel_id(0x11).as_str().to_owned()])
        .await
        .is_err());
    assert!(store
        .binding
        .history_inspect(&axel_support, &[axel_id(0x11).as_str().to_owned()])
        .await
        .is_err());
}

// ── Resume from disk after two rollovers and a restart ──────────────────────

#[tokio::test]
async fn a6_resume_from_head_and_final_after_two_rollovers_and_restart() {
    let store = legacy();
    let r = run(7);
    let w1 = seal_run(&store.binding, &r, "NEEDLE first window").await;
    let w2 = seal_run(&store.binding, &r, "NEEDLE second window").await;
    let head = {
        let mut h = record(
            &r,
            current(&store.binding),
            vec![],
            &[w1.clone(), w2.clone()],
            None,
        );
        h.kind = RecordKind::Head;
        h
    };
    let mut fin = record(
        &r,
        current(&store.binding),
        vec![],
        &[w1.clone(), w2.clone()],
        None,
    );
    let mut history = fin.history.clone();
    history.push(json!({"role": "user", "content": "a later round"}));
    fin = with_history(fin, history);

    let root = store.base.join("subagent-history");
    let dir = root.join(PROCESS_DIR);
    private_dir(&root);
    private_dir(&dir);
    private_file(
        &dir.join("sa_5.head.json"),
        &serde_json::to_vec(&head).unwrap(),
    );
    private_file(&dir.join("sa_5.json"), &serde_json::to_vec(&fin).unwrap());

    // "Restart": a fresh binding of the same storage, nothing in memory.
    let restarted = legacy_binding(&store.base, "project-a");
    let raw = dir.join("sa_5.head.json");
    let location = record_file::locate(raw.to_str().unwrap(), &root).unwrap();
    assert_eq!(location.handle(), "sa_5");
    assert_eq!(location.kind(), RecordKind::Head);
    let resolved =
        record_file::resolve_location(&location, WriterState::Quiescent, Some(current(&restarted)))
            .unwrap();
    let Resolution::Authoritative {
        kind,
        record,
        loss_warning,
    } = resolved
    else {
        panic!("final should resolve: {resolved:?}");
    };
    assert_eq!((kind, loss_warning), (RecordKind::Final, false));
    let g = grant(&record, &restarted).await;
    assert_eq!(g.allow().len(), 2);
    let body = fetch(&g, json!({"ids": [ctx(&w1)]})).await.unwrap();
    assert!(body.contains("NEEDLE first window"), "{body}");

    // Without the final, the head is the crash checkpoint.
    std::fs::remove_file(dir.join("sa_5.json")).unwrap();
    let raw = dir.join("sa_5.json");
    let location = record_file::locate(raw.to_str().unwrap(), &root).unwrap();
    let resolved =
        record_file::resolve_location(&location, WriterState::Quiescent, Some(current(&restarted)))
            .unwrap();
    let Resolution::Authoritative {
        kind,
        record,
        loss_warning,
    } = resolved
    else {
        panic!("head should resolve: {resolved:?}");
    };
    assert_eq!((kind, loss_warning), (RecordKind::Head, true));
    let g = grant(&record, &restarted).await;
    let out = search(&g, json!({"source": "history", "query": "second"}))
        .await
        .unwrap();
    assert!(out.contains(&ctx(&w2)), "{out}");

    // Its successor inherits both windows and is admitted the same way.
    let lineage = successor_lineage(&record, kind).unwrap();
    let next = record_fn_successor(&restarted, lineage);
    assert_eq!(grant(&next, &restarted).await.allow().len(), 2);

    // A writer that may still publish blocks resolution.
    let resolved = record_file::resolve_location(
        &location,
        WriterState::MayPublish,
        Some(current(&restarted)),
    )
    .unwrap();
    assert!(matches!(resolved, Resolution::Unresolved(_)));
}

fn record_fn_successor(binding: &MemoryBinding, lineage: SuccessorLineage) -> WorkerRecord {
    record(
        &run(8),
        current(binding),
        lineage.inherited_windows,
        &[],
        Some(lineage.predecessor),
    )
}

// ── Record-file paths ───────────────────────────────────────────────────────

struct Root {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    dir: PathBuf,
}

fn root_with_record() -> Root {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap().join("subagent-history");
    let dir = root.join(PROCESS_DIR);
    private_dir(&root);
    private_dir(&dir);
    let binding = Binding {
        backend: BackendKind::Legacy,
        scope_digest: scope_digest("p0123456789abcdef"),
    };
    let rec = record(&run(1), binding, vec![], &[], None);
    private_file(&dir.join("sa_3.json"), &serde_json::to_vec(&rec).unwrap());
    Root {
        _tmp: tmp,
        root,
        dir,
    }
}

fn load_path(root: &Root, path: &Path) -> Result<record_file::LoadedRecords, RecordFileError> {
    let location = record_file::locate(path.to_str().unwrap(), &root.root)?;
    record_file::load(&location)
}

#[test]
fn a7_valid_location_loads_both_siblings_by_name() {
    let r = root_with_record();
    let loaded = load_path(&r, &r.dir.join("sa_3.json")).unwrap();
    assert!(matches!(loaded.head, Candidate::Absent));
    assert!(matches!(
        loaded.final_record,
        Candidate::Present(FinalRecord::V2(_))
    ));
    // A sibling that does not parse is unreadable, for the resolver to judge.
    private_file(&r.dir.join("sa_3.head.json"), b"{not json");
    let loaded = load_path(&r, &r.dir.join("sa_3.json")).unwrap();
    assert!(matches!(loaded.head, Candidate::Unreadable(_)));
}

#[test]
fn a7_in_root_symlink_rejected() {
    use std::os::unix::fs::symlink;
    let r = root_with_record();
    // A symlinked record file inside the root.
    symlink(r.dir.join("sa_3.json"), r.dir.join("sa_9.json")).unwrap();
    assert_eq!(
        load_path(&r, &r.dir.join("sa_9.json")).unwrap_err(),
        RecordFileError::Symlink
    );
    // A symlinked process directory inside the root.
    let alias = r.root.join("1-0f5c1e4a-7d2b-4c3e-9a8b-1c2d3e4f5a6b");
    symlink(&r.dir, &alias).unwrap();
    assert_eq!(
        load_path(&r, &alias.join("sa_3.json")).unwrap_err(),
        RecordFileError::Symlink
    );
    // A symlinked sibling refuses the whole load.
    symlink(r.dir.join("sa_3.json"), r.dir.join("sa_3.head.json")).unwrap();
    assert_eq!(
        load_path(&r, &r.dir.join("sa_3.json")).unwrap_err(),
        RecordFileError::Symlink
    );
}

#[test]
fn a7_private_regular_single_link_files_only() {
    use std::os::unix::fs::PermissionsExt;
    let r = root_with_record();
    // Hard link: two names for one record.
    std::fs::hard_link(r.dir.join("sa_3.json"), r.dir.join("sa_4.json")).unwrap();
    assert_eq!(
        load_path(&r, &r.dir.join("sa_4.json")).unwrap_err(),
        RecordFileError::NotPrivate
    );
    std::fs::remove_file(r.dir.join("sa_4.json")).unwrap();
    // Group/other-readable file or directory.
    std::fs::set_permissions(
        r.dir.join("sa_3.json"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert_eq!(
        load_path(&r, &r.dir.join("sa_3.json")).unwrap_err(),
        RecordFileError::NotPrivate
    );
    std::fs::set_permissions(
        r.dir.join("sa_3.json"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    std::fs::set_permissions(&r.dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        load_path(&r, &r.dir.join("sa_3.json")).unwrap_err(),
        RecordFileError::NotPrivate
    );
    std::fs::set_permissions(&r.dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    // A FIFO is not a record and must not block the open.
    let fifo = std::ffi::CString::new(r.dir.join("sa_6.json").to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert_eq!(
        load_path(&r, &r.dir.join("sa_6.json")).unwrap_err(),
        RecordFileError::NotRegular
    );
    // A directory where a record should be.
    private_dir(&r.dir.join("sa_7.json"));
    assert_eq!(
        load_path(&r, &r.dir.join("sa_7.json")).unwrap_err(),
        RecordFileError::NotRegular
    );
}

#[test]
fn a7_whole_file_byte_limit_before_allocation() {
    let r = root_with_record();
    let big = r.dir.join("sa_8.json");
    private_file(&big, b"");
    // Sparse: reported size only, nothing is written or read.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&big)
        .unwrap()
        .set_len(MAX_RECORD_FILE_BYTES + 1)
        .unwrap();
    assert_eq!(
        load_path(&r, &big).unwrap_err(),
        RecordFileError::TooLarge(MAX_RECORD_FILE_BYTES + 1)
    );
    // The same limit guards the derived sibling.
    std::fs::rename(&big, r.dir.join("sa_3.head.json")).unwrap();
    assert_eq!(
        load_path(&r, &r.dir.join("sa_3.json")).unwrap_err(),
        RecordFileError::TooLarge(MAX_RECORD_FILE_BYTES + 1)
    );
}

#[test]
fn a7_paths_outside_root_or_misnamed_are_refused() {
    let r = root_with_record();
    let root = &r.root;
    let refused = |raw: String| record_file::locate(&raw, root).unwrap_err();
    assert_eq!(refused("/etc/passwd".into()), RecordFileError::NotUnderRoot);
    assert_eq!(
        refused(format!("{}/../sa_3.json", r.dir.display())),
        RecordFileError::NotUnderRoot
    );
    assert_eq!(
        refused(format!("{}/./sa_3.json", r.dir.display())),
        RecordFileError::NotUnderRoot
    );
    assert_eq!(
        refused("relative/sa_3.json".into()),
        RecordFileError::NotUnderRoot
    );
    assert_eq!(
        refused(format!("{}/sa_3.json", root.display())),
        RecordFileError::NotUnderRoot
    );
    assert_eq!(
        refused(format!("{}/sub/sa_3.json", r.dir.display())),
        RecordFileError::NotUnderRoot
    );
    for name in [
        "sa_03.json",
        "sa_0.json",
        "sa_3.json.json",
        "sa_3.final.json",
        "notes.json",
        "sa_3",
        "sa_3.head.json.tmp",
    ] {
        assert_eq!(
            refused(format!("{}/{name}", r.dir.display())),
            RecordFileError::BadName,
            "{name}"
        );
    }
    assert_eq!(
        refused(format!("{}/not-a-process-dir/sa_3.json", root.display())),
        RecordFileError::BadName
    );
    let ok = record_file::locate(r.dir.join("sa_3.json").to_str().unwrap(), root).unwrap();
    assert_eq!((ok.handle(), ok.kind()), ("sa_3", RecordKind::Final));
}

#[test]
fn a7_root_spelled_through_a_trusted_alias_is_accepted() {
    use std::os::unix::fs::symlink;
    let r = root_with_record();
    // The configured root may itself sit behind a symlink (trusted host
    // configuration); paths spelled with its canonical form still match.
    let alias = r.root.parent().unwrap().join("alias-root");
    symlink(&r.root, &alias).unwrap();
    let path = r.dir.join("sa_3.json");
    let location = record_file::locate(path.to_str().unwrap(), &alias).unwrap();
    assert!(record_file::load(&location).is_ok());
    let aliased = alias.join(PROCESS_DIR).join("sa_3.json");
    let location = record_file::locate(aliased.to_str().unwrap(), &alias).unwrap();
    assert!(record_file::load(&location).is_ok());
}
