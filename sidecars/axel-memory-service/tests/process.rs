use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::fs::{symlink, PermissionsExt},
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
            assert!(s.len() < MAX_FRAME);
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
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join("synthetic.r8");
    (dir, path)
}
#[test]
fn hello_is_immediate_and_does_not_touch_brain() {
    let (dir, path) = temp();
    let mut child = Command::new(env!("CARGO_BIN_EXE_synaps-axel-memory-service"))
        .args(["--brain", path.to_str().unwrap(), "--project", PROJECT])
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{}", envelope(PROJECT, "hello", json!({}))).unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap()["result"]["revision"],
        REVISION
    );
    assert!(!path.exists());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    writeln!(stdin, "{}", envelope(PROJECT, "search", json!({}))).unwrap();
    line.clear();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap()["result"],
        json!([])
    );
    assert!(child.wait().unwrap().success());
}
#[test]
fn full_roundtrip_short_empty_sensitive_and_reopen() {
    let (_dir, path) = temp();
    for (id, body, class) in [
        ("mem-a", "x", "normal"),
        ("mem-b", "", "normal"),
        ("mem-c", "Sensitive Ünicode body", "sensitive"),
    ] {
        let mut r = record(id, body);
        r["sensitivity"] = json!(class);
        assert_eq!(call(&path, "store", r.clone())["result"], r);
        assert_eq!(
            call(&path, "fetch", json!({"ids":[id]}))["result"],
            json!([r])
        );
    }
    let c = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        c.query_row(
            "SELECT sensitivity FROM memories WHERE id='mem-c'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "secret"
    );
    for (term, want) in [("Sensitive", 0), ("x", 1)] {
        let count: i64 = c
            .query_row(
                "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH ?1",
                [term],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, want);
    }
}
#[test]
fn secret_body_and_title_are_never_indexed_or_disclosed() {
    let (_dir, path) = temp();
    let mut r = record("mem-secret", "UniqueSecretNeedle");
    r["sensitivity"] = json!("secret");
    r["meta"] = json!({"title":"UniqueTitleNeedle"});
    let mut expected = r.clone();
    expected["content"] = json!("");
    assert_eq!(call(&path, "store", r)["result"], expected);
    assert_eq!(
        call(&path, "fetch", json!({"ids":["mem-secret"]}))["result"],
        json!([expected])
    );
    for needle in ["UniqueSecretNeedle", "UniqueTitleNeedle", ""] {
        assert_eq!(
            call(&path, "search", json!({"content_contains":needle}))["result"],
            json!([])
        );
    }
    let d = call(&path, "search", json!({}));
    assert_eq!(d["result"][0]["snippet"], "");
    assert_eq!(d["result"][0]["truncated"], false);
    assert_eq!(d["result"][0]["content_bytes"], 18);
    let c = rusqlite::Connection::open(&path).unwrap();
    for term in ["UniqueSecretNeedle", "UniqueTitleNeedle"] {
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH ?1",
                [term],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }
}
#[test]
fn literal_unicode_filters_and_utf8_byte_bounds() {
    let (_dir, path) = temp();
    let mut r = record("mem-unicode", "ÉCOLE %_ quoted OR and 日本語");
    r["timestamp_ms"] = json!(12345);
    assert_eq!(call(&path, "store", r)["ok"], true);
    let q = json!({"content_contains":"éCoLe %_", "tag_prefix":"literal_%","since_ms":12345,"until_ms":12345,"limit":99,"snippet_bytes":1});
    let d = call(&path, "search", q);
    assert_eq!(d["result"].as_array().unwrap().len(), 1);
    assert_eq!(d["result"][0]["snippet"], "");
    assert_eq!(d["result"][0]["truncated"], true);
    for q in [
        json!({"content_contains":"*"}),
        json!({"tag_prefix":"ä"}),
        json!({"since_ms":12346}),
        json!({"until_ms":12344}),
        json!({"limit":0}),
    ] {
        assert_eq!(call(&path, "search", q)["result"], json!([]));
    }
}
#[test]
fn ttl_is_absolute_and_fetch_is_all_or_nothing() {
    let (_dir, path) = temp();
    let mut old = record("mem-old", "expired");
    old["timestamp_ms"] = json!(1);
    old["retention"] = json!({"max_age_days":1});
    assert_eq!(call(&path, "store", old)["ok"], true);
    let live = record("mem-live", "live");
    call(&path, "store", live);
    for id in ["mem-old", "mem-missing"] {
        let r = call(&path, "fetch", json!({"ids":["mem-live",id]}));
        assert_eq!(r["error"]["code"], "not_found");
        assert!(r.get("result").is_none());
    }
    assert_eq!(
        call(&path, "search", json!({}))["result"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
#[test]
fn disclosure_gate_and_invalid_metadata_fail_closed() {
    let (_dir, path) = temp();
    call(&path, "store", record("mem-private", "withheld"));
    let c = rusqlite::Connection::open(&path).unwrap();
    c.execute("UPDATE memories SET retention='local_only'", [])
        .unwrap();
    drop(c);
    assert_eq!(
        call(&path, "fetch", json!({"ids":["mem-private"]}))["error"]["code"],
        "not_found"
    );
    assert_eq!(
        call(&path, "search", json!({"content_contains":"withheld"}))["result"],
        json!([])
    );
}
#[test]
fn tombstone_cannot_resurrect_and_other_scope_is_rejected() {
    let (_dir, path) = temp();
    let r = record("mem-once", "note");
    call(&path, "store", r.clone());
    assert_eq!(
        call_scope(&path, FOREIGN, "fetch", json!({"ids":["mem-once"]}))["error"]["code"],
        "not_found"
    );
    assert_eq!(
        call(&path, "forget", json!({"id":"mem-once"}))["result"],
        true
    );
    assert_eq!(
        call(&path, "forget", json!({"id":"mem-once"}))["result"],
        false
    );
    assert_eq!(call(&path, "store", r)["error"]["code"], "id_conflict");
    assert_eq!(call(&path, "search", json!({}))["result"], json!([]));
}
#[test]
fn wrong_envelope_payload_and_bad_protocol_do_not_create() {
    let (_dir, path) = temp();
    let input = format!("{}\n", envelope(PROJECT, "store", record("mem-a", "x")));
    assert_eq!(
        raw(&path, PROJECT, input.as_bytes())[0]["error"]["code"],
        "protocol_error"
    );
    assert!(!path.exists());
    let mut r = record("mem-a", "x");
    r["project"] = json!(FOREIGN);
    assert_eq!(call(&path, "store", r)["error"]["code"], "project_mismatch");
    assert!(!path.exists());
    let input = format!("{}\n", envelope(FOREIGN, "hello", json!({})));
    assert_eq!(
        raw(&path, PROJECT, input.as_bytes())[0]["error"]["code"],
        "project_mismatch"
    );
    assert!(!path.exists());
}
#[test]
fn framing_and_reply_caps() {
    let (_dir, path) = temp();
    assert_eq!(
        raw(&path, PROJECT, b"{not json}\n")[0]["error"]["code"],
        "invalid_request"
    );
    assert_eq!(
        raw(&path, PROJECT, &vec![b'x'; MAX_FRAME + 1])[0]["error"]["code"],
        "size_limit"
    );
    assert!(!path.exists());
    let bytes = synaps_axel_memory_service::protocol::reply_bytes(
        PROJECT,
        Ok(json!("x".repeat(MAX_FRAME))),
    );
    assert!(bytes.len() < MAX_FRAME);
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["error"]["code"],
        "size_limit"
    );
}
#[test]
fn legacy_brain_is_not_migrated_and_permissions_are_private() {
    let (_dir, path) = temp();
    let c = rusqlite::Connection::open(&path).unwrap();
    c.execute_batch("CREATE TABLE brain_meta(key TEXT,value TEXT); INSERT INTO brain_meta VALUES('meta','legacy');").unwrap();
    drop(c);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let before = fs::read(&path).unwrap();
    assert_eq!(
        call(&path, "search", json!({}))["error"]["code"],
        "unsupported_brain"
    );
    assert_eq!(before, fs::read(&path).unwrap());
    let (_dir, new) = temp();
    call(&new, "store", record("mem-perm", "x"));
    assert_eq!(
        fs::metadata(new).unwrap().permissions().mode() & 0o777,
        0o600
    );
}
#[test]
fn symlinks_and_public_parent_or_file_are_refused() {
    let (dir, path) = temp();
    let target = dir.path().join("target.r8");
    fs::write(&target, b"untouched").unwrap();
    symlink(&target, &path).unwrap();
    assert_eq!(
        call(&path, "search", json!({}))["error"]["code"],
        "unsafe_path"
    );
    assert_eq!(fs::read(&target).unwrap(), b"untouched");
    fs::remove_file(&path).unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        call(&path, "search", json!({}))["error"]["code"],
        "unsafe_path"
    );
    assert!(!path.exists());
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(&path, b"untouched").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        call(&path, "search", json!({}))["error"]["code"],
        "unsafe_path"
    );
}

