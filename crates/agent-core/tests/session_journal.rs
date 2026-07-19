//! Phase 5 / Task 35 — opt-in session journal + snapshots (spec §9.8;
//! docs/decisions/T35-session-journal-opt-in.md).
//!
//! Contract under test:
//!
//! 1. `session_persistence = json` (the DEFAULT) is byte-for-byte legacy
//!    behavior: one atomic `<id>.json` per save, no journal file, and
//!    old-format sessions load unchanged.
//! 2. `journal` mode is opt-in and additive: the snapshot keeps the
//!    unchanged legacy schema; deltas append to `<id>.journal`; steady-state
//!    save cost is proportional to the DELTA, never to total history.
//! 3. Recovery: torn tails, stale journals after a snapshot, and gaps all
//!    replay idempotently to a consistent session (kill-during-save safety).
//! 4. Rollback: one json-mode save folds the journal away entirely.
//! 5. Retention treats `<id>.json` + `<id>.journal` as ONE artifact and
//!    chain heads protect both; orphan journals are swept.

use agent_core::core::retention::{sweep_at, RetentionPolicy, RetentionRoots};
use agent_core::core::session::Session;
use agent_core::core::session_journal::{
    delete_session_files_in_dir, journal_meta_tail, journal_path, load_session_in_dir,
    save_session_in_dir, snapshot_due, SaveMode, SessionPersistence, JOURNAL_SNAPSHOT_MIN_BYTES,
};
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

// ─── helpers ─────────────────────────────────────────────────────────────────

fn session_with_messages(n: usize) -> Session {
    let mut s = Session::new("claude-sonnet-4-6", "medium", Some("harness prompt"));
    for i in 0..n {
        push_msg(&mut s, &format!("message body {i}"));
    }
    s
}

fn push_msg(s: &mut Session, body: &str) {
    let role = if s.api_messages.len() % 2 == 0 {
        "user"
    } else {
        "assistant"
    };
    s.api_messages
        .push(Arc::new(json!({"role": role, "content": body})));
}

fn save(dir: &TempDir, s: &Session, mode: SessionPersistence) -> SaveMode {
    save_session_in_dir(dir.path(), s, mode)
        .expect("save must succeed")
        .mode
}

fn load(dir: &TempDir, id: &str) -> Session {
    load_session_in_dir(dir.path(), id).expect("load must succeed")
}

fn snapshot_file(dir: &TempDir, id: &str) -> std::path::PathBuf {
    dir.path().join(format!("{id}.json"))
}

fn assert_equivalent(a: &Session, b: &Session) {
    assert_eq!(a.id, b.id);
    assert_eq!(a.title, b.title);
    assert_eq!(a.name, b.name);
    assert_eq!(a.model, b.model);
    assert_eq!(a.thinking_level, b.thinking_level);
    assert_eq!(a.system_prompt, b.system_prompt);
    assert_eq!(a.total_input_tokens, b.total_input_tokens);
    assert_eq!(a.total_output_tokens, b.total_output_tokens);
    assert_eq!(a.session_cost, b.session_cost);
    assert_eq!(a.abort_context, b.abort_context);
    assert_eq!(a.parent_session, b.parent_session);
    assert_eq!(a.compacted_into, b.compacted_into);
    assert_eq!(
        serde_json::to_string(&a.api_messages).unwrap(),
        serde_json::to_string(&b.api_messages).unwrap(),
        "message histories must be identical"
    );
}

// ─── 1. json default = legacy, old sessions load unchanged ───────────────────

#[test]
fn json_mode_writes_legacy_file_only_and_round_trips() {
    let tmp = TempDir::new().unwrap();
    let s = session_with_messages(4);
    let mode = save(&tmp, &s, SessionPersistence::Json);
    assert_eq!(mode, SaveMode::FullSnapshot);
    assert!(snapshot_file(&tmp, &s.id).exists());
    assert!(
        !journal_path(tmp.path(), &s.id).exists(),
        "json mode must never create a journal file"
    );
    assert_equivalent(&s, &load(&tmp, &s.id));
}

#[test]
fn session_persistence_default_is_json_and_parses_opt_in() {
    assert_eq!(SessionPersistence::default(), SessionPersistence::Json);
    assert_eq!(
        SessionPersistence::parse("journal"),
        Some(SessionPersistence::Journal)
    );
    assert_eq!(
        SessionPersistence::parse("json"),
        Some(SessionPersistence::Json)
    );
    assert_eq!(SessionPersistence::parse("sqlite"), None);
}

