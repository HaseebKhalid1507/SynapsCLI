//! Synthetic, durable service-process coverage. No model/provider/network calls.
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use synaps_axel_memory_service::{contract::SCHEMA, forum_contract as forum};
const A: &str = "p0123456789abcdef";
const B: &str = "pfedcba9876543210";
fn temp() -> (tempfile::TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    fs::set_permissions(d.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let p = d.path().join("forum.r8");
    (d, p)
}
fn call(p: &Path, scope: &str, op: &str, payload: Value) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_synaps-axel-memory-service"))
        .env_clear()
        .args(["--brain", p.to_str().unwrap(), "--project", scope])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = format!(
        "{}\n{}\n",
        json!({"schema":SCHEMA,"project":scope,"operation":"hello","payload":{}}),
        json!({"schema":SCHEMA,"project":scope,"operation":op,"payload":payload})
    );
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()).unwrap());
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
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
fn author() -> forum::Author {
    forum::Author {
        actor: format!("actor-{}", "a".repeat(32)),
        group: format!("group-{}", "b".repeat(32)),
        parent: None,
    }
}
fn post(key: &str) -> forum::PostRequest {
    forum::PostRequest {
        author: author(),
        post: forum::Post {
            request_key: key.into(),
            thread_id: None,
            reply_to: None,
            title: "Synthetic finding".into(),
            body: "Independent useful synthetic evidence".into(),
            retention_days: 30,
        },
    }
}
fn send(p: &Path, scope: &str, q: &forum::PostRequest) -> forum::Receipt {
    serde_json::from_value(ok(call(p, scope, "forum_post", json!(q)))).unwrap()
}
fn read(p: &Path, scope: &str, q: forum::Read) -> forum::Page {
    let value = ok(call(p, scope, "forum_read", json!(q)));
    assert!(serde_json::to_vec(&value).unwrap().len() <= forum::PAGE_BYTES);
    let page: forum::Page = serde_json::from_value(value).unwrap();
    assert!(page.entries.iter().map(|e| e.body.len()).sum::<usize>() <= forum::PAGE_BODY_BYTES);
    for entry in &page.entries {
        entry.validate().unwrap();
    }
    assert!(page
        .entries
        .windows(2)
        .all(|w| w[0].cursor() < w[1].cursor()));
    if page.next.is_some() {
        assert_eq!(page.next, page.entries.last().map(forum::Entry::cursor));
    }
    page
}
fn thread(id: &str) -> forum::Read {
    forum::Read {
        thread_id: Some(id.into()),
        ..Default::default()
    }
}
fn reply(root: &str, key: &str) -> forum::PostRequest {
    let mut q = post(key);
    q.post.title.clear();
    q.post.thread_id = Some(root.into());
    q
}
fn migration(mut inventory: Value, key: char) -> Value {
    inventory["migration_id"] = json!(key.to_string().repeat(64));
    inventory["manifest_digest"] = json!("f".repeat(64));
    inventory
}
fn record(scope: &str, q: &forum::PostRequest, timestamp: u64) -> Value {
    let envelope = forum::Envelope::new(scope, q.author.clone(), &q.post).unwrap();
    json!({"namespace":"forum","timestamp_ms":timestamp,"content":q.post.body,"tags":[],
        "meta":{"_synaps_forum":envelope},"id":envelope.id(),"project":scope,
        "provenance":{"source":envelope.source(),"session":envelope.author.group},
        "sensitivity":"normal","retention":{"max_age_days":q.post.retention_days}})
}
#[test]
fn roots_replies_durable_duplicate_and_root_deletion() {
    let (_d, p) = temp();
    let q = post("root");
    let root = send(&p, A, &q);
    assert_eq!(root.status, forum::Status::Created);
    let retry = send(&p, A, &q);
    assert_eq!(retry.status, forum::Status::Duplicate);
    assert_eq!(retry.timestamp_ms, root.timestamp_ms);
    let mut r = reply(&root.id, "reply");
    r.post.reply_to = Some(root.id.clone());
    let receipt = send(&p, A, &r);
    let rows = read(&p, A, thread(&root.id));
    assert_eq!(rows.entries.len(), 2);
    assert!(rows.entries.iter().any(|e| e.id == receipt.id));
    assert_eq!(read(&p, A, forum::Read::default()).entries.len(), 1);
    assert_eq!(ok(call(&p, A, "forum_forget", json!({"id":root.id}))), true);
    assert!(read(&p, A, forum::Read::default()).entries.is_empty());
    assert_eq!(read(&p, A, thread(&root.id)).entries[0].id, receipt.id);
    assert_eq!(send(&p, A, &r).status, forum::Status::Duplicate);
    assert_eq!(send(&p, A, &q).status, forum::Status::Tombstoned);
    r.post.request_key = "new-reply".into();
    assert_eq!(
        call(&p, A, "forum_post", json!(r))["error"]["code"],
        "not_found"
    );
    // Identical body/source group, different content-addressed message source:
    // deleting root must not suppress an independent message with identical text.
    let separate = send(&p, A, &post("independent"));
    assert_eq!(separate.status, forum::Status::Created);
    assert_eq!(read(&p, A, forum::Read::default()).entries.len(), 1);
}
#[test]
fn ordinary_recall_isolation_and_reserved_injection() {
    let (_d, p) = temp();
    let root = send(&p, A, &post("root"));
    let mut normal = record(A, &post("fake"), 1);
    normal["id"] = json!("note");
    normal["namespace"] = json!("notes");
    normal["meta"] = json!({});
    normal["retention"] = json!("standard");
    ok(call(&p, A, "store", normal.clone()));
    for q in [
        json!({"limit":1}),
        json!({"content_contains":"synthetic","limit":1}),
    ] {
        let rows = ok(call(&p, A, "search", q));
        assert_eq!(rows.as_array().unwrap().len(), 1);
        assert_eq!(rows[0]["id"], "note");
    }
    assert_eq!(
        call(&p, A, "fetch", json!({"ids":[root.id]}))["error"]["code"],
        "not_found"
    );
    assert_eq!(
        call(&p, A, "forget", json!({"id":root.id}))["error"]["code"],
        "invalid_request"
    );
    for (field, value) in [
        ("namespace", json!("forum")),
        ("id", json!("msg-not-valid")),
        ("meta", json!({"_synaps_forum":null})),
    ] {
        let mut forged = normal.clone();
        forged[field] = value;
        assert_eq!(
            call(&p, A, "store", forged)["error"]["code"],
            "invalid_request"
        );
    }
    assert_eq!(read(&p, A, thread(&root.id)).entries.len(), 1);
}
#[test]
fn scope_and_reference_validation_are_atomic() {
    let (_d, p) = temp();
    let root = send(&p, A, &post("root"));
    let second = send(&p, A, &post("second"));
    let foreign = send(&p, B, &post("foreign"));
    assert!(read(&p, B, forum::Read::default())
        .entries
        .iter()
        .all(|e| e.envelope.project == B));
    assert!(read(&p, B, thread(&root.id)).entries.is_empty());
    assert!(read(&p, B, thread(&format!("msg-{}", "0".repeat(64))))
        .entries
        .is_empty());
    assert_eq!(
        call(&p, B, "forum_forget", json!({"id":root.id}))["ok"],
        false
    );
    for parent in [&foreign.id, &second.id, &format!("msg-{}", "0".repeat(64))] {
        let mut q = reply(&root.id, "bad-parent");
        q.post.reply_to = Some(parent.clone());
        assert_eq!(call(&p, A, "forum_post", json!(q))["ok"], false);
    }
    assert_eq!(
        call(&p, B, "forum_post", json!(reply(&root.id, "bad-thread")))["ok"],
        false
    );
    assert_eq!(read(&p, A, thread(&root.id)).entries.len(), 1);
    let inventory = json!({"records":[record(A, &post("root"), 1)]});
    assert_eq!(
        call(&p, B, "migration_apply", migration(inventory, 'a'))["ok"],
        false
    );
}
#[test]
fn full_export_import_and_tombstone_replay_preserve_forum() {
    let (_d, p) = temp();
    let q = post("root");
    let root = send(&p, A, &q);
    let r = reply(&root.id, "reply");
    send(&p, A, &r);
    let all = ok(call(&p, A, "export", json!({"full":true})));
    assert_eq!(all["records"].as_array().unwrap().len(), 2);
    for row in all["records"].as_array().unwrap() {
        assert_eq!(row["meta"]["_axel"]["disclosure"], "standard");
        assert_eq!(row["namespace"], "forum");
        assert_eq!(
            row["provenance"]["source"],
            format!("forum:{}", row["id"].as_str().unwrap())
        );
    }
    let (_d2, p2) = temp();
    ok(call(&p2, A, "migration_apply", migration(all.clone(), 'a')));
    assert_eq!(ok(call(&p2, A, "export", json!({"full":true}))), all);
    assert_eq!(send(&p2, A, &q).timestamp_ms, root.timestamp_ms);
    // Export annotations must not make a native->same-native replay conflict.
    ok(call(&p, A, "migration_apply", migration(all.clone(), 'b')));
    ok(call(&p, A, "forum_forget", json!({"id":root.id})));
    let gone = ok(call(&p, A, "export", json!({"full":true})));
    assert_eq!(gone["records"].as_array().unwrap().len(), 1);
    assert!(!serde_json::to_string(&gone["tombstones"])
        .unwrap()
        .contains("Synthetic"));
    ok(call(&p2, A, "migration_apply", migration(gone, 'c')));
    ok(call(&p2, A, "migration_apply", migration(all, 'd')));
    assert_eq!(send(&p2, A, &q).status, forum::Status::Tombstoned);
    assert_eq!(send(&p2, A, &r).status, forum::Status::Duplicate);
    assert_eq!(read(&p2, A, thread(&root.id)).entries.len(), 1);
}
#[test]
fn expired_replay_never_refreshes_and_sweep_uses_note_tombstones() {
    let (_d, p) = temp();
    let q = post("old-root");
    let envelope = forum::Envelope::new(A, q.author.clone(), &q.post).unwrap();
    let mut r = reply(&envelope.id(), "surviving-reply");
    r.post.retention_days = 365;
    let now = chrono::Utc::now().timestamp_millis() as u64;
    ok(call(
        &p,
        A,
        "migration_apply",
        migration(json!({"records":[record(A,&q,1), record(A,&r,now)]}), 'a'),
    ));
    assert!(read(&p, A, forum::Read::default()).entries.is_empty());
    assert_eq!(read(&p, A, thread(&envelope.id())).entries.len(), 1);
    let mut fresh = reply(&envelope.id(), "refused");
    fresh.post.retention_days = 365;
    assert_eq!(
        call(&p, A, "forum_post", json!(fresh))["error"]["code"],
        "not_found"
    );
    assert_eq!(send(&p, A, &q).status, forum::Status::Tombstoned);
    let inventory = ok(call(&p, A, "export", json!({"full":true})));
    assert!(inventory["tombstones"]
        .as_array()
        .unwrap()
        .contains(&json!(envelope.id())));
    let q2 = post("swept-root");
    ok(call(
        &p,
        A,
        "migration_apply",
        migration(json!({"records":[record(A,&q2,1)]}), 'b'),
    ));
    ok(call(&p, A, "sweep", json!({})));
    assert_eq!(send(&p, A, &q2).status, forum::Status::Tombstoned);
}
#[test]
fn tied_keyset_pages_byte_caps_unicode_and_poll_after_drain() {
    let (_d, p) = temp();
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let mut root = post("paging-root");
    root.post.body = "é".repeat(300);
    let envelope = forum::Envelope::new(A, root.author.clone(), &root.post).unwrap();
    let mut records = vec![record(A, &root, now)];
    for i in 0..20 {
        let mut q = reply(&envelope.id(), &format!("r{i}"));
        // Quotes/backslashes expand JSON past the body-only budget.
        q.post.body = if i % 2 == 0 {
            "\\\"".repeat(4096)
        } else {
            "é".repeat(4096)
        };
        records.push(record(A, &q, now));
    }
    let mut expected: Vec<String> = records
        .iter()
        .map(|r| r["id"].as_str().unwrap().into())
        .collect();
    expected.sort();
    ok(call(
        &p,
        A,
        "migration_apply",
        migration(json!({"records":records}), 'a'),
    ));
    let roots = read(&p, A, forum::Read::default());
    assert_eq!(roots.entries.len(), 1);
    assert!(roots.entries[0].truncated);
    assert_eq!(roots.entries[0].body.len(), forum::SNIPPET_BYTES);
    let mut query = thread(&envelope.id());
    query.limit = 16;
    let mut seen = Vec::new();
    let retained_cursor = loop {
        let page = read(&p, A, query.clone());
        assert!(!page.entries.is_empty());
        seen.extend(page.entries.iter().map(|e| e.id.clone()));
        if page.next.is_none() {
            break page.entries.last().unwrap().cursor();
        }
        query.after = page.next;
    };
    assert_eq!(seen, expected); // Never skips the record that exceeded a budget.
    query.after = Some(retained_cursor);
    assert!(read(&p, A, query.clone()).entries.is_empty());
    // next=None only means drained NOW. Retain last emitted cursor to poll.
    let appended = send(&p, A, &reply(&envelope.id(), "later"));
    assert_eq!(read(&p, A, query).entries[0].id, appended.id);
    let literal = forum::Read {
        query: Some("%_".into()),
        ..Default::default()
    };
    assert!(read(&p, A, literal).entries.is_empty());
}
#[test]
fn parallel_process_siblings_do_not_drop_posts() {
    let (_d, p) = temp();
    let root = send(&p, A, &post("root"));
    let workers: Vec<_> = (0..6)
        .map(|i| {
            let p = p.clone();
            let id = root.id.clone();
            std::thread::spawn(move || {
                let mut q = reply(&id, &format!("worker-{i}"));
                q.author = author().child();
                send(&p, A, &q).id
            })
        })
        .collect();
    let mut expected = vec![root.id.clone()];
    for worker in workers {
        expected.push(worker.join().unwrap());
    }
    expected.sort();
    let mut actual: Vec<_> = read(&p, A, thread(&root.id))
        .entries
        .into_iter()
        .map(|e| e.id)
        .collect();
    actual.sort();
    assert_eq!(actual, expected);
}
#[test]
fn strict_wire_fields_caps_and_invalid_imports_fail_closed() {
    let (_d, p) = temp();
    assert_eq!(
        ok(call(&p, A, "capabilities", json!({})))["forum"],
        json!({"schema":1})
    );
    assert!(!p.exists());
    for q in [
        json!({"limit":0}),
        json!({"limit":17}),
        json!({"limit":"8"}),
        json!({"query":null}),
        json!({"after":null}),
        json!({"thread_id":"msg-nope"}),
        json!({"extra":true}),
    ] {
        assert_eq!(call(&p, A, "forum_read", q)["ok"], false);
        assert!(!p.exists());
    }
    let mut q = json!(post("root"));
    q["post"]["thread_id"] = Value::Null;
    assert_eq!(call(&p, A, "forum_post", q)["ok"], false);
    assert!(!p.exists());
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let base = record(A, &post("root"), now);
    let mut forged = Vec::new();
    for (field, value) in [
        ("namespace", json!("notes")),
        ("content", json!("tamper")),
        ("tags", json!(["spoof"])),
        ("sensitivity", json!("secret")),
        ("retention", json!("standard")),
        ("project", json!(B)),
    ] {
        let mut r = base.clone();
        r[field] = value;
        forged.push(r);
    }
    for policy in [
        json!({"disclosure":"local_only"}),
        json!({"expires_ms":1}),
        json!({"repository":"forged"}),
        Value::Null,
    ] {
        let mut r = base.clone();
        r["meta"]["_axel"] = policy;
        forged.push(r);
    }
    let mut r = base.clone();
    r["meta"]["extra"] = json!(true);
    forged.push(r);
    let mut r = base.clone();
    r["meta"]["_synaps_forum"]["extra"] = json!(true);
    forged.push(r);
    let mut r = base.clone();
    r["meta"]["_synaps_forum"]["project"] = json!(B);
    forged.push(r);
    let mut r = base.clone();
    r["provenance"]["source"] = json!("forum");
    forged.push(r);
    let mut r = base.clone();
    r["provenance"]["session"] = json!("forged");
    forged.push(r);
    for r in forged {
        assert_eq!(
            call(
                &p,
                A,
                "migration_apply",
                migration(json!({"records":[r]}), 'a')
            )["ok"],
            false
        );
    }
    assert!(read(&p, A, forum::Read::default()).entries.is_empty());
    let mut valid = base;
    valid["meta"]["_axel"] = json!({"disclosure":"standard","expires_ms":now+30*86_400_000});
    ok(call(
        &p,
        A,
        "migration_apply",
        migration(json!({"records":[valid]}), 'b'),
    ));
    assert_eq!(read(&p, A, forum::Read::default()).entries.len(), 1);
}

