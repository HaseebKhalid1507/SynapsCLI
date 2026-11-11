use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
};
use synaps_axel_memory_service::contract::{MAX_FRAME, REVISION, SCHEMA};
const PROJECT: &str = "p0123456789abcdef";
const FOREIGN: &str = "pfedcba9876543210";
fn record(id: &str, content: &str) -> Value {
    json!({"namespace":"notes", "timestamp_ms": chrono::Utc::now().timestamp_millis(), "content":content, "tags":["Äpfel", "literal_%"], "meta":{"nested":[true, 42],"title":"metadata title"}, "id":id, "project":PROJECT, "provenance":{"source":"tool:test","session":"synthetic"},"sensitivity":"normal","retention":"standard"})
}
fn envelope(project: &str, op: &str, payload: Value) -> Value {
    json!({"schema":SCHEMA,"project":project,"operation":op,"payload":payload})
}
fn raw(path: &Path, project: &str, input: &[u8]) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_synaps-axel-memory-service"))
        .args(["--brain", path.to_str().unwrap(), "--project", project])
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Writer thread prevents large request/reply pipe deadlock.
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert!(
        output.status.success(),
        "status {:?}; stderr {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    output
        .stdout
        .split(|b| *b == b'\n')
        .filter(|s| !s.is_empty())
        .map(|s| {
            assert!(s.len() < 24 * MAX_FRAME);
            serde_json::from_slice(s).unwrap()
        })
        .collect()
}
fn call_scope(path: &Path, project: &str, op: &str, payload: Value) -> Value {
    let input = format!(
        "{}\n{}\n",
        envelope(project, "hello", json!({})),
        envelope(project, op, payload)
    );
    let replies = raw(path, project, input.as_bytes());
    assert_eq!(replies.len(), 2);
    assert_eq!(
        replies[0]["result"],
        json!({"backend":"axel","revision":REVISION,"contract":SCHEMA})
    );
    assert_eq!(replies[1]["project"], project);
    replies[1].clone()
}
fn call(path: &Path, op: &str, payload: Value) -> Value {
    call_scope(path, PROJECT, op, payload)
}
fn temp() -> (tempfile::TempDir, std::path::PathBuf) {
    // Symlink-free base: macOS temp dirs live under the /var symlink.
    let dir = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join("synthetic.r8");
    (dir, path)
}
fn hash(parts: &[&[u8]]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_be_bytes());
        h.update(p);
    }
    format!("{:x}", h.finalize())
}
#[derive(serde::Serialize)]
struct Row {
    source_index: usize,
    block_indices: Vec<usize>,
    message: Value,
}
fn seal(logical: &str, message: Value) -> Value {
    let logical = hash(&[logical.as_bytes()]);
    let rows = vec![Row {
        source_index: 0,
        block_indices: if message["content"].is_array() {
            vec![0]
        } else {
            vec![]
        },
        message,
    }];
    let digest = hash(&[
        logical.as_bytes(),
        &1u64.to_be_bytes(),
        &serde_json::to_vec(&rows).unwrap(),
    ]);
    json!({"logical_id":logical,"source_message_count":1,"messages":rows,"note":"hidden-only-needle","digest":digest})
}
fn chat(id: &str) -> Value {
    json!({"schema":"chat_turn_capture/1","capture_id":id.repeat(64),"project_id":PROJECT,"session_id":"synthetic","turn_id":"t1","turn_ordinal":1,"source_digest":"d".repeat(64),"user":"synthetic question","assistant":"synthetic answer","tools":[{"name":"test","summary":"synthetic tool"}]})
}
fn apply(mut export: Value) -> Value {
    export["migration_id"] = json!("e".repeat(64));
    export["manifest_digest"] = json!("f".repeat(64));
    export
}
fn ok(v: Value) -> Value {
    assert_eq!(v["ok"], true, "{v}");
    v["result"].clone()
}
#[test]
fn capture_atomic_retry_forget_and_full_export_import() {
    let (_d, p) = temp();
    let q = chat("a");
    assert_eq!(
        ok(call(
            &p,
            "capture_query",
            json!({"capture_id":"a".repeat(64)})
        ))["committed"],
        false
    );
    ok(call(&p, "capture", q.clone()));
    ok(call(&p, "capture", q.clone()));
    assert_eq!(ok(call(&p, "stats", json!({})))["notes"], 1);
    let mut changed = q.clone();
    changed["assistant"] = json!("changed");
    assert_eq!(call(&p, "capture", changed)["error"]["code"], "id_conflict");
    let all = ok(call(&p, "export", json!({"full":true})));
    assert_eq!(all["captures"][0]["evidence"], q);
    let (_d2, p2) = temp();
    assert_eq!(all["format"], "synaps-axel-export/1");
    assert_eq!(all["source_project"], PROJECT);
    assert_eq!(all["target_project"], PROJECT);
    let import = apply(all.clone());
    ok(call(&p2, "migration_apply", import.clone()));
    ok(call(&p2, "migration_apply", import));
    assert_eq!(
        ok(call(
            &p2,
            "capture_query",
            json!({"capture_id":"a".repeat(64)})
        ))["committed"],
        true
    );
    assert_eq!(ok(call(&p2, "export", json!({"full":true}))), all);
    let mut unknown = apply(all.clone());
    unknown["unknown_inventory_field"] = json!(true);
    assert_eq!(
        call(&p2, "migration_apply", unknown)["error"]["code"],
        "invalid_request"
    );
    let mut changed_scope = apply(all.clone());
    changed_scope["source_project"] = json!(FOREIGN);
    assert_eq!(
        call(&p2, "migration_apply", changed_scope)["error"]["code"],
        "id_conflict"
    );
    let mut wrong_version = apply(all.clone());
    wrong_version["format"] = json!("synaps-axel-export/2");
    assert_eq!(
        call(&p2, "migration_apply", wrong_version)["error"]["code"],
        "invalid_request"
    );
    let mut incomplete = apply(all.clone());
    incomplete.as_object_mut().unwrap().remove("source_project");
    assert_eq!(
        call(&p2, "migration_apply", incomplete)["error"]["code"],
        "invalid_request"
    );
    let mut wrong_target = apply(all);
    wrong_target["target_project"] = json!(FOREIGN);
    assert_eq!(
        call(&p2, "migration_apply", wrong_target)["error"]["code"],
        "project_mismatch"
    );
    let id = format!("mem-cap-{}", "a".repeat(64));
    ok(call(&p, "forget", json!({"id":id})));
    ok(call(&p, "capture", q));
    assert_eq!(
        ok(call(
            &p,
            "capture_query",
            json!({"capture_id":"a".repeat(64)})
        ))["tombstoned"],
        true
    );
    let mut export = apply(ok(call(&p, "export", json!({"full":true}))));
    export["migration_id"] = json!("b".repeat(64));
    ok(call(&p2, "migration_apply", export));
    assert_eq!(
        ok(call(
            &p2,
            "capture_query",
            json!({"capture_id":"a".repeat(64)})
        ))["tombstoned"],
        true
    );
    ok(call(&p2, "capture", chat("c")));
    assert_eq!(
        ok(call(
            &p2,
            "capture_query",
            json!({"capture_id":"c".repeat(64)})
        ))["tombstoned"],
        true
    );
}
#[test]
fn history_digest_hidden_note_paging_retry_and_suppression() {
    let (_d, p) = temp();
    let q = seal(
        "session",
        json!({"role":"user","content":"synthetic evidence é"}),
    );
    let r = ok(call(&p, "history_seal", q.clone()));
    assert_eq!(r["id"].as_str().unwrap().len(), 32);
    assert_eq!(ok(call(&p, "history_seal", q.clone()))["id"], r["id"]);
    assert_eq!(
        ok(call(
            &p,
            "history_search",
            json!({"query":"hidden-only-needle","limit":8})
        )),
        json!([])
    );
    assert_eq!(
        ok(call(
            &p,
            "history_fetch",
            json!({"id":r["id"],"start":0,"limit":1})
        )),
        q["messages"]
    );
    assert_eq!(
        ok(call(&p, "history_note", json!({"id":r["id"]}))),
        "hidden-only-needle"
    );
    let (_d2, p2) = temp();
    let r2 = ok(call(
        &p2,
        "history_seal",
        seal(
            "other",
            json!({"role":"user","content":"synthetic evidence é"}),
        ),
    ));
    ok(call(&p, "history_forget", json!({"id":r["id"]})));
    ok(call(
        &p2,
        "migration_apply",
        apply(ok(call(&p, "export", json!({"full":true})))),
    ));
    assert_eq!(
        call(&p2, "history_fetch", json!({"id":r2["id"]}))["error"]["code"],
        "not_found"
    );
    assert_eq!(call(&p2, "history_seal", q)["error"]["code"], "id_conflict");
    assert_eq!(
        call_scope(&p, FOREIGN, "stats", json!({}))["result"]["notes"],
        0
    );
}
#[test]
fn history_large_empty_and_nested_privacy() {
    let (_d, p) = temp();
    let q = seal(
        "large",
        json!({"role":"user","content":"x".repeat(MAX_FRAME+100)}),
    );
    let r = ok(call(&p, "history_seal", q.clone()));
    assert_eq!(
        ok(call(&p, "history_fetch", json!({"id":r["id"]}))),
        q["messages"]
    );
    for message in [
        json!({"role":"assistant","content":[{"type":"thinking","thinking":"private"}]}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"thinking","thinking":"private"}]}]}),
        json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"test","input":{"nested":{"private":true,"text":"private"}}}]}),
    ] {
        assert_eq!(
            call(&p, "history_seal", seal("privacy", message))["error"]["code"],
            "invalid_request"
        );
    }
    let logical = hash(&[b"empty"]);
    let digest = hash(&[logical.as_bytes(), &0u64.to_be_bytes(), b"[]"]);
    ok(call(
        &p,
        "history_seal",
        json!({"logical_id":logical,"source_message_count":0,"messages":[],"note":"","digest":digest}),
    ));
}
#[test]
fn migration_rollback_nonexistent_tombstone_and_note_fingerprint_union() {
    let (_d, p) = temp();
    let old = record("old", "same source");
    ok(call(&p, "store", old.clone()));
    let (_d2, p2) = temp();
    let mut copy = old.clone();
    copy["id"] = json!("copy");
    ok(call(&p2, "store", copy));
    ok(call(&p, "forget", json!({"id":"old"})));
    let export = apply(ok(call(&p, "export", json!({"full":true}))));
    ok(call(&p2, "migration_apply", export));
    assert_eq!(ok(call(&p2, "stats", json!({})))["notes"], 0);
    let bad = apply(
        json!({"records":[record("first","one"),record("first","different")],"tombstones":["nonexistent"]}),
    );
    assert_eq!(
        call(&p2, "migration_apply", bad)["error"]["code"],
        "id_conflict"
    );
    assert_eq!(
        call(&p2, "fetch", json!({"ids":["first"]}))["error"]["code"],
        "not_found"
    );
    ok(call(&p2, "store", record("nonexistent", "rollback proved")));
    let mut tomb = apply(json!({"tombstones":["missing"]}));
    tomb["migration_id"] = json!("9".repeat(64));
    ok(call(&p2, "migration_apply", tomb));
    assert_eq!(
        call(&p2, "store", record("missing", "no"))["error"]["code"],
        "id_conflict"
    );
}
#[test]
fn restricted_summary_ttl_sweep_and_capabilities() {
    let (_d, p) = temp();
    ok(call(&p, "capabilities", json!({})));
    assert!(!p.exists());
    let q = json!({"schema":"conversation_summary/1","capture_id":"7".repeat(64),"project_id":PROJECT,"source_session_id":"s","source_message_count":2,"source_turn_range":{"first":0,"last":1,"digest":"8".repeat(64)},"summary":"local summary private body","summary_provider":null,"summary_model":null,"local_only":true,"prompt_stack_digest":"9".repeat(64),"redaction_policy":"policy_exclusions","content_classes":["user_text"],"summarized_at_unix_ms":chrono::Utc::now().timestamp_millis()});
    ok(call(&p, "capture", q.clone()));
    let full = ok(call(&p, "export", json!({"full":true})));
    assert_eq!(full["captures"][0]["evidence"], q);
    let (_copy_dir, copy) = temp();
    ok(call(&copy, "migration_apply", apply(full.clone())));
    assert_eq!(ok(call(&copy, "export", json!({"full":true}))), full);
    assert_eq!(ok(call(&copy, "search", json!({}))), json!([]));
    let mut unknown = apply(full);
    unknown["captures"][0]["unknown_capture_field"] = json!(true);
    assert_eq!(
        call(&copy, "migration_apply", unknown)["error"]["code"],
        "invalid_request"
    );
    let mut unknown = q;
    unknown["unknown_capture_field"] = json!(true);
    assert_eq!(
        call(&copy, "capture", unknown)["error"]["code"],
        "invalid_request"
    );
    assert_eq!(ok(call(&p, "search", json!({}))), json!([]));
    assert_eq!(
        call(
            &p,
            "fetch",
            json!({"ids":[format!("mem-cap-{}","7".repeat(64))]})
        )["error"]["code"],
        "not_found"
    );
    let mut expired = record("expired", "ttl");
    expired["meta"] = json!({"_axel":{"expires_ms":1}});
    ok(call(&p, "store", expired));
    assert_eq!(
        call(&p, "fetch", json!({"ids":["expired"]}))["error"]["code"],
        "not_found"
    );
    ok(call(&p, "sweep", json!({})));
    assert_eq!(ok(call(&p, "stats", json!({})))["tombstones"], 1);
    let r = ok(call(
        &p,
        "history_seal",
        seal("protected", json!({"role":"user","content":"protected"})),
    ));
    let swept = ok(call(&p, "sweep", json!({"max_disk_bytes":1})));
    assert_eq!(swept["target_met"], false);
    ok(call(&p, "history_fetch", json!({"id":r["id"]})));
}
#[test]
fn legacy_export_is_read_only_scoped_and_preserves_absolute_policy() {
    let (d, p) = temp();
    ok(call(&p, "store", record("legacy", "source")));
    let c = rusqlite::Connection::open(&p).unwrap();
    c.execute(
        "UPDATE memories SET retention='local_only',expires_at='2000-01-01T00:00:00+00:00'",
        [],
    )
    .unwrap();
    c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    drop(c);
    let before = fs::read(&p).unwrap();
    let target = d.path().join("never-created.r8");
    let q = json!({"source_brain":p,"source_project":PROJECT});
    let result = ok(call(&target, "legacy_export", q.clone()));
    assert!(!target.exists());
    assert_eq!(before, fs::read(&p).unwrap());
    assert_eq!(result["format"], "synaps-axel-export/1");
    assert_eq!(result["source_project"], PROJECT);
    assert_eq!(result["target_project"], PROJECT);
    assert_eq!(result["records"][0]["namespace"], "notes");
    assert_eq!(result["records"][0]["provenance"]["source"], "tool:test");
    assert_eq!(result["records"][0]["sensitivity"], "normal");
    assert_eq!(
        result["records"][0]["meta"]["_axel"]["expires_ms"],
        946684800000u64
    );
    assert_eq!(
        result["source_digest"],
        ok(call(&target, "legacy_export", q))["source_digest"]
    );
}