#[test]
fn config_key_session_persistence_is_opt_in() {
    let cfg = agent_core::core::config::load_config_from_str("session_persistence = journal\n");
    assert_eq!(cfg.session_persistence, SessionPersistence::Journal);
    assert!(
        cfg.warnings.is_empty(),
        "known key must not warn: {:?}",
        cfg.warnings
    );

    let default_cfg = agent_core::core::config::load_config_from_str("");
    assert_eq!(default_cfg.session_persistence, SessionPersistence::Json);

    let bad = agent_core::core::config::load_config_from_str("session_persistence = sqlite\n");
    assert_eq!(
        bad.session_persistence,
        SessionPersistence::Json,
        "unknown persistence values keep the safe default"
    );
    assert!(
        bad.warnings
            .iter()
            .any(|w| w.contains("session_persistence")),
        "unparseable value must surface a typed warning: {:?}",
        bad.warnings
    );
}

#[test]
fn old_format_sessions_without_journal_load_unchanged() {
    // A pre-T30 on-disk fixture: no compaction/provenance fields, no journal.
    let tmp = TempDir::new().unwrap();
    let raw = r#"{
        "id": "20250101-000000-old1",
        "title": "legacy session",
        "model": "claude-3-5-sonnet",
        "thinking_level": "medium",
        "system_prompt": null,
        "created_at": "2025-01-01T00:00:00Z",
        "updated_at": "2025-01-01T00:05:00Z",
        "total_input_tokens": 12,
        "total_output_tokens": 34,
        "session_cost": 0.005,
        "api_messages": [{"role":"user","content":"hello"}]
    }"#;
    std::fs::write(tmp.path().join("20250101-000000-old1.json"), raw).unwrap();
    let s = load(&tmp, "20250101-000000-old1");
    assert_eq!(s.title, "legacy session");
    assert_eq!(s.api_messages.len(), 1);
    assert_eq!(s.session_cost, 0.005);
    assert!(s.compaction.is_none());
}

// ─── 2. journal mode: additive, delta-proportional ───────────────────────────

#[test]
fn journal_first_save_writes_snapshot_and_open_record() {
    let tmp = TempDir::new().unwrap();
    let s = session_with_messages(3);
    let mode = save(&tmp, &s, SessionPersistence::Journal);
    assert_eq!(mode, SaveMode::FullSnapshot);
    assert!(snapshot_file(&tmp, &s.id).exists());
    let journal = std::fs::read_to_string(journal_path(tmp.path(), &s.id)).unwrap();
    let lines: Vec<&str> = journal.lines().collect();
    assert_eq!(lines.len(), 1, "fresh journal holds only the open record");
    let open: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(open["k"], "open");
    assert_eq!(open["base"], 3);
    // Snapshot stays the UNCHANGED legacy schema: plain Session JSON.
    let snap: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(snapshot_file(&tmp, &s.id)).unwrap())
            .unwrap();
    assert_eq!(snap["id"], s.id.as_str());
    assert_eq!(snap["api_messages"].as_array().unwrap().len(), 3);
}

#[test]
fn journal_append_is_delta_proportional_and_leaves_snapshot_untouched() {
    let tmp = TempDir::new().unwrap();
    // ~256 KiB of history so history size dwarfs any append overhead.
    let mut s = Session::new("m", "medium", None);
    for i in 0..64 {
        push_msg(&mut s, &format!("{i} {}", "x".repeat(4096)));
    }
    save(&tmp, &s, SessionPersistence::Journal);
    let snap_before = std::fs::read(snapshot_file(&tmp, &s.id)).unwrap();

    push_msg(&mut s, "one small delta message");
    let receipt = save_session_in_dir(tmp.path(), &s, SessionPersistence::Journal).unwrap();
    assert_eq!(receipt.mode, SaveMode::Append { messages: 1 });
    assert!(
        receipt.bytes_written < 16 * 1024,
        "appending one small message to a {}-byte history must write a \
         delta-proportional amount, wrote {}",
        snap_before.len(),
        receipt.bytes_written
    );
    assert_eq!(
        std::fs::read(snapshot_file(&tmp, &s.id)).unwrap(),
        snap_before,
        "an append-mode save must not rewrite the snapshot"
    );
    assert_equivalent(&s, &load(&tmp, &s.id));
}

