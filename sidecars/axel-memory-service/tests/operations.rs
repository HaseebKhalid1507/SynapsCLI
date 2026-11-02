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
    raw_with_stderr(path, project, input).0
}
/// The only stderr the service writes: one line per seal capacity eviction
/// with a scope-key prefix, class, whole hours of age and the row ID.
fn eviction_line(line: &str) -> bool {
    let fields: Vec<&str> = line.split(' ').collect();
    let hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    matches!(fields.as_slice(), ["axel_history_evict", scope, class, age, id]
        if scope.strip_prefix("scope=p").is_some_and(|s| hex(s, 8))
            && ["tombstoned", "stale", "superseded", "newest", "floor"]
                .iter()
                .any(|c| class.strip_prefix("class=") == Some(*c))
            && age.strip_prefix("age_h=").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            && id.strip_prefix("id=").is_some_and(|s| hex(s, 32)))
}
fn raw_with_stderr(path: &Path, project: &str, input: &[u8]) -> (Vec<Value>, String) {
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
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.lines().all(eviction_line), "{stderr}");
    let replies = output
        .stdout
        .split(|b| *b == b'\n')
        .filter(|s| !s.is_empty())
        .map(|s| {
            assert!(s.len() < 24 * MAX_FRAME);
            serde_json::from_slice(s).unwrap()
        })
        .collect();
    (replies, stderr)
}
fn call_with_stderr(path: &Path, op: &str, payload: Value) -> (Value, String) {
    let input = format!(
        "{}\n{}\n",
        envelope(PROJECT, "hello", json!({})),
        envelope(PROJECT, op, payload)
    );
    let (replies, stderr) = raw_with_stderr(path, PROJECT, input.as_bytes());
    assert_eq!(replies.len(), 2);
    (replies[1].clone(), stderr)
}
/// Stored history row IDs, oldest first (read-only; no service running).
fn history_ids(path: &Path) -> Vec<Value> {
    let c = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap();
    let mut stmt = c
        .prepare("SELECT id FROM synaps_history ORDER BY created_ms,id")
        .unwrap();
    let ids = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|id| json!(id.unwrap()))
        .collect();
    ids
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
#[test]
fn history_seal_eviction_spares_active_sessions_first_and_newest_archives() {
    // Every session here is active (newest archive < 24 h old), so each one's
    // FIRST and newest archive are floor-protected: an idle session's single
    // archive survives a busy session filling the scope, and so does the busy
    // session's first window. Its superseded middle windows go, oldest first.
    let (_d, p) = temp();
    let idle = ok(call(&p, "history_seal", seal("idle", json!({"role":"user","content":"idle 0"}))))["id"].clone();
    let mut busy = Vec::new();
    for i in 0..255 {
        let q = seal("busy", json!({"role":"user","content":format!("busy {i}")}));
        busy.push(ok(call(&p, "history_seal", q))["id"].clone());
    }
    assert_eq!(history_ids(&p).len(), 256);
    for i in 255..268 {
        let q = seal("busy", json!({"role":"user","content":format!("busy {i}")}));
        let (reply, stderr) = call_with_stderr(&p, "history_seal", q);
        busy.push(ok(reply)["id"].clone());
        assert_eq!(stderr.lines().count(), 1, "seal {i}: {stderr}");
        assert!(stderr.contains(" class=superseded "), "seal {i}: {stderr}");
    }
    let kept = history_ids(&p);
    assert_eq!(kept.len(), 256);
    assert!(kept.contains(&idle), "the idle session's newest archive was evicted");
    assert!(kept.contains(&busy[0]), "the busy session's first archive was evicted");
    assert!(busy[1..14].iter().all(|id| !kept.contains(id)), "exactly the oldest middle windows went");
    assert!(busy[14..].iter().all(|id| kept.contains(id)));
    // A newer idle archive makes idle 0 that active session's FIRST row:
    // both stay, and each seal takes the busy session's next middle window.
    let idle1 = ok(call(&p, "history_seal", seal("idle", json!({"role":"user","content":"idle 1"}))))["id"].clone();
    ok(call(&p, "history_seal", seal("busy", json!({"role":"user","content":"busy 268"}))));
    let kept = history_ids(&p);
    assert_eq!(kept.len(), 256);
    assert!(kept.contains(&idle) && kept.contains(&idle1) && kept.contains(&busy[0]));
    assert!(!kept.contains(&busy[14]) && !kept.contains(&busy[15]) && kept.contains(&busy[16]));
}
#[test]
fn history_seal_evicts_oldest_to_fit_capacity() {
    // docs/specs/archive-size-fit.md: a full inventory never fails a seal.
    let (_d, p) = temp();
    let mut ids = Vec::new();
    for i in 0..256 {
        let q = seal(
            &format!("session-{i}"),
            json!({"role":"user","content":format!("evidence {i}")}),
        );
        // Rows 1..=256 fit the cap without any eviction.
        let (reply, stderr) = call_with_stderr(&p, "history_seal", q);
        assert!(stderr.is_empty(), "row {}: {stderr}", i + 1);
        ids.push(ok(reply)["id"].clone());
    }
    assert_eq!(history_ids(&p).len(), 256);
    // Forget one live row: the tombstone is the first eviction candidate.
    ok(call(&p, "history_forget", json!({"id":ids[5]})));
    let q = seal("session-256", json!({"role":"user","content":"evidence 256"}));
    let (reply, stderr) = call_with_stderr(&p, "history_seal", q.clone());
    let newest = ok(reply);
    assert_eq!(
        stderr,
        format!("axel_history_evict scope=p01234567 class=tombstoned age_h=0 id={}\n", ids[5].as_str().unwrap())
    );
    // Tombstone evicted, oldest live row (ids[0]) still present.
    assert_eq!(
        ok(call(&p, "history_fetch", json!({"id":ids[0],"start":0,"limit":1}))).as_array().unwrap().len(),
        1
    );
    // Every row left is an active session's only (first and newest) archive:
    // the next seal evicts exactly one, the oldest floor row.
    let q2 = seal("session-257", json!({"role":"user","content":"evidence 257"}));
    let (reply, stderr) = call_with_stderr(&p, "history_seal", q2);
    ok(reply);
    assert_eq!(
        stderr,
        format!("axel_history_evict scope=p01234567 class=floor age_h=0 id={}\n", ids[0].as_str().unwrap())
    );
    assert_eq!(history_ids(&p).len(), 256);
    assert_eq!(
        call(&p, "history_fetch", json!({"id":ids[0],"start":0,"limit":1}))["error"]["code"],
        "not_found"
    );
    assert_eq!(
        ok(call(&p, "history_fetch", json!({"id":ids[1],"start":0,"limit":1}))).as_array().unwrap().len(),
        1
    );
    assert_eq!(
        ok(call(&p, "history_fetch", json!({"id":newest["id"],"start":0,"limit":1}))),
        q["messages"]
    );
    // Forgotten evidence stays suppressed after eviction of its tombstone.
    let again = seal("session-5", json!({"role":"user","content":"evidence 5"}));
    assert_eq!(call(&p, "history_seal", again)["error"]["code"], "id_conflict");
    // One apply may carry a full scope (256 rows) into an empty scope, but
    // imports remain strict: the same export into a non-empty scope fails.
    let export = ok(call(&p, "export", json!({"full":true})));
    assert_eq!(export["histories"].as_array().unwrap().len(), 256);
    let (_d2, p2) = temp();
    ok(call(&p2, "migration_apply", apply(export.clone())));
    assert_eq!(history_ids(&p2).len(), 256);
    let (_d3, p3) = temp();
    ok(call(&p3, "history_seal", seal("x-0", json!({"role":"user","content":"x 0"}))));
    assert_eq!(
        call(&p3, "migration_apply", apply(export))["error"]["code"],
        "size_limit"
    );
    assert_eq!(history_ids(&p3).len(), 1);
}
#[test]
fn history_owned_ids_filter_before_limit_and_inspect_owner() {
    let (_d, p) = temp();
    let caps = ok(call(&p, "capabilities", json!({})));
    assert_eq!(caps["history_owned"], json!({"versions":[1,2],"max_ids":128}));
    assert!(caps["operations"]
        .as_array()
        .unwrap()
        .contains(&json!("history_inspect")));
    // Own windows first; newer foreign windows would win the ordering.
    let own = ok(call(
        &p,
        "history_seal",
        seal(
            "worker-run:a",
            json!({"role":"user","content":"NEEDLE own one"}),
        ),
    ))["id"]
        .clone();
    let gone = ok(call(
        &p,
        "history_seal",
        seal(
            "worker-run:a",
            json!({"role":"user","content":"NEEDLE own two"}),
        ),
    ))["id"]
        .clone();
    let mut foreign = Vec::new();
    for (n, logical) in ["worker-run:b", "parent-session", "worker-run:b"]
        .iter()
        .enumerate()
    {
        let text = format!("NEEDLE foreign {n}");
        foreign.push(
            ok(call(
                &p,
                "history_seal",
                seal(logical, json!({"role":"user","content":text})),
            ))["id"]
                .clone(),
        );
    }
    let wide = ok(call(
        &p,
        "history_search",
        json!({"query":"needle","limit":1}),
    ));
    assert_ne!(wide[0]["id"], own);
    let hits = ok(call(
        &p,
        "history_search",
        json!({"query":"needle","limit":1,"ids":[own]}),
    ));
    assert_eq!(hits.as_array().unwrap().len(), 1);
    assert_eq!(hits[0]["id"], own);
    assert!(hits[0]["snippet"].as_str().unwrap().contains("own one"));
    let both = ok(call(
        &p,
        "history_search",
        json!({"query":"","limit":8,"ids":[gone, own]}),
    ));
    assert_eq!(both.as_array().unwrap().len(), 2);
    assert_eq!(
        ok(call(
            &p,
            "history_search",
            json!({"query":"needle","ids":[]})
        )),
        json!([])
    );
    let own_str = own.as_str().unwrap().to_owned();
    for bad in [
        json!([own_str.to_uppercase()]),
        json!([own_str, own_str]),
        json!(["not-hex"]),
        json!((0..129).map(|n| format!("{n:032x}")).collect::<Vec<_>>()),
        json!("x"),
    ] {
        assert_eq!(
            call(&p, "history_search", json!({"query":"","ids":bad}))["error"]["code"],
            "invalid_request",
            "{bad}"
        );
        assert_eq!(
            call(&p, "history_inspect", json!({"ids":bad}))["error"]["code"],
            "invalid_request",
            "{bad}"
        );
    }
    ok(call(&p, "history_forget", json!({"id":gone})));
    let unknown = "f".repeat(32);
    let rows = ok(call(
        &p,
        "history_inspect",
        json!({"ids":[unknown, foreign[1], gone, own]}),
    ));
    let a = hash(&[b"worker-run:a"]);
    let parent = hash(&[b"parent-session"]);
    let find = |id: &Value| {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == *id)
            .cloned()
    };
    assert_eq!(rows.as_array().unwrap().len(), 3);
    assert!(find(&json!(unknown)).is_none());
    assert_eq!(
        find(&own).unwrap(),
        json!({"id":own,"logical_id":a,"live":true})
    );
    assert_eq!(
        find(&gone).unwrap(),
        json!({"id":gone,"logical_id":a,"live":false})
    );
    assert_eq!(
        find(&foreign[1]).unwrap(),
        json!({"id":foreign[1],"logical_id":parent,"live":true})
    );
    assert_eq!(
        ok(call(&p, "history_inspect", json!({"ids":[]}))),
        json!([])
    );
    // A forgotten window no longer matches even when allowed.
    assert_eq!(
        ok(call(&p, "history_search", json!({"query":"","ids":[gone]}))),
        json!([])
    );
    // Another repository's scope sees nothing of this one.
    assert_eq!(
        ok(call_scope(
            &p,
            FOREIGN,
            "history_inspect",
            json!({"ids":[own]})
        )),
        json!([])
    );
    assert_eq!(
        call(&p, "history_inspect", json!({"ids":[own],"extra":1}))["error"]["code"],
        "invalid_request"
    );
}
/// `history_owned/2`: `version:2` wraps the version 1 rows with the brain's
/// seal-eviction count; without `version` the reply is the version 1 array.
#[test]
fn history_inspect_version_2_reports_the_eviction_count_beside_v1_rows() {
    let (_d, p) = temp();
    let own = ok(call(
        &p,
        "history_seal",
        seal("worker-run:a", json!({"role":"user","content":"own"})),
    ))["id"]
        .clone();
    let a = hash(&[b"worker-run:a"]);
    let v1 = ok(call(&p, "history_inspect", json!({"ids":[own]})));
    assert_eq!(v1, json!([{"id":own,"logical_id":a,"live":true}]));
    assert_eq!(
        ok(call(&p, "history_inspect", json!({"ids":[own],"version":1}))),
        v1
    );
    // A new brain starts its count at zero.
    assert_eq!(
        ok(call(&p, "history_inspect", json!({"ids":[own],"version":2}))),
        json!({"entries":v1,"evictions":0})
    );
    assert_eq!(
        ok(call(&p, "history_inspect", json!({"ids":[],"version":2}))),
        json!({"entries":[],"evictions":0})
    );
    for bad in [json!({"ids":[own],"version":3}), json!({"ids":[own],"version":2,"extra":1})] {
        assert_eq!(
            call(&p, "history_inspect", bad.clone())["error"]["code"],
            "invalid_request",
            "{bad}"
        );
    }
}