#[test]
fn tombstone_only_legacy_inventory_keeps_versioned_scope() {
    let (d, p) = temp();
    ok(call(&p, "store", record("gone", "synthetic")));
    ok(call(&p, "forget", json!({"id":"gone"})));
    let target = d.path().join("target.r8");
    let q = json!({"source_brain":p,"source_project":PROJECT});
    let inventory = ok(call_scope(&target, PROJECT, "legacy_export", q.clone()));
    assert!(!target.exists());
    assert_eq!(inventory["format"], "synaps-axel-export/1");
    assert_eq!(inventory["source_project"], PROJECT);
    assert_eq!(inventory["target_project"], PROJECT);
    assert_eq!(inventory["records"], json!([]));
    assert_eq!(inventory["tombstones"], json!(["gone"]));
    assert_eq!(
        inventory,
        ok(call_scope(&target, PROJECT, "legacy_export", q))
    );
    ok(call_scope(
        &target,
        PROJECT,
        "migration_apply",
        apply(inventory),
    ));
    let restored = ok(call_scope(&target, PROJECT, "export", json!({"full":true})));
    assert_eq!(restored["format"], "synaps-axel-export/1");
    assert_eq!(restored["source_project"], PROJECT);
    assert_eq!(restored["target_project"], PROJECT);
    assert_eq!(restored["tombstones"], json!(["gone"]));
}