#[test]
fn journal_replays_messages_and_metadata_on_load() {
    let tmp = TempDir::new().unwrap();
    let mut s = session_with_messages(2);
    save(&tmp, &s, SessionPersistence::Journal);

    push_msg(&mut s, "third message");
    push_msg(&mut s, "fourth message");
    s.title = "updated title".into();
    s.total_input_tokens = 111;
    s.total_output_tokens = 222;
    s.session_cost = 0.42;
    s.abort_context = Some("aborted mid-tool".into());
    s.updated_at = chrono::Utc::now();
    save(&tmp, &s, SessionPersistence::Journal);

    let loaded = load(&tmp, &s.id);
    assert_eq!(loaded.api_messages.len(), 4);
    assert_equivalent(&s, &loaded);
}

#[test]
fn journal_meta_tail_reports_fresh_metadata_without_full_read() {
    let tmp = TempDir::new().unwrap();
    let mut s = session_with_messages(2);
    save(&tmp, &s, SessionPersistence::Journal);
    assert!(
        journal_meta_tail(tmp.path(), &s.id).is_none(),
        "a fresh journal has no meta record yet"
    );
    push_msg(&mut s, "delta");
    s.session_cost = 1.25;
    s.updated_at = chrono::Utc::now();
    save(&tmp, &s, SessionPersistence::Journal);
    let tail = journal_meta_tail(tmp.path(), &s.id).expect("meta tail after an append");
    assert_eq!(tail.session_cost, 1.25);
    assert_eq!(tail.updated_at, s.updated_at);
}

// ─── 3. recovery: torn tails, stale journals, gaps, rewrites ─────────────────

#[test]
fn torn_journal_tail_recovers_last_consistent_state() {
    let tmp = TempDir::new().unwrap();
    let mut s = session_with_messages(2);
    save(&tmp, &s, SessionPersistence::Journal);
    push_msg(&mut s, "durable third");
    s.updated_at = chrono::Utc::now();
    save(&tmp, &s, SessionPersistence::Journal);

    // Kill-during-append: truncate the journal mid-way through its last line.
    let jpath = journal_path(tmp.path(), &s.id);
    let bytes = std::fs::read(&jpath).unwrap();
    let torn = &bytes[..bytes.len() - 7];
    std::fs::write(&jpath, torn).unwrap();

    let recovered = load(&tmp, &s.id);
    assert!(
        recovered.api_messages.len() >= 2,
        "recovery must never lose snapshot state"
    );
    assert!(
        recovered.api_messages.len() <= 3,
        "recovery must never invent state"
    );
    // And the NEXT save self-heals to full consistency again.
    save(&tmp, &s, SessionPersistence::Journal);
    assert_equivalent(&s, &load(&tmp, &s.id));
}

#[test]
fn stale_journal_records_below_snapshot_are_idempotent() {
    // Crash window: snapshot advanced, journal reset never happened. Every
    // journal record refers to state ALREADY inside the snapshot.
    let tmp = TempDir::new().unwrap();
    let s = session_with_messages(3);
    save(&tmp, &s, SessionPersistence::Json); // snapshot with 3 messages
    let stale = format!(
        "{}\n{}\n{}\n",
        json!({"v":1,"k":"open","base":1}),
        json!({"v":1,"k":"msg","i":1,"m":{"role":"assistant","content":"OLD DUPLICATE"}}),
        json!({"v":1,"k":"meta","meta":{
            "id": s.id, "title": "STALE TITLE", "model": s.model,
            "thinking_level": s.thinking_level, "system_prompt": null,
            "created_at": s.created_at.to_rfc3339(),
            "updated_at": "2020-01-01T00:00:00Z",
            "total_input_tokens": 0, "total_output_tokens": 0,
            "session_cost": 999.0
        }}),
    );
    std::fs::write(journal_path(tmp.path(), &s.id), stale).unwrap();

    let loaded = load(&tmp, &s.id);
    assert_eq!(loaded.api_messages.len(), 3, "no duplicate replay");
    assert_eq!(loaded.title, s.title, "older meta must not regress state");
    assert_eq!(loaded.session_cost, s.session_cost);
}

