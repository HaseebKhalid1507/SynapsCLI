//! Phase 5 / Task 34 — disclosure classes and unified retention (spec §9.7).
//!
//! - Every disclosure class is enforced at the ONE model-visibility gate
//!   (sentinel test per class; consent and redaction fail CLOSED).
//! - Unified retention spans sessions (incl. compaction parents), memory,
//!   memory indexes, traces, and logs: max age + disk budget, headless
//!   inspect/export/forget, and chain integrity — a sweep never leaves a
//!   named chain pointing at a deleted session.

use agent_core::disclosure::{gate_for_model, may_persist, DisclosureClass, ModelVisibility};
use agent_core::retention::{
    export, forget, inspect, sweep_at, RetentionDomain, RetentionPolicy, RetentionRoots,
};
use tempfile::TempDir;

const SENTINEL: &str = "DISCLOSURE-SENTINEL-77aa";

// ─── disclosure gate: one sentinel test per class ────────────────────────────

#[test]
fn model_visible_class_passes_content_through() {
    match gate_for_model(DisclosureClass::ModelVisible, SENTINEL, false, None) {
        ModelVisibility::Visible(text) => assert_eq!(text, SENTINEL),
        withheld => panic!("baseline class must be visible, got {withheld:?}"),
    }
}

#[test]
fn local_only_class_never_reaches_model_context() {
    match gate_for_model(DisclosureClass::LocalOnly, SENTINEL, true, None) {
        ModelVisibility::Withheld(reason) => {
            assert!(reason.contains("local_only"), "typed reason: {reason}")
        }
        visible => panic!("local_only must be withheld even with consent, got {visible:?}"),
    }
}

#[test]
fn after_redaction_class_requires_a_redactor_and_applies_it() {
    // No redactor configured → fail CLOSED.
    match gate_for_model(
        DisclosureClass::ModelVisibleAfterRedaction,
        SENTINEL,
        true,
        None,
    ) {
        ModelVisibility::Withheld(reason) => assert!(reason.contains("redact")),
        visible => panic!("missing redactor must withhold, got {visible:?}"),
    }
    // With a redactor: the redacted form (and only it) is visible.
    let redactor = |text: &str| text.replace(SENTINEL, "[REDACTED]");
    match gate_for_model(
        DisclosureClass::ModelVisibleAfterRedaction,
        SENTINEL,
        false,
        Some(&redactor),
    ) {
        ModelVisibility::Visible(text) => {
            assert!(!text.contains(SENTINEL));
            assert!(text.contains("[REDACTED]"));
        }
        withheld => panic!("redacted content should be visible, got {withheld:?}"),
    }
}

#[test]
fn after_consent_class_is_withheld_until_consent() {
    match gate_for_model(
        DisclosureClass::ModelVisibleAfterConsent,
        SENTINEL,
        false,
        None,
    ) {
        ModelVisibility::Withheld(reason) => assert!(reason.contains("consent")),
        visible => panic!("no consent must withhold, got {visible:?}"),
    }
    match gate_for_model(
        DisclosureClass::ModelVisibleAfterConsent,
        SENTINEL,
        true,
        None,
    ) {
        ModelVisibility::Visible(text) => assert_eq!(text, SENTINEL),
        withheld => panic!("explicit consent must reveal, got {withheld:?}"),
    }
}

#[test]
fn persist_never_transmit_class_is_withheld_from_model_context() {
    match gate_for_model(DisclosureClass::PersistNeverTransmit, SENTINEL, true, None) {
        ModelVisibility::Withheld(reason) => assert!(reason.contains("never_transmit")),
        visible => panic!("persist-never-transmit must not transmit, got {visible:?}"),
    }
    assert!(may_persist(DisclosureClass::PersistNeverTransmit));
}

#[test]
fn never_persist_class_is_visible_but_unpersistable() {
    match gate_for_model(DisclosureClass::NeverPersist, SENTINEL, false, None) {
        ModelVisibility::Visible(text) => assert_eq!(text, SENTINEL),
        withheld => panic!("never-persist governs persistence, not visibility: {withheld:?}"),
    }
    assert!(!may_persist(DisclosureClass::NeverPersist));
    for class in [
        DisclosureClass::ModelVisible,
        DisclosureClass::LocalOnly,
        DisclosureClass::ModelVisibleAfterRedaction,
        DisclosureClass::ModelVisibleAfterConsent,
        DisclosureClass::PersistNeverTransmit,
    ] {
        assert!(may_persist(class), "{class:?} may persist");
    }
}

// ─── retention harness ───────────────────────────────────────────────────────