#[test]
fn history_restore_suppression_dominates_stale_live_rows_and_seal_retry() {
    let (_d, p) = temp();
    let message = json!({"role":"user","content":"synthetic shared history evidence"});
    let first = seal("first-logical", message.clone());
    let second = seal("second-logical", message);
    let other = seal(
        "unrelated-logical",
        json!({"role":"user","content":"unrelated surviving evidence"}),
    );
    let first_row = ok(call(&p, "history_seal", first.clone()));
    let second_row = ok(call(&p, "history_seal", second.clone()));
    let other_row = ok(call(&p, "history_seal", other.clone()));
    ok(call(
        &p,
        "store",
        record("surviving-note", "unrelated note"),
    ));
    ok(call(&p, "history_forget", json!({"id":first_row["id"]})));
    for q in [&first, &second] {
        assert_eq!(
            call(&p, "history_seal", q.clone())["error"]["code"],
            "id_conflict"
        );
    }
    let inventory = ok(call(&p, "export", json!({"full":true})));
    assert_eq!(inventory["source_project"], PROJECT);
    assert_eq!(inventory["target_project"], PROJECT);
    // Older inventories can contain a physically live row suppressed by a
    // different logical history's forgotten fingerprint. Preserve this case.
    assert!(inventory["histories"]
        .as_array()
        .unwrap()
        .iter()
        .any(|h| h["id"] == second_row["id"] && h["tombstone"] == false));
    let (_dest_dir, dest) = temp();
    let request = apply(inventory);
    let receipt = ok(call(&dest, "migration_apply", request.clone()));
    assert_eq!(ok(call(&dest, "migration_apply", request)), receipt);
    for row in [&first_row, &second_row] {
        assert_eq!(
            call(&dest, "history_fetch", json!({"id":row["id"]}))["error"]["code"],
            "not_found"
        );
        assert_eq!(
            call(&dest, "history_note", json!({"id":row["id"]}))["error"]["code"],
            "not_found"
        );
    }
    for q in [first, second] {
        assert_eq!(
            call(&dest, "history_seal", q)["error"]["code"],
            "id_conflict"
        );
    }
    assert_eq!(
        ok(call(&dest, "history_fetch", json!({"id":other_row["id"]}))),
        other["messages"]
    );
    assert_eq!(
        ok(call(&dest, "history_search", json!({})))
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        ok(call(&dest, "fetch", json!({"ids":["surviving-note"]})))[0]["content"],
        "unrelated note"
    );
    let restored = ok(call(&dest, "export", json!({"full":true})));
    for h in restored["histories"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|h| h["id"] != other_row["id"])
    {
        assert_eq!(h["tombstone"], true);
        assert_eq!(h["messages"], json!([]));
        assert_eq!(h["note"], "");
    }
    let (_again_dir, again) = temp();
    ok(call(&again, "migration_apply", apply(restored.clone())));
    assert_eq!(ok(call(&again, "export", json!({"full":true}))), restored);
}