#[test]
fn search_caps_and_full_body_limit_are_enforced() {
    let (_dir, path) = temp();
    for n in 0..27 {
        let mut r = record(&format!("mem-{n}"), &"日".repeat(200));
        r["timestamp_ms"] = json!(1000 + n);
        assert_eq!(call(&path, "store", r)["ok"], true);
    }
    let d = call(&path, "search", json!({"limit":1000,"snippet_bytes":1000}));
    let rows = d["result"].as_array().unwrap();
    assert_eq!(rows.len(), 25);
    assert_eq!(rows[0]["id"], "mem-26");
    for row in rows {
        assert_eq!(row["snippet"].as_str().unwrap().len(), 399);
        assert_eq!(row["truncated"], true);
    }
    assert_eq!(call(&path,"search",json!({"content_contains":null,"tag_prefix":null,"since_ms":null,"until_ms":null,"limit":null,"snippet_bytes":null}))["result"].as_array().unwrap().len(),8);
    assert_eq!(
        call(&path, "store", record("mem-long", &"x".repeat(16385)))["error"]["code"],
        "size_limit"
    );
    let max = record("mem-max", &"x".repeat(16384));
    assert_eq!(call(&path, "store", max.clone())["result"], max);
    assert_eq!(
        call(&path, "fetch", json!({"ids":["mem-max"]}))["result"],
        json!([max])
    );
}

#[test]
fn ancillary_and_parent_symlinks_are_refused_and_errors_are_safe() {
    let (dir, path) = temp();
    let target = dir.path().join("sensitive-canary");
    fs::write(&target, b"do not modify").unwrap();
    symlink(&target, dir.path().join("synthetic.r8-wal")).unwrap();
    let r = call(&path, "search", json!({}));
    assert_eq!(r["error"]["code"], "unsafe_path");
    assert!(!r.to_string().contains("canary"));
    assert!(!path.exists());
    let link = dir.path().join("link");
    symlink(dir.path(), &link).unwrap();
    assert_eq!(
        call(&link.join("other.r8"), "search", json!({}))["error"]["code"],
        "unsafe_path"
    );
}