struct Harness {
    _tmp: TempDir,
    roots: RetentionRoots,
}

const DAY_MS: u64 = 86_400_000;

fn harness() -> Harness {
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
    Harness { _tmp: tmp, roots }
}

fn write_session(roots: &RetentionRoots, id: &str, updated_ms: u64, compacted_into: Option<&str>) {
    let updated = chrono::DateTime::from_timestamp_millis(updated_ms as i64)
        .unwrap()
        .to_rfc3339();
    let mut session = serde_json::json!({
        "id": id,
        "title": format!("session {id}"),
        "model": "claude-sonnet-4-6",
        "thinking_level": "high",
        "system_prompt": null,
        "created_at": updated,
        "updated_at": updated,
        "total_input_tokens": 0,
        "total_output_tokens": 0,
        "session_cost": 0.0,
        "api_messages": [{"role": "user", "content": "hi"}]
    });
    if let Some(succ) = compacted_into {
        session["compacted_into"] = serde_json::json!(succ);
    }
    std::fs::write(
        roots.config_dir.join("sessions").join(format!("{id}.json")),
        serde_json::to_string(&session).unwrap(),
    )
    .unwrap();
}

fn write_chain(roots: &RetentionRoots, name: &str, head: &str) {
    std::fs::write(
        roots.config_dir.join("chains").join(format!("{name}.json")),
        serde_json::json!({"head": head}).to_string(),
    )
    .unwrap();
}

fn session_exists(roots: &RetentionRoots, id: &str) -> bool {
    roots
        .config_dir
        .join("sessions")
        .join(format!("{id}.json"))
        .exists()
}

// ─── retention: age sweep + chain integrity (failing-first core case) ────────

#[test]
fn age_sweep_never_leaves_named_chains_dangling() {
    let h = harness();
    let now = 100 * DAY_MS;
    // Both sessions are FAR past the age budget; one is a named chain head.
    write_session(&h.roots, "old-unreferenced", 10 * DAY_MS, None);
    write_session(&h.roots, "old-chain-head", 10 * DAY_MS, None);
    write_chain(&h.roots, "mainline", "old-chain-head");

    let outcome = sweep_at(
        &h.roots,
        &RetentionPolicy {
            max_age_days: Some(30),
            max_disk_bytes: None,
        },
        now,
    )
    .unwrap();

    assert!(!session_exists(&h.roots, "old-unreferenced"), "aged out");
    assert!(
        session_exists(&h.roots, "old-chain-head"),
        "a named chain head must survive every sweep"
    );
    assert_eq!(outcome.protected_chain_heads, 1);

    // No chain points at a missing session file.
    for entry in std::fs::read_dir(h.roots.config_dir.join("chains")).unwrap() {
        let raw = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        let chain: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let head = chain["head"].as_str().unwrap();
        assert!(
            session_exists(&h.roots, head),
            "chain must never dangle: {head}"
        );
    }
}