#[test]
fn tombstone_first_history_recovers_evidence_in_both_orders_and_roundtrips() {
    for reverse in [false, true] {
        let (_d, p) = temp();
        let message = json!({"role":"user","content":"synthetic tomb-first history evidence"});
        let first = seal("tomb-first-H1", message.clone());
        let second = seal("tomb-first-H2", message);
        let other = seal(
            "tomb-first-unrelated",
            json!({"role":"user","content":"unrelated surviving history"}),
        );
        let ids = ["1".repeat(32), "2".repeat(32), "3".repeat(32)];
        // A legacy tombstone has no messages, note, or evidence fingerprint.
        ok(call(
            &p,
            "migration_apply",
            apply(json!({"histories":[{
                "id":ids[0],"logical_id":first["logical_id"],"digest":first["digest"],"tombstone":true
            }]})),
        ));
        let before = ok(call(&p, "export", json!({"full":true})));
        assert_eq!(before["fingerprints"].as_array().unwrap().len(), 1);
        assert_eq!(before["fingerprints"][0]["digest"], first["digest"]);
        let mut histories: Vec<Value> = [&first, &second, &other]
            .into_iter()
            .zip(&ids)
            .map(|(q, id)| {
                let mut h = q.clone();
                h["id"] = json!(id);
                h["tombstone"] = json!(false);
                h
            })
            .collect();
        if reverse {
            histories.reverse();
        }
        let mut stale = apply(
            json!({"histories":histories,"records":[record("unrelated-note","unrelated surviving note")]}),
        );
        stale["migration_id"] = json!("9".repeat(64));
        let receipt = ok(call(&p, "migration_apply", stale.clone()));
        assert_eq!(ok(call(&p, "migration_apply", stale)), receipt);
        let (_restored_dir, restored) = temp();
        for path in [&p, &restored] {
            if path == &restored {
                let inventory = ok(call(&p, "export", json!({"full":true})));
                ok(call(path, "migration_apply", apply(inventory.clone())));
                assert_eq!(ok(call(path, "export", json!({"full":true}))), inventory);
            }
            for (id, q) in ids.iter().zip([&first, &second]) {
                for op in ["history_fetch", "history_note"] {
                    assert_eq!(
                        call(path, op, json!({"id":id}))["error"]["code"],
                        "not_found",
                        "reverse={reverse} op={op} id={id}"
                    );
                }
                assert_eq!(
                    call(path, "history_seal", q.clone())["error"]["code"],
                    "id_conflict"
                );
            }
            assert_eq!(
                ok(call(path, "history_search", json!({"query":"tomb-first"}))),
                json!([])
            );
            let hits = ok(call(path, "history_search", json!({})));
            assert_eq!(hits.as_array().unwrap().len(), 1);
            assert_eq!(hits[0]["id"], ids[2]);
            assert_eq!(
                ok(call(path, "history_fetch", json!({"id":ids[2]}))),
                other["messages"]
            );
            assert_eq!(
                ok(call(path, "history_note", json!({"id":ids[2]}))),
                other["note"]
            );
            assert_eq!(ok(call(path, "history_seal", other.clone()))["id"], ids[2]);
            assert_eq!(
                ok(call(path, "fetch", json!({"ids":["unrelated-note"]})))[0]["content"],
                "unrelated surviving note"
            );
            let inventory = ok(call(path, "export", json!({"full":true})));
            for (id, q) in ids.iter().zip([&first, &second]) {
                let h = inventory["histories"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|h| h["id"] == *id)
                    .unwrap();
                assert_eq!(h["logical_id"], q["logical_id"]);
                assert_eq!(h["digest"], q["digest"]);
                assert_eq!(h["tombstone"], true);
                assert_eq!(h["messages"], json!([]));
                assert_eq!(h["note"], "");
            }
            // Verify bodies are physically removed, not merely filtered at read time.
            let c = rusqlite::Connection::open_with_flags(
                path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let count: usize = c.query_row("SELECT count(*) FROM synaps_history WHERE id IN (?1,?2) AND tombstoned=1 AND messages='[]' AND note=''", rusqlite::params![ids[0],ids[1]], |r| r.get(0)).unwrap();
            assert_eq!(count, 2);
        }
    }
}

#[test]
fn tombstone_first_notes_and_capture_inventory_recover_evidence_in_both_orders() {
    for captures in [false, true] {
        for reverse in [false, true] {
            let (_source_dir, source) = temp();
            let ids = if captures {
                for id in ["a", "b"] {
                    ok(call(&source, "capture", chat(id)));
                }
                [
                    format!("mem-cap-{}", "a".repeat(64)),
                    format!("mem-cap-{}", "b".repeat(64)),
                ]
            } else {
                for id in ["N1", "N2"] {
                    ok(call(
                        &source,
                        "store",
                        record(id, "synthetic tomb-first note evidence"),
                    ));
                }
                ["N1".into(), "N2".into()]
            };
            ok(call(
                &source,
                "store",
                record("unrelated", "unrelated surviving note"),
            ));
            let mut stale = ok(call(&source, "export", json!({"full":true})));
            if reverse {
                stale["records"].as_array_mut().unwrap().reverse();
                stale["captures"].as_array_mut().unwrap().reverse();
            }
            let (_d, p) = temp();
            ok(call(
                &p,
                "migration_apply",
                apply(json!({"tombstones":[ids[0]]})),
            ));
            let mut stale = apply(stale);
            stale["migration_id"] = json!("9".repeat(64));
            ok(call(&p, "migration_apply", stale));
            let (_restored_dir, restored) = temp();
            for path in [&p, &restored] {
                if path == &restored {
                    let inventory = ok(call(&p, "export", json!({"full":true})));
                    ok(call(path, "migration_apply", apply(inventory.clone())));
                    assert_eq!(ok(call(path, "export", json!({"full":true}))), inventory);
                }
                for id in &ids {
                    assert_eq!(
                        call(path, "fetch", json!({"ids":[id]}))["error"]["code"],
                        "not_found",
                        "captures={captures} reverse={reverse} id={id}"
                    );
                }
                let hits = ok(call(path, "search", json!({})));
                assert_eq!(hits.as_array().unwrap().len(), 1);
                assert_eq!(hits[0]["id"], "unrelated");
                assert_eq!(
                    ok(call(path, "fetch", json!({"ids":["unrelated"]})))[0]["content"],
                    "unrelated surviving note"
                );
                let inventory = ok(call(path, "export", json!({"full":true})));
                assert_eq!(inventory["records"].as_array().unwrap().len(), 1);
                if captures {
                    for id in ["a", "b"] {
                        assert_eq!(
                            ok(call(
                                path,
                                "capture_query",
                                json!({"capture_id":id.repeat(64)})
                            ))["tombstoned"],
                            true
                        );
                    }
                    for capture in inventory["captures"].as_array().unwrap() {
                        assert_eq!(capture["tombstoned"], true);
                        assert!(capture["evidence"].is_null());
                    }
                }
                let c = rusqlite::Connection::open_with_flags(
                    path,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )
                .unwrap();
                let count: usize = c
                    .query_row(
                        "SELECT count(*) FROM memories WHERE id IN (?1,?2)",
                        rusqlite::params![ids[0], ids[1]],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(count, 0);
                let count: usize = c
                    .query_row(
                        "SELECT count(*) FROM synaps_captures WHERE tombstoned!=1 OR evidence!=''",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(count, 0);
            }
        }
    }
}

#[test]
fn tombstone_first_capture_worker_recovers_source_before_acknowledgement() {
    for reverse in [false, true] {
        let (_d, p) = temp();
        let ids = [
            format!("mem-cap-{}", "a".repeat(64)),
            format!("mem-cap-{}", "b".repeat(64)),
        ];
        ok(call(
            &p,
            "migration_apply",
            apply(json!({"tombstones":[ids[0]]})),
        ));
        for id in if reverse { ["b", "a"] } else { ["a", "b"] } {
            ok(call(&p, "capture", chat(id)));
        }
        let mut other = chat("c");
        other["source_digest"] = json!("f".repeat(64));
        other["assistant"] = json!("unrelated surviving capture");
        ok(call(&p, "capture", other.clone()));
        let (_restored_dir, restored) = temp();
        for path in [&p, &restored] {
            if path == &restored {
                ok(call(
                    path,
                    "migration_apply",
                    apply(ok(call(&p, "export", json!({"full":true})))),
                ));
            }
            for id in ["a", "b"] {
                ok(call(path, "capture", chat(id)));
                assert_eq!(
                    ok(call(
                        path,
                        "capture_query",
                        json!({"capture_id":id.repeat(64)})
                    ))["tombstoned"],
                    true
                );
                assert_eq!(
                    call(
                        path,
                        "fetch",
                        json!({"ids":[format!("mem-cap-{}",id.repeat(64))]})
                    )["error"]["code"],
                    "not_found"
                );
            }
            assert_eq!(
                ok(call(
                    path,
                    "capture_query",
                    json!({"capture_id":other["capture_id"]})
                ))["tombstoned"],
                false
            );
            let inventory = ok(call(path, "export", json!({"full":true})));
            assert_eq!(inventory["records"].as_array().unwrap().len(), 1);
            for c in inventory["captures"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|c| c["capture_id"] != other["capture_id"])
            {
                assert_eq!(c["tombstoned"], true);
                assert!(c["evidence"].is_null());
            }
            let c = rusqlite::Connection::open_with_flags(
                path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let count: usize = c.query_row("SELECT count(*) FROM synaps_captures WHERE note_id IN (?1,?2) AND tombstoned=1 AND evidence=''", rusqlite::params![ids[0],ids[1]], |r| r.get(0)).unwrap();
            assert_eq!(count, 2);
        }
    }
}
fn seal_many(path: &Path, prefix: &str, n: usize) -> Vec<(Value, Value)> {
    (0..n)
        .map(|i| {
            let q = seal(
                &format!("{prefix}-{i}"),
                json!({"role":"user","content":format!("{prefix} evidence {i}")}),
            );
            (ok(call(path, "history_seal", q.clone()))["id"].clone(), q)
        })
        .collect()
}
#[test]
fn history_seal_keeps_every_live_archive() {
    // docs/specs/archive-size-fit.md: no per-scope count/byte cap, no eviction.
    let (_d, p) = temp();
    let sealed = seal_many(&p, "session", 200);
    let stats = ok(call(&p, "stats", json!({})));
    assert_eq!(
        (stats["history"].as_u64(), stats["tombstones"].as_u64()),
        (Some(200), Some(0))
    );
    for (id, q) in &sealed {
        assert_eq!(
            ok(call(
                &p,
                "history_fetch",
                json!({"id":id,"start":0,"limit":1})
            )),
            q["messages"]
        );
    }
    // Search output stays bounded however large the inventory is.
    let hits = ok(call(
        &p,
        "history_search",
        json!({"query":"session evidence","limit":99}),
    ));
    assert_eq!(hits.as_array().unwrap().len(), 32);
    let oldest = ok(call(
        &p,
        "history_search",
        json!({"query":"session evidence 0","limit":8}),
    ));
    assert_eq!(oldest.as_array().unwrap().len(), 1);
    assert_eq!(oldest[0]["id"], sealed[0].0);
    // Per-request limit still applies in a large scope: messages+note > 16 MiB.
    let big = seal(
        "big",
        json!({"role":"user","content":"x".repeat(16 * 1024 * 1024 + 1)}),
    );
    assert_eq!(call(&p, "history_seal", big)["error"]["code"], "size_limit");
    assert_eq!(ok(call(&p, "stats", json!({})))["history"], 200);
}
#[test]
fn history_seal_never_deletes_live_rows_beside_tombstones() {
    let (_d, p) = temp();
    let mut sealed = seal_many(&p, "session", 130);
    let forgotten = [0, 5, 129];
    for i in forgotten {
        assert_eq!(
            ok(call(&p, "history_forget", json!({"id":sealed[i].0}))),
            true
        );
    }
    sealed.extend(seal_many(&p, "later", 10));
    // Live count proves nothing was deleted: 140 sealed - 3 forgotten.
    let stats = ok(call(&p, "stats", json!({})));
    assert_eq!(
        (stats["history"].as_u64(), stats["tombstones"].as_u64()),
        (Some(137), Some(3))
    );
    for i in [0, 1, 4, 5, 6, 127, 128, 129, 130, 139] {
        let fetched = call(
            &p,
            "history_fetch",
            json!({"id":sealed[i].0,"start":0,"limit":1}),
        );
        if forgotten.contains(&i) {
            assert_eq!(fetched["error"]["code"], "not_found");
        } else {
            assert_eq!(ok(fetched), sealed[i].1["messages"], "row {i}");
        }
    }
    // Tombstones are retained, so forgotten evidence stays suppressed.
    let again = seal(
        "session-5",
        json!({"role":"user","content":"session evidence 5"}),
    );
    assert_eq!(
        call(&p, "history_seal", again)["error"]["code"],
        "id_conflict"
    );
}
#[test]
fn import_into_large_scope_bounds_only_the_incoming_batch() {
    let (_d, target) = temp();
    let kept = seal_many(&target, "target", 129);
    // A full 128-history batch (one tombstoned) into a 129-row scope succeeds.
    let (_d2, source) = temp();
    let batch = seal_many(&source, "source", 128);
    ok(call(&source, "history_forget", json!({"id":batch[7].0})));
    let counts = ok(call(
        &target,
        "migration_apply",
        apply(ok(call(&source, "export", json!({"full":true})))),
    ));
    assert_eq!(counts["histories"], 128);
    let stats = ok(call(&target, "stats", json!({})));
    assert_eq!(
        (stats["history"].as_u64(), stats["tombstones"].as_u64()),
        (Some(256), Some(1))
    );
    assert_eq!(
        ok(call(
            &target,
            "history_fetch",
            json!({"id":kept[0].0,"start":0,"limit":1})
        )),
        kept[0].1["messages"]
    );
    assert_eq!(
        ok(call(
            &target,
            "history_fetch",
            json!({"id":batch[0].0,"start":0,"limit":1})
        )),
        batch[0].1["messages"]
    );
    // An over-bound batch (129 histories) still fails atomically: its note is absent.
    let (_d3, over) = temp();
    let rejected = seal_many(&over, "over", 129);
    ok(call(
        &over,
        "store",
        record("mem-over", "over-bound batch note"),
    ));
    let mut inventory = apply(ok(call(&over, "export", json!({"full":true}))));
    inventory["migration_id"] = json!("a".repeat(64));
    assert_eq!(
        call(&target, "migration_apply", inventory)["error"]["code"],
        "size_limit"
    );
    // So does a 2-history batch whose frame exceeds 24 MiB.
    let wide: Vec<Value> = (0..2)
        .map(|i| {
            let mut h = seal(
                &format!("wide-{i}"),
                json!({"role":"user","content":"w".repeat(13 * 1024 * 1024)}),
            );
            h["id"] = json!(format!("{i:032x}"));
            h
        })
        .collect();
    let inventory = json!({"migration_id":"b".repeat(64),"manifest_digest":"f".repeat(64),"records":[record("mem-wide","wide batch note")],"histories":wide});
    assert_eq!(
        call(&target, "migration_apply", inventory)["error"]["code"],
        "size_limit"
    );
    for id in ["mem-over", "mem-wide"] {
        assert_eq!(
            call(&target, "fetch", json!({"ids":[id]}))["error"]["code"],
            "not_found"
        );
    }
    assert_eq!(
        call(&target, "history_fetch", json!({"id":rejected[0].0}))["error"]["code"],
        "not_found"
    );
    let stats = ok(call(&target, "stats", json!({})));
    assert_eq!(
        (stats["history"].as_u64(), stats["notes"].as_u64()),
        (Some(256), Some(0))
    );
}
/// Latency probe, not a gate. Run explicitly in release:
/// `cargo test --release --test operations -- --ignored history_search_latency_probe --nocapture`
/// ~400 archives, median ~0.5 MB, a few at 5-16 MB, in one scope.
#[test]
#[ignore = "latency probe; run explicitly"]
fn history_search_latency_probe() {
    use std::time::Instant;
    let (_d, p) = temp();
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    const WORDS: [&str; 16] = [
        "The", "rollover", "Archive", "session", "fn", "cargo", "Context", "token", "while",
        "SQLite", "memory", "TEST", "window", "commit", "result", "Ok(())",
    ];
    let mut sizes: Vec<usize> = (0..394)
        .map(|_| {
            let z = ((0..4)
                .map(|_| (next() % 10_000) as f64 / 10_000.0)
                .sum::<f64>()
                - 2.0)
                * 3f64.sqrt();
            ((512.0 * 1024.0 * (0.8 * z).exp()) as usize).clamp(32 * 1024, 4 * 1024 * 1024)
        })
        .collect();
    sizes.extend([5, 6, 8, 10, 12, 15].map(|m| m * 1024 * 1024));
    let mut total = 0usize;
    let started = Instant::now();
    for (n, size) in sizes.iter().enumerate() {
        let per = (size / 4000).max(8 * 1024);
        let logical = hash(&[format!("probe-{n}").as_bytes()]);
        let mut rows = Vec::new();
        let mut bytes = 0;
        while bytes < *size {
            let mut text = if rows.is_empty() {
                format!("probe-marker-{n:04} ")
            } else {
                String::new()
            };
            while text.len() < per {
                text.push_str(WORDS[(next() % 16) as usize]);
                text.push(' ');
            }
            bytes += text.len();
            let i = rows.len();
            let (block_indices, message) = if i % 2 == 0 {
                (vec![], json!({"role":"user","content":text}))
            } else {
                (
                    vec![0],
                    json!({"role":"assistant","content":[{"type":"text","text":text}]}),
                )
            };
            rows.push(Row {
                source_index: i,
                block_indices,
                message,
            });
        }
        let body = serde_json::to_vec(&rows).unwrap();
        total += body.len();
        let count = rows.len() as u64;
        let digest = hash(&[logical.as_bytes(), &count.to_be_bytes(), &body]);
        ok(call(
            &p,
            "history_seal",
            json!({"logical_id":logical,"source_message_count":count,"messages":rows,"note":"probe","digest":digest}),
        ));
    }
    println!(
        "probe: {} archives, {:.1} MiB messages, median {:.2} MiB, built in {:.1?}",
        sizes.len(),
        total as f64 / 1048576.0,
        {
            let mut s = sizes.clone();
            s.sort();
            s[s.len() / 2] as f64 / 1048576.0
        },
        started.elapsed()
    );
    // Real process per operation, as the host runs it; scope_info is the
    // spawn+open baseline, so search minus baseline is the query itself.
    let time = |label: &str, op: &str, payload: Value| {
        let mut runs: Vec<f64> = (0..7)
            .map(|_| {
                let t = Instant::now();
                let reply = call(&p, op, payload.clone());
                assert_eq!(reply["ok"], true, "{reply}");
                t.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        runs.sort_by(f64::total_cmp);
        println!(
            "{label:<40} min {:>8.1} ms  median {:>8.1} ms  max {:>8.1} ms",
            runs[0], runs[3], runs[6]
        );
    };
    time("scope_info (spawn+open baseline)", "scope_info", json!({}));
    time(
        "history_search no match",
        "history_search",
        json!({"query":"zzqv-absent-needle","limit":25}),
    );
    time(
        "history_search common word (25 hits)",
        "history_search",
        json!({"query":"rollover","limit":25}),
    );
    time(
        "history_search oldest-only marker",
        "history_search",
        json!({"query":"probe-marker-0000 ","limit":25}),
    );
    time("stats", "stats", json!({}));
    let oldest = ok(call(
        &p,
        "history_search",
        json!({"query":"probe-marker-0000 ","limit":25}),
    ));
    assert_eq!(oldest.as_array().unwrap().len(), 1);
    time(
        "history_fetch oldest (128 rows)",
        "history_fetch",
        json!({"id":oldest[0]["id"],"limit":128}),
    );
    time(
        "history_seal small, scope >= 400 rows",
        "history_seal",
        seal("probe-extra", json!({"role":"user","content":"one more"})),
    );
}