#[test]
fn journal_gap_stops_replay_at_consistent_prefix() {
    let tmp = TempDir::new().unwrap();
    let s = session_with_messages(2);
    save(&tmp, &s, SessionPersistence::Json);
    // Journal claims message index 3 but index 2 is missing: inconsistent
    // suffix must be ignored, not partially applied.
    let gap = format!(
        "{}\n{}\n",
        json!({"v":1,"k":"open","base":2}),
        json!({"v":1,"k":"msg","i":3,"m":{"role":"user","content":"orphan"}}),
    );
    std::fs::write(journal_path(tmp.path(), &s.id), gap).unwrap();
    let loaded = load(&tmp, &s.id);
    assert_eq!(loaded.api_messages.len(), 2);
}

#[test]
fn history_shrink_triggers_full_resnapshot() {
    // In-place compaction replaces the history with a shorter one — the
    // journal cannot express that; the save must resnapshot atomically.
    let tmp = TempDir::new().unwrap();
    let mut s = session_with_messages(6);
    save(&tmp, &s, SessionPersistence::Journal);
    s.api_messages.truncate(2);
    s.updated_at = chrono::Utc::now();
    let mode = save(&tmp, &s, SessionPersistence::Journal);
    assert_eq!(mode, SaveMode::FullSnapshot);
    assert_equivalent(&s, &load(&tmp, &s.id));
}

#[test]
fn edited_durable_tail_triggers_full_resnapshot() {
    let tmp = TempDir::new().unwrap();
    let mut s = session_with_messages(2);
    save(&tmp, &s, SessionPersistence::Journal);
    push_msg(&mut s, "will be edited");
    save(&tmp, &s, SessionPersistence::Journal);
    // Rewrite the last durable message in memory (append-only assumption
    // tripwire) — the journal tail no longer matches.
    *s.api_messages.last_mut().unwrap() =
        Arc::new(json!({"role":"assistant","content":"EDITED BODY"}));
    s.updated_at = chrono::Utc::now();
    let mode = save(&tmp, &s, SessionPersistence::Journal);
    assert_eq!(mode, SaveMode::FullSnapshot, "edited tail must resnapshot");
    assert_equivalent(&s, &load(&tmp, &s.id));
}

// ─── 4. periodic snapshots + rollback ────────────────────────────────────────

#[test]
fn snapshot_due_threshold_is_bounded_below_and_proportional() {
    assert!(!snapshot_due(0, 0));
    assert!(!snapshot_due(JOURNAL_SNAPSHOT_MIN_BYTES - 1, 0));
    assert!(snapshot_due(JOURNAL_SNAPSHOT_MIN_BYTES, 0));
    // Large snapshots stretch the threshold proportionally (bounded write
    // amplification), so a 100 MiB session does not resnapshot every 256 KiB.
    let snap = 100 * 1024 * 1024_u64;
    assert!(!snapshot_due(snap / 4 - 1, snap));
    assert!(snapshot_due(snap / 4, snap));
}

#[test]
fn oversized_journal_compacts_into_fresh_snapshot() {
    let tmp = TempDir::new().unwrap();
    let mut s = session_with_messages(1);
    save(&tmp, &s, SessionPersistence::Journal);
    // Push well past the 256 KiB floor in a handful of appends.
    let mut saw_compaction = false;
    for i in 0..6 {
        push_msg(&mut s, &format!("{i} {}", "y".repeat(64 * 1024)));
        s.updated_at = chrono::Utc::now();
        let receipt = save_session_in_dir(tmp.path(), &s, SessionPersistence::Journal).unwrap();
        if receipt.mode == SaveMode::FullSnapshot {
            saw_compaction = true;
        }
    }
    assert!(saw_compaction, "journal growth must trigger a snapshot");
    let journal_len = std::fs::metadata(journal_path(tmp.path(), &s.id))
        .unwrap()
        .len();
    assert!(
        journal_len < JOURNAL_SNAPSHOT_MIN_BYTES,
        "post-compaction journal must be reset, still {journal_len} bytes"
    );
    assert_equivalent(&s, &load(&tmp, &s.id));
}

#[test]
fn json_mode_save_folds_and_deletes_the_journal() {
    // Rollback: opting back out restores pure-legacy state on the next save.
    let tmp = TempDir::new().unwrap();
    let mut s = session_with_messages(2);
    save(&tmp, &s, SessionPersistence::Journal);
    push_msg(&mut s, "journal-only message");
    s.updated_at = chrono::Utc::now();
    save(&tmp, &s, SessionPersistence::Journal);
    assert!(journal_path(tmp.path(), &s.id).exists());

    let mode = save(&tmp, &s, SessionPersistence::Json);
    assert_eq!(mode, SaveMode::FullSnapshot);
    assert!(
        !journal_path(tmp.path(), &s.id).exists(),
        "a json-mode save must fold and delete the journal"
    );
    assert_equivalent(&s, &load(&tmp, &s.id));
}