#[test]
fn age_sweep_spans_compaction_parents_memory_traces_and_logs() {
    let h = harness();
    let now = 100 * DAY_MS;

    // Compaction parent (old, forward-linked) ages out; recent successor stays.
    write_session(&h.roots, "old-parent", 10 * DAY_MS, Some("fresh-successor"));
    write_session(&h.roots, "fresh-successor", 99 * DAY_MS, None);

    // Memory: an expired record (per-record retention), an old record under
    // the global age, a fresh record, and a tombstoned one.
    let memory = h.roots.base_dir.join("memory").join("project-p11.jsonl");
    let lines = [
        format!(
            r#"{{"namespace":"project-p11","timestamp_ms":{},"content":"expired by record retention","tags":[],"id":"mem-exp","project":"p11","retention":{{"max_age_days":5}}}}"#,
            80 * DAY_MS
        ),
        format!(
            r#"{{"namespace":"project-p11","timestamp_ms":{},"content":"expired by global age","tags":[],"id":"mem-old","project":"p11"}}"#,
            10 * DAY_MS
        ),
        format!(
            r#"{{"namespace":"project-p11","timestamp_ms":{},"content":"fresh survivor","tags":[],"id":"mem-new","project":"p11"}}"#,
            99 * DAY_MS
        ),
        format!(
            r#"{{"namespace":"project-p11","timestamp_ms":{},"content":"tombstoned body","tags":[],"id":"mem-dead","project":"p11"}}"#,
            99 * DAY_MS
        ),
        format!(
            r#"{{"tombstone":"mem-dead","timestamp_ms":{}}}"#,
            99 * DAY_MS
        ),
    ];
    std::fs::write(&memory, lines.join("\n") + "\n").unwrap();
    // A stale derived index dir for that namespace must be dropped too.
    let index_dir = h.roots.base_dir.join("memory/index/project-p11");
    std::fs::create_dir_all(&index_dir).unwrap();
    std::fs::write(index_dir.join("manifest.json"), "{}").unwrap();

    // Traces and logs (mtime-aged: everything is "now"-written, so use a
    // future now to age them past the budget).
    std::fs::write(h.roots.cache_dir.join("request-trace.jsonl"), "t\n").unwrap();
    std::fs::write(h.roots.config_dir.join("synaps.log.2020-01-01"), "l\n").unwrap();

    // Traces/logs age by REAL file mtime, so the sweep clock must sit past
    // the real clock; the 1970-epoch embedded session/memory stamps are
    // ancient relative to it either way.
    let sweep_clock = agent_core::epoch_millis() + 40 * DAY_MS;
    let outcome = sweep_at(
        &h.roots,
        &RetentionPolicy {
            max_age_days: Some(30),
            max_disk_bytes: None,
        },
        sweep_clock,
    )
    .unwrap();

    assert!(
        !session_exists(&h.roots, "old-parent"),
        "compaction parent swept"
    );
    // Sessions age by embedded updated_at; both 1970-stamped sessions are
    // far past the 30-day budget at the real-clock sweep time.
    assert!(!session_exists(&h.roots, "fresh-successor"));

    let rewritten = std::fs::read_to_string(&memory).unwrap();
    assert!(!rewritten.contains("expired by record retention"));
    assert!(!rewritten.contains("expired by global age"));
    assert!(
        !rewritten.contains("tombstoned body"),
        "tombstones purge physically"
    );
    assert!(!rewritten.contains("mem-dead"));
    // mem-new is 99d old at sweep time (now+40d) minus ts 99d → 41d → also
    // aged out under the 30-day budget.
    assert!(!rewritten.contains("fresh survivor"));
    assert!(outcome.memory_records_dropped >= 4);

    assert!(
        !index_dir.exists(),
        "derived index dropped with its namespace"
    );
    assert!(!h.roots.cache_dir.join("request-trace.jsonl").exists());
    assert!(!h.roots.config_dir.join("synaps.log.2020-01-01").exists());
}

#[test]
fn memory_survivors_and_recent_artifacts_are_kept() {
    let h = harness();
    let now = 100 * DAY_MS;
    write_session(&h.roots, "recent", 99 * DAY_MS, None);
    let memory = h.roots.base_dir.join("memory").join("project-p22.jsonl");
    std::fs::write(
        &memory,
        format!(
            "{}\n",
            format_args!(
                r#"{{"namespace":"project-p22","timestamp_ms":{},"content":"stays","tags":[],"id":"mem-stay","project":"p22"}}"#,
                99 * DAY_MS
            )
        ),
    )
    .unwrap();
    std::fs::write(h.roots.cache_dir.join("request-trace.jsonl"), "t\n").unwrap();

    sweep_at(
        &h.roots,
        &RetentionPolicy {
            max_age_days: Some(30),
            max_disk_bytes: None,
        },
        now,
    )
    .unwrap();

    assert!(session_exists(&h.roots, "recent"));
    assert!(std::fs::read_to_string(&memory).unwrap().contains("stays"));
    assert!(h.roots.cache_dir.join("request-trace.jsonl").exists());
}

#[test]
fn disk_budget_deletes_oldest_first_but_never_chain_heads() {
    let h = harness();
    let now = 100 * DAY_MS;
    write_session(&h.roots, "oldest", 10 * DAY_MS, None);
    write_session(&h.roots, "middle", 50 * DAY_MS, None);
    write_session(&h.roots, "newest", 99 * DAY_MS, None);
    write_chain(&h.roots, "pin", "oldest");

    // Budget just below the current total: freeing ONE unprotected file
    // (the oldest, since the pinned head is immune) satisfies it.
    let total = inspect(&h.roots).unwrap().total_bytes;
    let outcome = sweep_at(
        &h.roots,
        &RetentionPolicy {
            max_age_days: None,
            max_disk_bytes: Some(total - 10),
        },
        now,
    )
    .unwrap();

    assert!(
        session_exists(&h.roots, "oldest"),
        "chain head immune to budget"
    );
    assert!(
        !session_exists(&h.roots, "middle"),
        "oldest unprotected goes first"
    );
    assert!(session_exists(&h.roots, "newest"));
    assert!(outcome.freed_bytes > 0);
}

// ─── headless inspect / export / forget ──────────────────────────────────────

