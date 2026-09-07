use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use synaps_axel_memory_service::contract::{SCHEMA, USER_PROJECT};
const A: &str = "p0123456789abcdef";
const B: &str = "pfedcba9876543210";
fn temp() -> (tempfile::TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    fs::set_permissions(d.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let p = d.path().join("synthetic.r8");
    (d, p)
}
fn call(p: &Path, scope: &str, op: &str, payload: Value) -> Value {
    flags(p, scope, op, payload, &[])
}
fn flags(p: &Path, scope: &str, op: &str, payload: Value, flags: &[&str]) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_synaps-axel-memory-service"))
        .env_clear()
        .args(["--brain", p.to_str().unwrap(), "--project", scope])
        .args(flags)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"schema":SCHEMA,"project":scope,"operation":"hello","payload":{}})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"schema":SCHEMA,"project":scope,"operation":op,"payload":payload})
    )
    .unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{:?}", out);
    assert!(out.stderr.is_empty());
    serde_json::from_slice(
        out.stdout
            .split(|b| *b == b'\n')
            .rfind(|b| !b.is_empty())
            .unwrap(),
    )
    .unwrap()
}
fn ok(v: Value) -> Value {
    assert_eq!(v["ok"], true, "{v}");
    v["result"].clone()
}
fn record(p: &str, id: &str) -> Value {
    json!({"project":p,"id":id,"namespace":format!("project-{p}"),"timestamp_ms":chrono::Utc::now().timestamp_millis(),"content":"synthetic shared evidence","tags":[],"provenance":{"source":"test","session":"session"},"sensitivity":"normal","retention":"standard"})
}
fn hash(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_be_bytes());
        h.update(p);
    }
    format!("{:x}", h.finalize())
}
fn key(path: &Path) -> String {
    format!(
        "p{}",
        &format!("{:x}", Sha256::digest(path.as_os_str().as_encoded_bytes()))[..16]
    )
}
fn capture(p: &str) -> Value {
    json!({"schema":"chat_turn_capture/1","capture_id":"a".repeat(64),"project_id":p,"session_id":"synthetic","turn_id":"turn","turn_ordinal":1,"source_digest":"b".repeat(64),"user":"synthetic user","assistant":"synthetic assistant","tools":[]})
}
fn seal() -> Value {
    #[derive(serde::Serialize)]
    struct Row {
        source_index: usize,
        block_indices: Vec<usize>,
        message: Value,
    }
    let rows = vec![Row {
        source_index: 0,
        block_indices: vec![],
        message: json!({"role":"user","content":"synthetic archive"}),
    }];
    let logical = "d".repeat(64);
    let digest = hash(&[
        logical.as_bytes(),
        &1u64.to_be_bytes(),
        &serde_json::to_vec(&rows).unwrap(),
    ]);
    json!({"logical_id":logical,"source_message_count":1,"messages":rows,"note":"hidden note","digest":digest})
}
fn migration(records: Value, tombstones: Value) -> Value {
    json!({"migration_id":"e".repeat(64),"manifest_digest":"f".repeat(64),"records":records,"tombstones":tombstones})
}
fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn repo(d: &Path) -> (PathBuf, PathBuf, String, String) {
    let main = d.join("main");
    let wt = d.join("worker");
    fs::create_dir(&main).unwrap();
    git(&main, &["init", "-q"]);
    git(
        &main,
        &[
            "-c",
            "user.name=Synthetic",
            "-c",
            "user.email=synthetic@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "synthetic",
        ],
    );
    git(
        &main,
        &["worktree", "add", "-qb", "worker", wt.to_str().unwrap()],
    );
    private_dirs(&main);
    private_dirs(&wt);
    let a = format!("p{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
    assert_ne!(a, USER_PROJECT);
    assert_ne!(a, key(&main));
    let b = key(&wt);
    marker(&main, &a, &main, &[main.clone(), wt.clone()]);
    (main, wt, a, b)
}
fn private_dirs(p: &Path) {
    fs::set_permissions(p, fs::Permissions::from_mode(0o775)).unwrap();
    for e in fs::read_dir(p).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            private_dirs(&p);
        }
    }
}
fn marker(main: &Path, key: &str, initial: &Path, roots: &[PathBuf]) {
    let path = main.join(".git/synaps-memory-identity.json");
    fs::write(
        &path,
        serde_json::to_vec(&json!({"version":2,"key":key,"initial_root":initial,"roots":roots}))
            .unwrap(),
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}
fn alias(p: &Path, canonical: &str, root: &Path, source: &str, old: &Path) -> Value {
    flags(
        p,
        canonical,
        "scope_alias",
        json!({"alias_project":source,"alias_root":old,"canonical_root":root}),
        &["--operator"],
    )
}
#[test]
fn two_projects_all_operations_and_global_collision_fail_closed() {
    let (_d, p) = temp();
    ok(call(&p, A, "store", record(A, "same")));
    ok(call(&p, B, "store", record(B, "other")));
    assert_eq!(
        ok(call(&p, B, "search", json!({})))
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        call(&p, B, "fetch", json!({"ids":["same"]}))["error"]["code"],
        "not_found"
    );
    assert_eq!(call(&p, B, "forget", json!({"id":"same"}))["ok"], false);
    assert_eq!(
        call(&p, B, "store", record(B, "same"))["error"]["code"],
        "id_conflict"
    );
    assert_eq!(
        call(
            &p,
            B,
            "migration_apply",
            migration(json!([]), json!(["same"]))
        )["ok"],
        false
    );
    ok(call(&p, A, "capture", capture(A)));
    assert_eq!(
        ok(call(
            &p,
            B,
            "capture_query",
            json!({"capture_id":"a".repeat(64)})
        ))["committed"],
        false
    );
    assert_eq!(
        call(&p, B, "capture", capture(B))["error"]["code"],
        "id_conflict"
    );
    let h = ok(call(&p, A, "history_seal", seal()));
    assert_eq!(ok(call(&p, B, "history_search", json!({}))), json!([]));
    for op in ["history_fetch", "history_note"] {
        assert_eq!(
            call(&p, B, op, json!({"id":h["id"]}))["error"]["code"],
            "not_found"
        );
    }
    assert_eq!(
        ok(call(&p, B, "history_forget", json!({"id":h["id"]}))),
        false
    );
    let before = ok(call(&p, A, "export", json!({"full":true})));
    let stats = ok(call(&p, B, "stats", json!({})));
    assert!(stats["database_bytes"].as_u64().unwrap() > stats["bytes"].as_u64().unwrap());
    ok(call(
        &p,
        B,
        "sweep",
        json!({"max_disk_bytes":stats["bytes"]}),
    ));
    assert_eq!(ok(call(&p, B, "stats", json!({})))["notes"], 1);
    ok(call(
        &p,
        B,
        "sweep",
        json!({"max_age_days":0,"max_disk_bytes":0}),
    ));
    assert_eq!(ok(call(&p, A, "export", json!({"full":true}))), before);
    ok(call(&p, A, "forget", json!({"id":"same"})));
    assert_eq!(
        call(
            &p,
            B,
            "migration_apply",
            migration(json!([record(B, "same")]), json!([]))
        )["error"]["code"],
        "id_conflict"
    );
    assert_eq!(
        call(
            &p,
            B,
            "migration_apply",
            migration(json!([]), json!(["same"]))
        )["error"]["code"],
        "id_conflict"
    );
    assert_eq!(
        ok(call(&p, B, "export", json!({"full":true})))["tombstones"],
        json!(["other"])
    );
}
#[test]
fn alias_worktrees_preserves_identity_receipts_and_shared_deletion() {
    let (d, p) = temp();
    let (main, wt, a, b) = repo(d.path());
    let r = record(&b, "old-id");
    let m = migration(json!([r.clone()]), json!([]));
    let receipt = ok(call(&p, &b, "migration_apply", m.clone()));
    ok(call(&p, &b, "capture", capture(&b)));
    let h = ok(call(&p, &b, "history_seal", seal()));
    let before = ok(call(&p, &b, "export", json!({"full":true})));
    assert_eq!(
        call(&p, &a, "fetch", json!({"ids":["old-id"]}))["error"]["code"],
        "not_found"
    );
    assert_eq!(
        call(
            &p,
            &a,
            "scope_alias",
            json!({"alias_project":b,"alias_root":wt,"canonical_root":main})
        )["error"]["code"],
        "protocol_error"
    );
    let info = ok(alias(&p, &a, &main, &b, &wt));
    assert_eq!(info["canonical_project"], a);
    assert_eq!(info["members"].as_array().unwrap().len(), 2);
    assert_eq!(ok(alias(&p, &a, &main, &b, &wt)), info);
    let after = ok(call(&p, &b, "export", json!({"full":true})));
    for field in [
        "records",
        "captures",
        "histories",
        "tombstones",
        "fingerprints",
    ] {
        assert_eq!(after[field], before[field], "{field}");
    }
    assert_eq!(ok(call(&p, &a, "fetch", json!({"ids":["old-id"]})))[0], r);
    assert_eq!(ok(call(&p, &a, "migration_apply", m)), receipt);
    ok(call(&p, &a, "capture", capture(&b)));
    assert_eq!(ok(call(&p, &a, "history_seal", seal()))["id"], h["id"]);
    assert_eq!(
        ok(call(
            &p,
            B,
            "capture_query",
            json!({"capture_id":"a".repeat(64)})
        ))["committed"],
        false
    );
    ok(call(&p, &a, "forget", json!({"id":"old-id"})));
    assert_eq!(call(&p, &b, "store", r)["error"]["code"], "id_conflict");
    ok(call(&p, &a, "history_forget", json!({"id":h["id"]})));
    assert_eq!(
        call(&p, &b, "history_fetch", json!({"id":h["id"]}))["error"]["code"],
        "not_found"
    );
    ok(call(
        &p,
        &a,
        "forget",
        json!({"id":format!("mem-cap-{}","a".repeat(64))}),
    ));
    assert_eq!(
        ok(call(
            &p,
            &b,
            "capture_query",
            json!({"capture_id":"a".repeat(64)})
        ))["tombstoned"],
        true
    );
}
#[test]
fn alias_historical_move_unknown_and_reused_foreign_path() {
    let (d, p) = temp();
    let (main, wt, a, b) = repo(d.path());
    ok(call(&p, &b, "stats", json!({})));
    let moved = d.path().join("moved");
    fs::rename(&main, &moved).unwrap();
    git(&moved, &["worktree", "repair"]);
    // Stable canonical key stays bound to the historical main root.
    ok(alias(&p, &a, &moved, &b, &wt));
    let old = d.path().join("historical");
    let oldkey = key(&old);
    marker(
        &moved,
        &a,
        &main,
        &[main.clone(), wt.clone(), old.clone(), moved.clone()],
    );
    assert_eq!(
        alias(&p, &a, &moved, &oldkey, &old)["error"]["code"],
        "not_found"
    );
    ok(call(&p, &oldkey, "store", record(&oldkey, "historical-id")));
    ok(alias(&p, &a, &moved, &oldkey, &old));
    fs::create_dir(&old).unwrap();
    git(&old, &["init", "-q"]);
    assert_eq!(
        alias(&p, &a, &moved, &oldkey, &old)["error"]["code"],
        "project_mismatch"
    );
    assert_eq!(alias(&p, B, &moved, &b, &wt)["ok"], false);
    assert_eq!(
        ok(call(&p, &oldkey, "scope_info", json!({})))["canonical_project"],
        a
    );
}
#[test]
fn native_readonly_export_keeps_capture_history_and_digest() {
    let (d, p) = temp();
    ok(call(&p, A, "capture", capture(A)));
    ok(call(&p, A, "history_seal", seal()));
    ok(call(&p, B, "store", record(B, "foreign")));
    let expected = ok(call(&p, A, "export", json!({"full":true})));
    let bytes = fs::read(&p).unwrap();
    let target = d.path().join("absent.r8");
    let q = json!({"source_brain":p,"source_project":A});
    let inventory = ok(call(&target, A, "legacy_export", q.clone()));
    for field in [
        "records",
        "captures",
        "histories",
        "tombstones",
        "fingerprints",
    ] {
        assert_eq!(inventory[field], expected[field], "{field}");
    }
    assert_eq!(ok(call(&target, A, "legacy_export", q.clone())), inventory);
    assert_eq!(fs::read(&p).unwrap(), bytes);
    assert!(!target.exists());
    assert_eq!(
        call(&target, B, "legacy_export", q)["error"]["code"],
        "project_mismatch"
    );
}
#[test]
fn reserved_scope_explicit_flag_and_old_protocol_rejected() {
    let (_d, p) = temp();
    assert_eq!(
        call(&p, USER_PROJECT, "stats", json!({}))["error"]["code"],
        "project_mismatch"
    );
    assert!(!p.exists());
    assert_eq!(
        flags(&p, A, "stats", json!({}), &["--user-scope"])["error"]["code"],
        "project_mismatch"
    );
    ok(flags(
        &p,
        USER_PROJECT,
        "store",
        record(USER_PROJECT, "user-note"),
        &["--user-scope"],
    ));
    assert_eq!(ok(call(&p, A, "search", json!({}))), json!([]));
    let mut input = std::io::Cursor::new(format!(
        "{}\n",
        json!({"schema":"synaps-axel/1","project":A,"operation":"hello","payload":{}})
    ));
    let mut output = Vec::new();
    synaps_axel_memory_service::protocol::run(&mut input, &mut output, &p, A).unwrap();
    let reply: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(reply["schema"], SCHEMA);
    assert_eq!(reply["ok"], false);
}
#[test]
fn concurrent_project_writers_wait_without_external_retry_or_scope_loss() {
    let (_d, p) = temp();
    let mut threads = Vec::new();
    for i in 0..8 {
        let p = p.clone();
        threads.push(std::thread::spawn(move || {
            let scope = if i % 2 == 0 { A } else { B };
            ok(call(&p, scope, "store", record(scope, &format!("id-{i}"))));
        }));
    }
    for t in threads {
        t.join().unwrap();
    }
    for scope in [A, B] {
        assert_eq!(ok(call(&p, scope, "stats", json!({})))["notes"], 4);
    }
}

#[test]
fn pinned_upgrade_is_explicit_and_preserves_original_records() {
    let (_d, p) = temp();
    let r = record(A, "pinned");
    ok(call(&p, A, "store", r.clone()));
    let c = rusqlite::Connection::open(&p).unwrap();
    c.execute(
        "DELETE FROM brain_meta WHERE key='synaps-axel/2/multi-project'",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO brain_meta VALUES('synaps-axel/1/project',?1)",
        [A],
    )
    .unwrap();
    c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    drop(c);
    let before = fs::read(&p).unwrap();
    assert_eq!(
        call(&p, B, "stats", json!({}))["error"]["code"],
        "unsupported_brain"
    );
    assert_eq!(fs::read(&p).unwrap(), before);
    assert_eq!(ok(call(&p, A, "scope_info", json!({})))["mode"], "pinned");
    assert_eq!(
        call(&p, A, "scope_upgrade", json!({"expected_project":A}))["error"]["code"],
        "protocol_error"
    );
    assert_eq!(
        flags(
            &p,
            A,
            "scope_upgrade",
            json!({"expected_project":B}),
            &["--operator"]
        )["ok"],
        false
    );
    let q = json!({"expected_project":A});
    let result = ok(flags(&p, A, "scope_upgrade", q.clone(), &["--operator"]));
    assert_eq!(result["mode"], "multi_project");
    assert_eq!(
        ok(flags(&p, A, "scope_upgrade", q, &["--operator"])),
        result
    );
    assert_eq!(ok(call(&p, A, "fetch", json!({"ids":["pinned"]})))[0], r);
    ok(call(&p, B, "store", record(B, "new-project")));
    assert_eq!(
        flags(
            &p,
            B,
            "scope_upgrade",
            json!({"expected_project":B}),
            &["--operator"]
        )["error"]["code"],
        "id_conflict"
    );
    let c = rusqlite::Connection::open(&p).unwrap();
    let provenance: String = c
        .query_row(
            "SELECT provenance FROM memories WHERE id='pinned'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&provenance).unwrap()["schema"],
        "synaps-axel/1"
    );
}

#[test]
fn foreign_migration_receipt_history_and_capture_tombstones_conflict() {
    let (_d, p) = temp();
    let m = migration(json!([]), json!([]));
    let receipt = ok(call(&p, A, "migration_apply", m.clone()));
    assert_eq!(ok(call(&p, A, "migration_apply", m.clone())), receipt);
    assert_eq!(
        call(&p, B, "migration_apply", m)["error"]["code"],
        "id_conflict"
    );
    let h = ok(call(&p, A, "history_seal", seal()));
    let inventory = ok(call(&p, A, "export", json!({"full":true})));
    let mut import = migration(json!([]), json!([]));
    import["migration_id"] = json!("1".repeat(64));
    import["histories"] = inventory["histories"].clone();
    assert_eq!(
        call(&p, B, "migration_apply", import)["error"]["code"],
        "id_conflict"
    );
    assert!(call(&p, A, "history_fetch", json!({"id":h["id"]}))["ok"] == true);
    ok(call(&p, A, "capture", capture(A)));
    ok(call(
        &p,
        A,
        "forget",
        json!({"id":format!("mem-cap-{}","a".repeat(64))}),
    ));
    assert_eq!(
        call(&p, B, "capture", capture(B))["error"]["code"],
        "id_conflict"
    );
    assert_eq!(
        ok(call(
            &p,
            B,
            "capture_query",
            json!({"capture_id":"a".repeat(64)})
        ))["committed"],
        false
    );
    assert_eq!(
        ok(call(&p, B, "export", json!({"full":true})))["tombstones"],
        json!([])
    );
}

#[test]
fn alias_union_applies_deletion_before_visibility_and_preserves_tombstone_owner() {
    let (d, p) = temp();
    let (main, wt, a, b) = repo(d.path());
    let old = ok(call(&p, &b, "history_seal", seal()));
    let current = ok(call(&p, &a, "history_seal", seal()));
    ok(call(&p, &b, "history_forget", json!({"id":old["id"]})));
    ok(call(&p, &b, "store", record(&b, "old-note")));
    ok(call(&p, &b, "forget", json!({"id":"old-note"})));
    let before = ok(call(&p, &b, "export", json!({"full":true})));
    ok(alias(&p, &a, &main, &b, &wt));
    assert_eq!(
        call(&p, &a, "history_fetch", json!({"id":current["id"]}))["error"]["code"],
        "not_found"
    );
    assert_eq!(
        call(&p, &a, "history_seal", seal())["error"]["code"],
        "id_conflict"
    );
    let after = ok(call(&p, &b, "export", json!({"full":true})));
    assert_eq!(before["tombstones"], after["tombstones"]);
    assert_eq!(
        call(&p, B, "store", record(B, "old-note"))["error"]["code"],
        "id_conflict"
    );
    let c = rusqlite::Connection::open(&p).unwrap();
    let owner: String = c
        .query_row(
            "SELECT project_key FROM memory_tombstones WHERE id='old-note'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(owner, b);
}

#[test]
fn nonnative_legacy_export_keeps_conservative_policy_conversion() {
    let (d, p) = temp();
    ok(call(&p, A, "store", record(A, "legacy")));
    let c = rusqlite::Connection::open(&p).unwrap();
    c.execute(
        "DELETE FROM brain_meta WHERE key='synaps-axel/2/multi-project'",
        [],
    )
    .unwrap();
    c.execute(
        "UPDATE memories SET retention='local_only',expires_at='2000-01-01T00:00:00+00:00'",
        [],
    )
    .unwrap();
    c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    drop(c);
    let before = fs::read(&p).unwrap();
    let target = d.path().join("unopened.r8");
    let inventory = ok(call(
        &target,
        B,
        "legacy_export",
        json!({"source_brain":p,"source_project":A}),
    ));
    assert_eq!(inventory["records"][0]["namespace"], format!("project-{B}"));
    assert_eq!(inventory["records"][0]["sensitivity"], "secret");
    assert_eq!(
        inventory["records"][0]["meta"]["_axel"]["source_project"],
        A
    );
    assert_eq!(
        inventory["records"][0]["meta"]["_axel"]["expires_ms"],
        946684800000u64
    );
    assert_eq!(fs::read(&p).unwrap(), before);
    assert!(!target.exists());
}

#[test]
fn alias_group_restore_batches_original_scopes_and_reauthorizes_membership() {
    let (d, p) = temp();
    let (main, wt, a, b) = repo(d.path());
    ok(call(&p, &a, "store", record(&a, "main-note")));
    ok(call(&p, &b, "store", record(&b, "worktree-note")));
    ok(call(&p, &b, "capture", capture(&b)));
    let mut second = capture(&a);
    second["capture_id"] = json!("c".repeat(64));
    second["source_digest"] = json!("d".repeat(64));
    ok(call(&p, &a, "capture", second.clone()));
    let mut surviving = capture(&a);
    surviving["capture_id"] = json!("9".repeat(64));
    surviving["source_digest"] = json!("8".repeat(64));
    surviving["assistant"] = json!("independent surviving capture evidence");
    ok(call(&p, &a, "capture", surviving.clone()));
    let h = ok(call(&p, &b, "history_seal", seal()));
    ok(alias(&p, &a, &main, &b, &wt));
    // Delete a member's capture THROUGH THE CANONICAL SCOPE before export.
    let capture_note = format!("mem-cap-{}", "a".repeat(64));
    ok(call(&p, &a, "forget", json!({"id":capture_note})));
    for id in ["a", "c"] {
        assert_eq!(
            ok(call(
                &p,
                &a,
                "capture_query",
                json!({"capture_id":id.repeat(64)})
            ))["tombstoned"],
            true
        );
    }
    assert_eq!(
        ok(call(
            &p,
            &a,
            "capture_query",
            json!({"capture_id":"9".repeat(64)})
        ))["tombstoned"],
        false
    );
    let scopes = ok(call(&p, &a, "scope_info", json!({})));
    let target_dir = d.path().join("restore");
    fs::create_dir(&target_dir).unwrap();
    fs::set_permissions(&target_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let target = target_dir.join("restored.r8");
    let mut inventories = Vec::new();
    for (i, scope) in scopes["members"].as_array().unwrap().iter().enumerate() {
        let scope = scope.as_str().unwrap();
        let inventory = ok(call(&p, scope, "export", json!({"full":true})));
        assert_eq!(inventory["format"], "synaps-axel-export/1");
        assert_eq!(inventory["source_project"], scope);
        assert_eq!(inventory["target_project"], scope);
        for record in inventory["records"].as_array().unwrap() {
            assert_eq!(record["project"], scope);
        }
        let native = ok(call(
            &target,
            scope,
            "legacy_export",
            json!({"source_brain":p,"source_project":scope}),
        ));
        for field in [
            "records",
            "histories",
            "captures",
            "fingerprints",
            "tombstones",
        ] {
            assert_eq!(inventory[field], native[field], "{field}");
        }
        let mut request = inventory.clone();
        request["migration_id"] = json!(format!("{i:064x}"));
        request["manifest_digest"] = json!("f".repeat(64));
        let receipt = ok(call(&target, scope, "migration_apply", request.clone()));
        assert_eq!(
            ok(call(&target, scope, "migration_apply", request.clone())),
            receipt
        );
        inventories.push((scope.to_owned(), inventory, request, receipt));
    }
    assert_eq!(
        call(&target, &a, "fetch", json!({"ids":["worktree-note"]}))["error"]["code"],
        "not_found"
    );
    ok(alias(&target, &a, &main, &b, &wt));
    for (scope, inventory, request, receipt) in inventories {
        assert_eq!(
            ok(call(&target, &scope, "export", json!({"full":true}))),
            inventory
        );
        assert_eq!(ok(call(&target, &a, "migration_apply", request)), receipt);
    }
    assert_eq!(
        ok(call(&target, &a, "fetch", json!({"ids":["worktree-note"]})))[0]["project"],
        b
    );
    assert_eq!(ok(call(&target, &a, "history_seal", seal()))["id"], h["id"]);
    ok(call(&target, &a, "capture", second));
    ok(call(&target, &a, "capture", surviving));
    assert_eq!(
        ok(call(
            &target,
            &a,
            "capture_query",
            json!({"capture_id":"9".repeat(64)})
        ))["tombstoned"],
        false
    );
    ok(call(&target, &a, "capture", capture(&b)));
    assert_eq!(
        ok(call(
            &target,
            &a,
            "capture_query",
            json!({"capture_id":"a".repeat(64)})
        ))["tombstoned"],
        true
    );
    assert_eq!(
        call(&target, &b, "fetch", json!({"ids":[capture_note]}))["error"]["code"],
        "not_found"
    );
}

#[test]
fn random_v2_marker_rejects_v1_bad_membership_and_reused_path() {
    let (d, p) = temp();
    let (main, wt, a, b) = repo(d.path());
    ok(call(&p, &b, "store", record(&b, "historical")));
    let path = main.join(".git/synaps-memory-identity.json");
    let original = fs::read(&path).unwrap();
    let marker: Value = serde_json::from_slice(&original).unwrap();
    for invalid in [
        {
            let mut m = marker.clone();
            m["version"] = json!(1);
            m
        },
        {
            let mut m = marker.clone();
            m["key"] = json!(key(&main));
            m
        },
        {
            let mut m = marker.clone();
            m["roots"] = json!([wt]);
            m
        },
        {
            let mut m = marker.clone();
            m["key"] = json!(USER_PROJECT);
            m
        },
    ] {
        fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert_eq!(
            alias(&p, &a, &main, &b, &wt)["error"]["code"],
            "project_mismatch"
        );
        assert_eq!(
            ok(call(&p, &a, "scope_info", json!({})))["members"],
            json!([a])
        );
    }
    fs::write(&path, original).unwrap();
    ok(alias(&p, &a, &main, &b, &wt));
    // A new repository occupying the same filesystem root gets a NEW random
    // identity, not implicit access to the previous repository's alias group.
    let replacement = format!("p{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
    assert_ne!(a, replacement);
    let mut next = marker;
    next["key"] = json!(replacement);
    fs::write(&path, serde_json::to_vec(&next).unwrap()).unwrap();
    assert_eq!(
        call(&p, &replacement, "fetch", json!({"ids":["historical"]}))["error"]["code"],
        "not_found"
    );
    assert_eq!(
        alias(&p, &replacement, &main, &b, &wt)["error"]["code"],
        "id_conflict"
    );
}

fn waiting_writer(p: &Path, scope: &str, id: &str) -> std::process::Child {
    let mut child = Command::new(env!("CARGO_BIN_EXE_synaps-axel-memory-service"))
        .env_clear()
        .args(["--brain", p.to_str().unwrap(), "--project", scope])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for (operation, payload) in [("hello", json!({})), ("store", record(scope, id))] {
        writeln!(
            input,
            "{}",
            json!({"schema":SCHEMA,"project":scope,"operation":operation,"payload":payload})
        )
        .unwrap();
    }
    child
}
fn writer_reply(child: std::process::Child) -> Value {
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    serde_json::from_slice(
        output
            .stdout
            .split(|b| *b == b'\n')
            .rfind(|b| !b.is_empty())
            .unwrap(),
    )
    .unwrap()
}
#[test]
fn held_lock_wait_release_timeout_and_cancel_are_prewrite_safe() {
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};
    let (d, p) = temp();
    let lock = fs::File::open(d.path()).unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let mut first = waiting_writer(&p, A, "first");
    let mut second = waiting_writer(&p, B, "second");
    let mut cancelled = waiting_writer(&p, A, "cancelled");
    std::thread::sleep(Duration::from_millis(200));
    assert!(first.try_wait().unwrap().is_none());
    assert!(second.try_wait().unwrap().is_none());
    assert!(!p.exists());
    cancelled.kill().unwrap();
    cancelled.wait().unwrap();
    drop(lock);
    ok(writer_reply(first));
    ok(writer_reply(second));
    assert_eq!(
        call(&p, A, "fetch", json!({"ids":["cancelled"]}))["error"]["code"],
        "not_found"
    );
    let before = fs::read(&p).unwrap();
    let lock = fs::File::open(d.path()).unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let start = Instant::now();
    assert_eq!(
        writer_reply(waiting_writer(&p, A, "timed-out"))["error"]["code"],
        "storage_error"
    );
    assert!(start.elapsed() >= Duration::from_secs(8));
    assert!(start.elapsed() < Duration::from_secs(10));
    assert_eq!(fs::read(&p).unwrap(), before);
    drop(lock);
    ok(call(&p, A, "store", record(A, "after-timeout")));
    assert_eq!(ok(call(&p, A, "stats", json!({})))["notes"], 2);
    assert_eq!(ok(call(&p, B, "stats", json!({})))["notes"], 1);
}

#[test]
fn user_scope_direct_protocol_is_notes_only_even_with_operator() {
    let (_d, p) = temp();
    for options in [vec!["--user-scope"], vec!["--user-scope", "--operator"]] {
        for operation in [
            "capture",
            "capture_query",
            "history_seal",
            "history_search",
            "history_fetch",
            "history_note",
            "history_forget",
            "forum_post",
            "forum_read",
            "forum_forget",
            "migration_apply",
            "scope_upgrade",
            "scope_alias",
            "stats",
            "sweep",
            "export",
            "legacy_export",
        ] {
            assert_eq!(
                flags(&p, USER_PROJECT, operation, json!({}), &options)["error"]["code"],
                "project_mismatch",
                "{operation}"
            );
            assert!(!p.exists());
        }
    }
    let options = ["--user-scope"];
    let caps = ok(flags(&p, USER_PROJECT, "capabilities", json!({}), &options));
    assert_eq!(
        caps["operations"],
        json!(["store", "search", "fetch", "forget", "scope_info"])
    );
    assert_eq!(caps["operator_operations"], json!([]));
    assert!(!p.exists());
    assert_eq!(
        ok(flags(&p, USER_PROJECT, "scope_info", json!({}), &options))["members"],
        json!([USER_PROJECT])
    );
    let note = record(USER_PROJECT, "explicit-user-note");
    ok(flags(&p, USER_PROJECT, "store", note.clone(), &options));
    assert_eq!(
        ok(flags(
            &p,
            USER_PROJECT,
            "fetch",
            json!({"ids":["explicit-user-note"]}),
            &options
        )),
        json!([note])
    );
    assert_eq!(
        ok(flags(&p, USER_PROJECT, "search", json!({}), &options))
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let before = fs::read(&p).unwrap();
    assert_eq!(
        flags(
            &p,
            USER_PROJECT,
            "capture",
            capture(USER_PROJECT),
            &["--user-scope", "--operator"]
        )["error"]["code"],
        "project_mismatch"
    );
    assert_eq!(fs::read(&p).unwrap(), before);
    ok(flags(
        &p,
        USER_PROJECT,
        "forget",
        json!({"id":"explicit-user-note"}),
        &options,
    ));
    assert_eq!(
        ok(flags(&p, USER_PROJECT, "search", json!({}), &options)),
        json!([])
    );
}

#[test]
fn tombstone_first_recovered_fingerprints_keep_original_owner_through_alias() {
    let (d, p) = temp();
    let (root, wt, a, b) = repo(d.path());
    let q = seal();
    let id = "1".repeat(32);
    let note = record(&b, "legacy-note");
    let mut tomb = migration(json!([]), json!([note["id"]]));
    tomb["histories"] =
        json!([{"id":id,"logical_id":q["logical_id"],"digest":q["digest"],"tombstone":true}]);
    ok(call(&p, &b, "migration_apply", tomb));
    // Identical evidence in an unrelated original scope must remain readable.
    let foreign = ok(call(&p, A, "history_seal", q.clone()));
    ok(call(&p, A, "store", record(A, "foreign-note")));
    ok(call(&p, &a, "scope_info", json!({})));
    ok(alias(&p, &a, &root, &b, &wt));
    let mut stale = migration(json!([note]), json!([]));
    stale["migration_id"] = json!("9".repeat(64));
    let mut h = q.clone();
    h["id"] = json!(id);
    stale["histories"] = json!([h]);
    ok(call(&p, &a, "migration_apply", stale));
    let original = ok(call(&p, &b, "export", json!({"full":true})));
    let canonical = ok(call(&p, &a, "export", json!({"full":true})));
    assert_eq!(canonical["fingerprints"], json!([]));
    assert_eq!(canonical["histories"], json!([]));
    assert_eq!(original["histories"][0]["id"], id);
    assert_eq!(original["histories"][0]["digest"], q["digest"]);
    assert_eq!(original["histories"][0]["logical_id"], q["logical_id"]);
    assert_eq!(original["histories"][0]["messages"], json!([]));
    let fps = original["fingerprints"].as_array().unwrap();
    assert_eq!(fps.iter().filter(|f| f["kind"] == "history").count(), 2);
    assert_eq!(fps.iter().filter(|f| f["kind"] == "note").count(), 1);
    assert_eq!(
        ok(call(&p, A, "history_fetch", json!({"id":foreign["id"]}))),
        q["messages"]
    );
    assert_eq!(
        ok(call(&p, A, "history_seal", q.clone()))["id"],
        foreign["id"]
    );
    assert_eq!(
        ok(call(&p, A, "fetch", json!({"ids":["foreign-note"]})))[0]["content"],
        "synthetic shared evidence"
    );
    let restored = d.path().join("restored.r8");
    let mut restore = original.clone();
    restore["migration_id"] = json!("8".repeat(64));
    restore["manifest_digest"] = json!("7".repeat(64));
    ok(call(&restored, &b, "migration_apply", restore));
    assert_eq!(
        ok(call(&restored, &b, "export", json!({"full":true}))),
        original
    );
    assert_eq!(
        call(&restored, &b, "history_seal", q)["error"]["code"],
        "id_conflict"
    );
    assert_eq!(
        call(&restored, &b, "store", record(&b, "copy-note"))["error"]["code"],
        "id_conflict"
    );
}

#[test]
fn forum_alias_worktrees_keep_original_envelope_and_shared_tombstones() {
    use synaps_axel_memory_service::forum_contract::{Author, Post, PostRequest};
    let (d, p) = temp();
    let (main, wt, a, b) = repo(d.path());
    let q = PostRequest {
        author: Author::fresh(),
        post: Post {
            request_key: "worktree-root".into(),
            thread_id: None,
            reply_to: None,
            title: "Synthetic worktree finding".into(),
            body: "Public synthetic worktree note".into(),
            retention_days: 30,
        },
    };
    let root = ok(call(&p, &b, "forum_post", json!(q)));
    ok(call(&p, &a, "scope_info", json!({})));
    let before = ok(call(&p, &b, "export", json!({"full":true})));
    ok(alias(&p, &a, &main, &b, &wt));
    let page = ok(call(
        &p,
        &a,
        "forum_read",
        json!({"thread_id":root["id"],"limit":8}),
    ));
    assert_eq!(page["entries"][0]["envelope"]["project"], b);
    assert_eq!(ok(call(&p, &b, "export", json!({"full":true}))), before);
    assert_eq!(
        ok(call(&p, &b, "forum_post", json!(q)))["status"],
        "duplicate"
    );
    let r = PostRequest {
        author: q.author.child(),
        post: Post {
            request_key: "worktree-reply".into(),
            thread_id: Some(root["id"].as_str().unwrap().into()),
            reply_to: None,
            title: String::new(),
            body: "Reply from canonical scope".into(),
            retention_days: 30,
        },
    };
    let reply = ok(call(&p, &a, "forum_post", json!(r)));
    assert_eq!(
        ok(call(
            &p,
            &b,
            "forum_read",
            json!({"thread_id":root["id"],"limit":8})
        ))["entries"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(ok(call(&p, &a, "search", json!({}))), json!([]));
    ok(call(&p, &a, "forum_forget", json!({"id":root["id"]})));
    assert_eq!(
        ok(call(&p, &b, "forum_post", json!(q)))["status"],
        "tombstoned"
    );
    let page = ok(call(
        &p,
        &b,
        "forum_read",
        json!({"thread_id":root["id"],"limit":8}),
    ));
    assert_eq!(page["entries"].as_array().unwrap().len(), 1);
    assert_eq!(page["entries"][0]["id"], reply["id"]);
    let old = ok(call(&p, &b, "export", json!({"full":true})));
    assert_eq!(old["tombstones"], json!([root["id"]]));
    assert_eq!(old["records"], json!([]));
}