#[test]
fn manual_journal_deletion_is_always_safe() {
    let tmp = TempDir::new().unwrap();
    let mut s = session_with_messages(2);
    save(&tmp, &s, SessionPersistence::Journal);
    push_msg(&mut s, "tail beyond snapshot");
    save(&tmp, &s, SessionPersistence::Journal);
    std::fs::remove_file(journal_path(tmp.path(), &s.id)).unwrap();
    let loaded = load(&tmp, &s.id);
    assert_eq!(
        loaded.api_messages.len(),
        2,
        "the snapshot alone is a valid consistent (older) session"
    );
}

// ─── 5. deletion + retention pair the snapshot with its journal ──────────────

#[test]
fn delete_removes_snapshot_and_journal_together() {
    let tmp = TempDir::new().unwrap();
    let s = session_with_messages(2);
    save(&tmp, &s, SessionPersistence::Journal);
    delete_session_files_in_dir(tmp.path(), &s.id).unwrap();
    assert!(!snapshot_file(&tmp, &s.id).exists());
    assert!(!journal_path(tmp.path(), &s.id).exists());
    // Idempotent (compaction rollback calls this on already-clean state).
    delete_session_files_in_dir(tmp.path(), &s.id).unwrap();
}

struct RetentionHarness {
    _tmp: TempDir,
    roots: RetentionRoots,
}

fn retention_harness() -> RetentionHarness {
    let tmp = TempDir::new().unwrap();
    let roots = RetentionRoots {
        config_dir: tmp.path().join("config"),
        base_dir: tmp.path().join("base"),
        cache_dir: tmp.path().join("cache"),
    };
    std::fs::create_dir_all(roots.config_dir.join("sessions")).unwrap();
    std::fs::create_dir_all(roots.config_dir.join("chains")).unwrap();
    std::fs::create_dir_all(roots.base_dir.join("memory")).unwrap();
    std::fs::create_dir_all(&roots.cache_dir).unwrap();
    RetentionHarness { _tmp: tmp, roots }
}

fn aged_session(dir: &std::path::Path, id: &str, updated_at: &str, with_journal: bool) {
    let mut s = session_with_messages(2);
    s.id = id.to_string();
    s.updated_at = chrono::DateTime::parse_from_rfc3339(updated_at)
        .unwrap()
        .with_timezone(&chrono::Utc);
    let mode = if with_journal {
        SessionPersistence::Journal
    } else {
        SessionPersistence::Json
    };
    save_session_in_dir(dir, &s, mode).unwrap();
}

#[test]
fn retention_age_sweep_deletes_session_and_journal_as_one_artifact() {
    let h = retention_harness();
    let sessions = h.roots.config_dir.join("sessions");
    aged_session(
        &sessions,
        "20200101-000000-dead",
        "2020-01-01T00:00:00Z",
        true,
    );
    aged_session(
        &sessions,
        "20990101-000000-live",
        "2099-01-01T00:00:00Z",
        true,
    );

    let now_ms = 1_800_000_000_000; // 2027 — the 2020 session is long past 30d
    let policy = RetentionPolicy {
        max_age_days: Some(30),
        max_disk_bytes: None,
    };
    sweep_at(&h.roots, &policy, now_ms).unwrap();

    assert!(!sessions.join("20200101-000000-dead.json").exists());
    assert!(
        !journal_path(&sessions, "20200101-000000-dead").exists(),
        "the journal must be swept with its session, never orphaned"
    );
    assert!(sessions.join("20990101-000000-live.json").exists());
    assert!(journal_path(&sessions, "20990101-000000-live").exists());
}