#[test]
fn inspect_reports_per_domain_files_and_bytes() {
    let h = harness();
    write_session(&h.roots, "s1", 50 * DAY_MS, None);
    std::fs::write(h.roots.base_dir.join("memory/ns.jsonl"), "x\n").unwrap();
    std::fs::write(h.roots.cache_dir.join("request-trace.jsonl"), "t\n").unwrap();
    std::fs::write(h.roots.config_dir.join("synaps.log.2026-01-01"), "l\n").unwrap();

    let report = inspect(&h.roots).unwrap();
    let count = |domain: RetentionDomain| {
        report
            .domains
            .iter()
            .find(|d| d.domain == domain)
            .map(|d| d.files)
            .unwrap_or(0)
    };
    assert_eq!(count(RetentionDomain::Sessions), 1);
    assert_eq!(count(RetentionDomain::Memory), 1);
    assert_eq!(count(RetentionDomain::Traces), 1);
    assert_eq!(count(RetentionDomain::Logs), 1);
    assert!(report.total_bytes > 0);
}

#[test]
fn export_copies_artifacts_headlessly() {
    let h = harness();
    write_session(&h.roots, "s1", 50 * DAY_MS, None);
    std::fs::write(h.roots.base_dir.join("memory/ns.jsonl"), "m\n").unwrap();

    let dest = h.roots.base_dir.join("export-out");
    let summary = export(&h.roots, &dest).unwrap();
    assert!(summary.files >= 2);
    assert!(dest.join("sessions/s1.json").exists());
    assert!(dest.join("memory/ns.jsonl").exists());
}

#[test]
fn forget_deletes_by_domain_and_protects_chain_heads() {
    let h = harness();
    write_session(&h.roots, "expendable", 50 * DAY_MS, None);
    write_session(&h.roots, "pinned", 50 * DAY_MS, None);
    write_chain(&h.roots, "keep", "pinned");

    forget(&h.roots, RetentionDomain::Sessions, "expendable").unwrap();
    assert!(!session_exists(&h.roots, "expendable"));

    let err = forget(&h.roots, RetentionDomain::Sessions, "pinned").unwrap_err();
    assert!(
        err.to_string().contains("chain"),
        "forgetting a chain head must fail closed: {err}"
    );
    assert!(session_exists(&h.roots, "pinned"));

    std::fs::write(h.roots.cache_dir.join("request-trace.jsonl"), "t\n").unwrap();
    forget(&h.roots, RetentionDomain::Traces, "request-trace.jsonl").unwrap();
    assert!(!h.roots.cache_dir.join("request-trace.jsonl").exists());
}

// ─── CP-13 fix1: forget hardening ────────────────────────────────────────────

#[test]
fn forget_rejects_path_traversal_ids_for_every_domain() {
    let h = harness();
    let victim = h.roots.config_dir.join("victim.json");
    std::fs::write(&victim, "precious").unwrap();

    for id in ["../victim", "..", "a/b", "a\\b", "/etc/passwd", ""] {
        for domain in [
            RetentionDomain::Sessions,
            RetentionDomain::Traces,
            RetentionDomain::Logs,
            RetentionDomain::MemoryIndex,
        ] {
            let err = forget(&h.roots, domain, id).unwrap_err();
            assert!(
                err.to_string().contains("invalid"),
                "{domain:?} id {id:?} must be rejected as invalid, got: {err}"
            );
        }
        // Memory ids embed a namespace part that must be sanitized too.
        let err = forget(&h.roots, RetentionDomain::Memory, &format!("{id}:mem-x")).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("invalid") || msg.contains("namespace"),
            "memory namespace part {id:?} must be rejected, got: {msg}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(&victim).unwrap(),
        "precious",
        "traversal ids must never touch files outside the domain directory"
    );
}