#[test]
fn import_timestamp_replay_cannot_refresh_live_or_expired_message() {
    let (_d, p) = temp();
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let q = post("live-import");
    let first = now - 86_400_000;
    ok(call(
        &p,
        A,
        "migration_apply",
        migration(json!({"records":[record(A, &q, first)]}), 'a'),
    ));
    ok(call(
        &p,
        A,
        "migration_apply",
        migration(json!({"records":[record(A, &q, now)]}), 'b'),
    ));
    let receipt = send(&p, A, &q);
    assert_eq!(receipt.status, forum::Status::Duplicate);
    assert_eq!(receipt.timestamp_ms, Some(first));
    let expired = post("expired-import");
    ok(call(
        &p,
        A,
        "migration_apply",
        migration(json!({"records":[record(A, &expired, 1)]}), 'c'),
    ));
    ok(call(
        &p,
        A,
        "migration_apply",
        migration(json!({"records":[record(A, &expired, now)]}), 'd'),
    ));
    assert_eq!(send(&p, A, &expired).status, forum::Status::Tombstoned);
    // Descriptor export has deliberately cleared bodies, not importable posts.
    let descriptors = ok(call(&p, A, "export", json!({})));
    assert_eq!(descriptors["records"][0]["content"], "");
    assert_eq!(
        call(&p, A, "migration_apply", migration(descriptors, 'e'))["ok"],
        false
    );
}