#[test]
fn retention_chain_head_protects_journal_too() {
    let h = retention_harness();
    let sessions = h.roots.config_dir.join("sessions");
    aged_session(
        &sessions,
        "20200101-000000-head",
        "2020-01-01T00:00:00Z",
        true,
    );
    std::fs::write(
        h.roots.config_dir.join("chains").join("main.json"),
        json!({"head": "20200101-000000-head"}).to_string(),
    )
    .unwrap();

    let policy = RetentionPolicy {
        max_age_days: Some(30),
        max_disk_bytes: None,
    };
    sweep_at(&h.roots, &policy, 1_800_000_000_000).unwrap();

    assert!(sessions.join("20200101-000000-head.json").exists());
    assert!(
        journal_path(&sessions, "20200101-000000-head").exists(),
        "a protected chain head keeps its journal"
    );
}

#[test]
fn retention_sweeps_orphan_journals() {
    let h = retention_harness();
    let sessions = h.roots.config_dir.join("sessions");
    aged_session(
        &sessions,
        "20200101-000000-orfn",
        "2020-01-01T00:00:00Z",
        true,
    );
    // The snapshot vanished out-of-band; the journal alone is unusable.
    std::fs::remove_file(sessions.join("20200101-000000-orfn.json")).unwrap();

    let policy = RetentionPolicy {
        max_age_days: Some(30),
        max_disk_bytes: None,
    };
    sweep_at(&h.roots, &policy, 1_800_000_000_000).unwrap();
    assert!(
        !journal_path(&sessions, "20200101-000000-orfn").exists(),
        "orphan journals are swept"
    );
}

// ─── 6. benchmarks (resource-capped, --ignored) ──────────────────────────────
//
// cargo test -p synaps-core --test session_journal -- --ignored --test-threads=1
//
// Machine-readable output: one `BENCH session_save …` line per scale.

fn session_of_bytes(target_bytes: usize) -> Session {
    let mut s = Session::new("claude-sonnet-4-6", "medium", None);
    let body = "z".repeat(8 * 1024);
    let mut approx = serde_json::to_string(&s).unwrap().len();
    while approx < target_bytes {
        let i = s.api_messages.len();
        let msg = json!({"role": if i % 2 == 0 {"user"} else {"assistant"},
                         "content": format!("{i} {body}")});
        approx += msg.to_string().len() + 1; // + separator
        s.api_messages.push(Arc::new(msg));
    }
    s
}

fn bench_save(hist_mib: usize) {
    let tmp = TempDir::new().unwrap();
    let mut s = session_of_bytes(hist_mib * 1024 * 1024);

    // Legacy: every save rewrites the full history.
    let t = std::time::Instant::now();
    let legacy = save_session_in_dir(tmp.path(), &s, SessionPersistence::Json).unwrap();
    let legacy_ms = t.elapsed().as_millis();

    // Journal steady state: first save snapshots, then appends are deltas.
    let tmp2 = TempDir::new().unwrap();
    save_session_in_dir(tmp2.path(), &s, SessionPersistence::Journal).unwrap();
    push_msg(&mut s, "steady-state delta message");
    s.updated_at = chrono::Utc::now();
    let t = std::time::Instant::now();
    let append = save_session_in_dir(tmp2.path(), &s, SessionPersistence::Journal).unwrap();
    let append_ms = t.elapsed().as_millis();
    assert_eq!(append.mode, SaveMode::Append { messages: 1 });
    assert!(
        append.bytes_written < 64 * 1024,
        "journal append must stay delta-bounded at {hist_mib} MiB"
    );

    // Documented recovery: kill during append, then load.
    let jpath = journal_path(tmp2.path(), &s.id);
    let bytes = std::fs::read(&jpath).unwrap();
    std::fs::write(&jpath, &bytes[..bytes.len().saturating_sub(9)]).unwrap();
    let t = std::time::Instant::now();
    let recovered = load_session_in_dir(tmp2.path(), &s.id).unwrap();
    let recover_ms = t.elapsed().as_millis();
    assert!(!recovered.api_messages.is_empty());

    println!(
        "BENCH session_save hist_mib={hist_mib} legacy_save_ms={legacy_ms} \
         legacy_bytes={} journal_append_ms={append_ms} journal_append_bytes={} \
         recover_load_ms={recover_ms}",
        legacy.bytes_written, append.bytes_written
    );
}

#[test]
#[ignore = "benchmark — run explicitly, resource-capped"]
fn bench_save_1mib_history() {
    bench_save(1);
}

#[test]
#[ignore = "benchmark — run explicitly, resource-capped"]
fn bench_save_10mib_history() {
    bench_save(10);
}

#[test]
#[ignore = "benchmark — run explicitly, resource-capped"]
fn bench_save_100mib_history() {
    bench_save(100);
}