#[test]
fn memory_forget_requires_exact_live_record_ids() {
    let h = harness();
    let memory = h.roots.base_dir.join("memory").join("project-p33.jsonl");
    let lines = [
        r#"{"namespace":"project-p33","timestamp_ms":100,"content":"a","tags":[],"id":"mem-abc","project":"p33"}"#,
        r#"{"namespace":"project-p33","timestamp_ms":200,"content":"b","tags":[],"id":"mem-abcdef","project":"p33"}"#,
    ];
    std::fs::write(&memory, lines.join("\n") + "\n").unwrap();

    // A substring/prefix of a live id is NOT a live id.
    let err = forget(&h.roots, RetentionDomain::Memory, "project-p33:mem-ab").unwrap_err();
    assert!(err.to_string().contains("not present"), "{err}");

    // Exact id tombstones exactly one record.
    forget(&h.roots, RetentionDomain::Memory, "project-p33:mem-abc").unwrap();
    let raw = std::fs::read_to_string(&memory).unwrap();
    assert!(raw.contains(r#""tombstone":"mem-abc""#));
    assert!(!raw.contains(r#""tombstone":"mem-abcdef""#));

    // Already-tombstoned ids are no longer live — forgetting again fails.
    let err = forget(&h.roots, RetentionDomain::Memory, "project-p33:mem-abc").unwrap_err();
    assert!(err.to_string().contains("not present"), "{err}");
}

// ─── CP-13 fix1: budget enforcement + fail-closed chains ─────────────────────

#[test]
fn disk_budget_evicts_derived_indexes_and_oldest_memory_records() {
    let h = harness();
    let now = 100 * DAY_MS;

    // Memory-dominated corpus: 50 old records + 2 fresh ones.
    let memory = h.roots.base_dir.join("memory").join("project-p44.jsonl");
    let mut lines: Vec<String> = (0..50)
        .map(|i| {
            format!(
                r#"{{"namespace":"project-p44","timestamp_ms":{},"content":"old bulk record {} {}","tags":[],"id":"mem-old{}","project":"p44"}}"#,
                DAY_MS + i,
                i,
                "x".repeat(200),
                i
            )
        })
        .collect();
    lines.push(format!(
        r#"{{"namespace":"project-p44","timestamp_ms":{},"content":"fresh keep one","tags":[],"id":"mem-keep1","project":"p44"}}"#,
        99 * DAY_MS
    ));
    lines.push(format!(
        r#"{{"namespace":"project-p44","timestamp_ms":{},"content":"fresh keep two","tags":[],"id":"mem-keep2","project":"p44"}}"#,
        99 * DAY_MS + 1
    ));
    std::fs::write(&memory, lines.join("\n") + "\n").unwrap();

    // A fat derived index dir — free win, evicted before any data.
    let index_dir = h.roots.base_dir.join("memory/index/project-p44");
    std::fs::create_dir_all(&index_dir).unwrap();
    std::fs::write(index_dir.join("seg-000000.jsonl"), "z".repeat(4_000)).unwrap();

    // Budget: room for roughly the two fresh records only.
    let outcome = sweep_at(
        &h.roots,
        &RetentionPolicy {
            max_age_days: None,
            max_disk_bytes: Some(700),
        },
        now,
    )
    .unwrap();

    assert!(
        !index_dir.exists(),
        "derived index evicted first (no data loss)"
    );
    let rewritten = std::fs::read_to_string(&memory).unwrap();
    assert!(rewritten.contains("fresh keep one") && rewritten.contains("fresh keep two"));
    assert!(
        !rewritten.contains("mem-old0"),
        "oldest records evicted first"
    );
    assert!(outcome.memory_records_dropped >= 40);

    // Final state actually satisfies the budget.
    let total = inspect(&h.roots).unwrap().total_bytes;
    assert!(total <= 700, "final total {total} must be <= budget");
}

#[test]
fn unsatisfiable_disk_budget_returns_a_typed_unmet_error() {
    let h = harness();
    let now = 100 * DAY_MS;
    write_session(&h.roots, "pinned", 10 * DAY_MS, None);
    write_chain(&h.roots, "keep", "pinned");

    let err = sweep_at(
        &h.roots,
        &RetentionPolicy {
            max_age_days: None,
            max_disk_bytes: Some(10),
        },
        now,
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("budget") && msg.contains("protected"),
        "unmet budget must surface typed, naming the protection: {msg}"
    );
    assert!(
        session_exists(&h.roots, "pinned"),
        "protected head untouched"
    );
}

#[test]
fn malformed_chain_files_fail_closed_without_destructive_sweep() {
    let h = harness();
    let now = 100 * DAY_MS;
    write_session(&h.roots, "old-session", 10 * DAY_MS, None);
    // A chain file that cannot be parsed: its head is UNKNOWN, so any
    // destructive session operation must refuse rather than risk dangling.
    std::fs::write(h.roots.config_dir.join("chains/broken.json"), "{not json").unwrap();

    let err = sweep_at(
        &h.roots,
        &RetentionPolicy {
            max_age_days: Some(30),
            max_disk_bytes: None,
        },
        now,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("chain"),
        "typed chain failure: {err}"
    );
    assert!(
        session_exists(&h.roots, "old-session"),
        "no session may be deleted while chain protection is unreadable"
    );

    let err = forget(&h.roots, RetentionDomain::Sessions, "old-session").unwrap_err();
    assert!(
        err.to_string().contains("chain"),
        "forget fails closed too: {err}"
    );
    assert!(session_exists(&h.roots, "old-session"));
}