#[test]
fn read_and_export_reject_forged_storage_and_metadata_only_decode_is_separate() {
    // Each mutation uses its own disposable brain. Service processes are closed
    // before direct synthetic corruption; production never grants this shortcut.
    for mutation in 0..7 {
        let (_d, p) = temp();
        let root = send(&p, A, &post("forged"));
        {
            let c = rusqlite::Connection::open(&p).unwrap();
            match mutation {
                0 => {
                    c.execute("UPDATE memories SET content=replace(content,'Independent','Tamperedxxx') WHERE id=?1", [&root.id]).unwrap();
                }
                1 => {
                    c.execute(
                        "UPDATE memories SET tags='[\"forged\"]' WHERE id=?1",
                        [&root.id],
                    )
                    .unwrap();
                }
                2 => {
                    c.execute("UPDATE memories SET provenance=json_set(provenance,'$.record.meta._synaps_forum.title','Different title') WHERE id=?1", [&root.id]).unwrap();
                }
                3 => {
                    c.execute("UPDATE memories SET provenance=json_set(provenance,'$.record.meta._axel',json('{\"repository\":\"spoof\"}')) WHERE id=?1", [&root.id]).unwrap();
                }
                4 => {
                    c.execute(
                        "UPDATE memories SET expires_at=NULL WHERE id=?1",
                        [&root.id],
                    )
                    .unwrap();
                }
                5 => {
                    c.execute("UPDATE memories SET provenance=json_set(provenance,'$.record.provenance.source','forum:forged') WHERE id=?1", [&root.id]).unwrap();
                }
                _ => {
                    c.execute("UPDATE memories SET provenance=json_set(provenance,'$.record.meta._synaps_forum.version',2) WHERE id=?1", [&root.id]).unwrap();
                }
            }
        }
        assert_eq!(
            call(&p, A, "forum_read", json!({"thread_id":root.id,"limit":8}))["ok"],
            false,
            "mutation {mutation}"
        );
        assert_eq!(
            call(&p, A, "export", json!({"full":true}))["ok"],
            false,
            "mutation {mutation}"
        );
        assert_eq!(ok(call(&p, A, "search", json!({}))), json!([]));
    }
}

#[test]
fn foreign_live_owner_wins_before_local_tombstone_reply() {
    let (_d, p) = temp();
    let q = post("collision");
    let root = send(&p, A, &q);
    {
        let c = rusqlite::Connection::open(&p).unwrap();
        c.execute("INSERT INTO synaps_scope_members VALUES(?1,?1)", [B])
            .unwrap();
        c.execute(
            "UPDATE memories SET project_key=?1 WHERE id=?2",
            rusqlite::params![B, root.id],
        )
        .unwrap();
        c.execute(
            "INSERT INTO memory_tombstones(id,project_key,deleted_at) VALUES(?1,?2,?3)",
            rusqlite::params![root.id, A, chrono::Utc::now().to_rfc3339()],
        )
        .unwrap();
    }
    assert_eq!(
        call(&p, A, "forum_post", json!(q))["error"]["code"],
        "id_conflict"
    );
    assert_eq!(
        call(
            &p,
            A,
            "migration_apply",
            migration(json!({"records":[record(A,&q,1)]}), 'a')
        )["error"]["code"],
        "id_conflict"
    );
    assert_eq!(
        call(&p, A, "forum_forget", json!({"id":root.id}))["error"]["code"],
        "id_conflict"
    );
}
