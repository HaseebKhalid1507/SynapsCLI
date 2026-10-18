//! Tool-level tests for `multi_edit`: real files in a temp dir, never the
//! user's files. Cases A.1-A.9 refer to the real-usage call shapes.

use super::*;
use crate::tools::test_helpers::create_tool_context;
use serde_json::json;
use std::path::Path;

fn tmpdir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn p(path: &Path) -> String {
    path.to_str().unwrap().to_string()
}

async fn call(params: Value) -> std::result::Result<String, String> {
    MultiEditTool
        .execute(params, create_tool_context())
        .await
        .map_err(|e| e.to_string())
}

fn leftovers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("agent-tmp"))
        .collect()
}

fn fixed_now() -> chrono::DateTime<chrono::Local> {
    use chrono::TimeZone;
    chrono::Local
        .with_ymd_and_hms(2026, 10, 18, 11, 23, 45)
        .single()
        .unwrap()
}

fn run_with(params: Value, hooks: &mut dyn commit::Hooks) -> std::result::Result<String, String> {
    let req = parse_request(&params)?;
    run(req, fixed_now(), None, &|_: &str| false, hooks)
}

// A.1: a missing anchor anywhere refuses the whole call; no file changes.
#[tokio::test]
async fn missing_anchor_in_a_later_file_changes_no_file() {
    let d = tmpdir();
    let a = d.path().join("a.md");
    let b = d.path().join("b.md");
    std::fs::write(&a, "one\ntwo\n").unwrap();
    std::fs::write(&b, "three\n").unwrap();
    let err = call(json!({"edits": [
        {"path": p(&a), "op": "insert_before", "anchor": "two", "text": "X\n"},
        {"path": p(&b), "op": "replace", "anchor": "nope", "text": "Y"}
    ]}))
    .await
    .unwrap_err();
    assert!(err.contains("edits[1]"), "{err}");
    assert!(err.contains("0 matches"), "{err}");
    assert!(err.contains("no file was changed"), "{err}");
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "one\ntwo\n");
    assert_eq!(std::fs::read_to_string(&b).unwrap(), "three\n");
    assert!(leftovers(d.path()).is_empty());
}

// A.2: two matches are refused with the count unless occurrence is given.
#[tokio::test]
async fn ambiguous_anchor_names_the_edit_and_the_count() {
    let d = tmpdir();
    let a = d.path().join("q.md");
    std::fs::write(&a, "5. **W8**: x\n6. **W8**: y\n").unwrap();
    let err = call(json!({"edits": [
        {"path": p(&a), "op": "replace", "anchor": "**W8**: ", "text": "**W8 archive**: "}
    ]}))
    .await
    .unwrap_err();
    assert!(err.contains("edits[0]") && err.contains("matched 2 times"), "{err}");
    let out = call(json!({"edits": [
        {"path": p(&a), "op": "replace", "anchor": "**W8**: ", "occurrence": 2, "text": "**W8 archive**: "}
    ]}))
    .await
    .unwrap();
    assert!(out.contains("[0]") && out.contains("replace L2"), "{out}");
    assert_eq!(
        std::fs::read_to_string(&a).unwrap(),
        "5. **W8**: x\n6. **W8 archive**: y\n"
    );
}

// A.3: a re-run guarded by unless_present is a reported no-op.
#[tokio::test]
async fn rerun_with_unless_present_is_a_reported_noop() {
    let d = tmpdir();
    let q = d.path().join("MERGE-QUEUE.md");
    std::fs::write(&q, "16. old\n\n### U's heads\n").unwrap();
    let params = json!({"edits": [{
        "path": p(&q), "op": "insert_before", "anchor": "\n### U's heads",
        "text": "\n17. new item", "unless_present": "\n17. "
    }]});
    let first = call(params.clone()).await.unwrap();
    assert!(first.contains("1 file(s) written, 1 edit(s) applied"), "{first}");
    let after_first = std::fs::read_to_string(&q).unwrap();
    assert_eq!(after_first, "16. old\n\n17. new item\n### U's heads\n");
    let second = call(params).await.unwrap();
    assert!(second.contains("0 file(s) written, 0 edit(s) applied, 1 skipped"), "{second}");
    assert!(second.contains("skipped: unless_present found at L3"), "{second}");
    assert_eq!(std::fs::read_to_string(&q).unwrap(), after_first);
}

// A.5 + A.6: edits to one file apply in order, with one write; a later
// anchor may be text an earlier edit inserted.
#[tokio::test]
async fn same_file_edits_apply_in_order_and_see_earlier_edits() {
    let d = tmpdir();
    let s = d.path().join("STATUS.md");
    std::fs::write(&s, "# S\n## Overnight\n").unwrap();
    let out = call(json!({"edits": [
        {"path": p(&s), "op": "insert_before", "anchor": "## Overnight", "text": "## Today\n- a\n"},
        {"path": p(&s), "op": "insert_after", "anchor": "- a\n", "text": "- b\n"},
        {"path": p(&s), "op": "replace", "anchor": "# S", "text": "# STATUS"}
    ]}))
    .await
    .unwrap();
    assert!(out.contains("1 file(s) written, 3 edit(s) applied"), "{out}");
    assert!(out.contains("[0]") && out.contains("insert_before L2-3"), "{out}");
    assert!(out.contains("[1]") && out.contains("insert_after L4"), "{out}");
    assert_eq!(
        std::fs::read_to_string(&s).unwrap(),
        "# STATUS\n## Today\n- a\n- b\n## Overnight\n"
    );
}

// A.7: shell-looking text is written verbatim.
#[tokio::test]
async fn shell_metacharacters_are_written_verbatim() {
    let d = tmpdir();
    let f = d.path().join("n.md");
    std::fs::write(&f, "anchor\n").unwrap();
    let text = "$(rm -rf ~) `id` \\n \"dq\" 'sq' ${HOME} → ≥ ×\n";
    call(json!({"edits": [{"path": p(&f), "op": "insert_after", "anchor": "anchor\n", "text": text}]}))
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&f).unwrap(), format!("anchor\n{text}"));
}

// A.8: placeholders in a JSONL row stay valid JSON; one clock reading per
// call is shared by every file and by timestamp_field.
#[tokio::test]
async fn one_clock_reading_across_files_and_jsonl_rows() {
    let d = tmpdir();
    let ep = d.path().join("episodes.jsonl");
    let st = d.path().join("STATUS.md");
    std::fs::write(&ep, "{\"n\":1}\n").unwrap();
    std::fs::write(&st, "# T 09:00\n").unwrap();
    let out = call(json!({"placeholders": true, "edits": [
        {"path": p(&ep), "op": "append_jsonl", "timestamp_field": "ts",
         "value": {"note": "said \"hi\" at {{now:%H:%M}}\n", "also": "{{now_iso}}"}},
        {"path": p(&st), "op": "replace", "anchor_mode": "line_regex",
         "anchor": "^# T (\\S+)$", "text": "{{now_iso}}"}
    ]}))
    .await
    .unwrap();
    let rows: Vec<Value> = std::fs::read_to_string(&ep)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(rows.len(), 2);
    let ts = rows[1]["ts"].as_str().unwrap().to_string();
    assert_eq!(rows[1]["also"], ts.as_str());
    assert!(rows[1]["note"].as_str().unwrap().starts_with("said \"hi\" at "));
    assert_eq!(std::fs::read_to_string(&st).unwrap(), format!("# T {ts}\n"));
    assert!(out.contains(&format!("clock {ts}")), "{out}");
}

// A.9: a failure while finalizing the 2nd of 3 files restores the 1st and
// the report names what happened to each file.
#[test]
fn finalize_failure_restores_committed_files_and_names_each() {
    struct FailSecond;
    impl commit::Hooks for FailSecond {
        fn finalize(&mut self, index: usize, tmp: &Path, p: &commit::Pending) -> std::io::Result<()> {
            if index == 1 {
                return Err(std::io::Error::other("injected rename failure"));
            }
            commit::finalize(tmp, p)
        }
    }
    let d = tmpdir();
    let files: Vec<_> = ["a", "b", "c"].iter().map(|n| d.path().join(n)).collect();
    for f in &files {
        std::fs::write(f, "orig\n").unwrap();
    }
    let err = run_with(
        json!({"edits": files.iter().map(|f| json!({"path": p(f), "op": "append", "text": "new\n"})).collect::<Vec<_>>()}),
        &mut FailSecond,
    )
    .unwrap_err();
    assert!(err.contains("injected rename failure"), "{err}");
    assert!(err.contains(&format!("Committed then restored: {}", p(&files[0]))), "{err}");
    assert!(err.contains("Restore FAILED: none"), "{err}");
    assert!(err.contains(&format!("Never written: {}, {}", p(&files[1]), p(&files[2]))), "{err}");
    for f in &files {
        assert_eq!(std::fs::read_to_string(f).unwrap(), "orig\n", "{}", f.display());
    }
    assert!(leftovers(d.path()).is_empty(), "{:?}", leftovers(d.path()));
}

#[test]
fn a_file_changed_after_planning_is_refused_and_left_alone() {
    struct Interfere(PathBuf);
    impl commit::Hooks for Interfere {
        fn after_stage(&mut self) {
            std::fs::write(&self.0, "someone else\n").unwrap();
        }
    }
    let d = tmpdir();
    let a = d.path().join("a.md");
    let b = d.path().join("b.md");
    std::fs::write(&a, "a\n").unwrap();
    std::fs::write(&b, "b\n").unwrap();
    let err = run_with(
        json!({"edits": [
            {"path": p(&a), "op": "append", "text": "1\n"},
            {"path": p(&b), "op": "append", "text": "2\n"}
        ]}),
        &mut Interfere(b.clone()),
    )
    .unwrap_err();
    assert!(err.contains("changed since it was read"), "{err}");
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "a\n");
    assert_eq!(std::fs::read_to_string(&b).unwrap(), "someone else\n");
    assert!(leftovers(d.path()).is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn existing_mode_is_kept() {
    use std::os::unix::fs::PermissionsExt;
    let d = tmpdir();
    let f = d.path().join("run.sh");
    std::fs::write(&f, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o750)).unwrap();
    call(json!({"edits": [{"path": p(&f), "op": "append", "text": "true\n"}]}))
        .await
        .unwrap();
    let mode = std::fs::metadata(&f).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o750);
}

#[cfg(unix)]
#[test]
#[serial_test::serial(umask)]
fn new_files_get_0666_minus_umask() {
    use std::os::unix::fs::PermissionsExt;
    struct Umask(libc::mode_t);
    impl Drop for Umask {
        fn drop(&mut self) {
            // SAFETY: umask has no preconditions.
            unsafe { libc::umask(self.0) };
        }
    }
    let d = tmpdir();
    for (mask, want) in [(0o022, 0o644), (0o027, 0o640)] {
        // SAFETY: umask has no preconditions; restored by the guard.
        let _guard = Umask(unsafe { libc::umask(mask) });
        let f = d.path().join(format!("new-{mask:o}.jsonl"));
        run_with(
            json!({"edits": [{"path": p(&f), "op": "append_jsonl", "create": true, "value": {"a": 1}}]}),
            &mut commit::RealHooks,
        )
        .unwrap();
        let mode = std::fs::metadata(&f).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, want, "umask {mask:o}");
    }
}

#[tokio::test]
async fn creating_needs_create_and_an_append_op() {
    let d = tmpdir();
    let f = d.path().join("new.md");
    let err = call(json!({"edits": [{"path": p(&f), "op": "append", "text": "x\n"}]}))
        .await
        .unwrap_err();
    assert!(err.contains("does not exist"), "{err}");
    let err = call(json!({"edits": [{"path": p(&f), "op": "replace", "anchor": "a", "text": "b", "create": true}]}))
        .await
        .unwrap_err();
    assert!(err.contains("create is only for append"), "{err}");
    assert!(!f.exists());
    let out = call(json!({"edits": [
        {"path": p(&f), "op": "append", "text": "x\n", "create": true},
        {"path": p(&f), "op": "append", "text": "y\n"}
    ]}))
    .await
    .unwrap();
    assert_eq!(out.matches("(created)").count(), 1, "{out}");
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "x\ny\n");
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_stays_a_link_and_its_target_is_edited() {
    let d = tmpdir();
    let real = d.path().join("real.md");
    let link = d.path().join("link.md");
    std::fs::write(&real, "a\n").unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    call(json!({"edits": [{"path": p(&link), "op": "append", "text": "b\n"}]}))
        .await
        .unwrap();
    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read_to_string(&real).unwrap(), "a\nb\n");

    let dangling = d.path().join("dangling.md");
    std::os::unix::fs::symlink(d.path().join("void"), &dangling).unwrap();
    let err = call(json!({"edits": [{"path": p(&dangling), "op": "append", "text": "x", "create": true}]}))
        .await
        .unwrap_err();
    assert!(err.contains("dangling symlink"), "{err}");
}

/// A symlink outside a worker's write scope must not redirect a foreground
/// write into it: the real target is checked as well as the path given.
#[cfg(unix)]
#[tokio::test]
async fn a_symlink_into_a_worker_scope_is_refused() {
    let d = tmpdir();
    let root = std::fs::canonicalize(d.path()).unwrap();
    let scope = root.join("scope");
    let outside = root.join("outside");
    std::fs::create_dir_all(&scope).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let real = scope.join("owned.md");
    std::fs::write(&real, "worker\n").unwrap();
    let link = outside.join("alias.md");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let ctx = create_tool_context();
    ctx.capabilities
        .orchestration
        .as_ref()
        .unwrap()
        .authorize_with_policy(
            "sa_scope",
            "anthropic/claude-sonnet-4-6",
            agent_core::orchestration::WorkerRole::Implementer,
            agent_core::orchestration::WorkerWritePolicy::NonOverlappingPaths(vec![format!(
                "{}/**",
                scope.display()
            )]),
        )
        .unwrap();
    let err = MultiEditTool
        .execute(
            json!({"edits": [{"path": p(&link), "op": "append", "text": "foreground\n"}]}),
            ctx,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("worker's write scope"), "{err}");
    assert_eq!(std::fs::read_to_string(&real).unwrap(), "worker\n");
}

#[cfg(unix)]
#[tokio::test]
async fn hard_linked_files_are_refused() {
    let d = tmpdir();
    let a = d.path().join("a.md");
    std::fs::write(&a, "x\n").unwrap();
    std::fs::hard_link(&a, d.path().join("b.md")).unwrap();
    let err = call(json!({"edits": [{"path": p(&a), "op": "append", "text": "y\n"}]}))
        .await
        .unwrap_err();
    assert!(err.contains("hard links"), "{err}");
}

#[tokio::test]
async fn limits_and_unknown_fields_are_refused_up_front() {
    let d = tmpdir();
    let f = d.path().join("f.md");
    std::fs::write(&f, "x\n").unwrap();
    let many: Vec<Value> = (0..=MAX_EDITS)
        .map(|_| json!({"path": p(&f), "op": "append", "text": "y"}))
        .collect();
    let err = call(json!({"edits": many})).await.unwrap_err();
    assert!(err.contains("at most 64"), "{err}");
    let files: Vec<Value> = (0..=MAX_FILES)
        .map(|n| {
            let path = d.path().join(format!("f{n}.md"));
            std::fs::write(&path, "x\n").unwrap();
            json!({"path": p(&path), "op": "append", "text": "y"})
        })
        .collect();
    let err = call(json!({"edits": files})).await.unwrap_err();
    assert!(err.contains("more than 16 files"), "{err}");
    let err = call(json!({"edits": [{"path": p(&f), "op": "replace", "anchor": "x", "text": "y", "ocurrence": 1}]}))
        .await
        .unwrap_err();
    assert!(err.contains("edits[0]") && err.contains("ocurrence"), "{err}");
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "x\n");
}

#[tokio::test]
async fn unknown_placeholder_fails_and_placeholders_off_is_literal() {
    let d = tmpdir();
    let f = d.path().join("f.md");
    std::fs::write(&f, "x\n").unwrap();
    let err = call(json!({"placeholders": true, "edits": [
        {"path": p(&f), "op": "append", "text": "{{now_isoo}}"}
    ]}))
    .await
    .unwrap_err();
    assert!(err.contains("edits[0]") && err.contains("unknown placeholder"), "{err}");
    call(json!({"edits": [{"path": p(&f), "op": "append", "text": "{{now_isoo}} }}"}]}))
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "x\n{{now_isoo}} }}");
}

#[test]
fn classified_as_a_non_idempotent_serialized_builtin() {
    let tool = MultiEditTool;
    assert_eq!(tool.name(), "multi_edit");
    assert_eq!(tool.effect(), crate::tools::catalog::ToolEffect::NonIdempotent);
    assert_eq!(
        tool.concurrency_key(&json!({})),
        Some(crate::tools::ConcurrencyKey::Serialize)
    );
    assert_eq!(tool.origin(), crate::tools::ToolOrigin::Builtin);
    assert_eq!(tool.parameters()["required"], json!(["edits"]));
}

#[test]
fn status_label_and_paths_tolerate_any_shape() {
    let input = json!({"edits": [
        {"path": "/w/a/STATUS.md"}, {"path": "/w/b.jsonl"}, {"path": "/w/a/STATUS.md"},
        {"no": "path"}, 7
    ]});
    assert_eq!(paths(&input), ["/w/a/STATUS.md", "/w/b.jsonl"]);
    assert_eq!(label(&input), "editing STATUS.md +1 files");
    assert_eq!(label(&json!({"edits": [{"path": "x.md"}]})), "editing x.md");
    assert_eq!(label(&json!({"edits": "nope"})), "editing ?");
    assert_eq!(label(&json!(null)), "editing ?");
}

#[cfg(unix)]
#[test]
fn staging_never_follows_reuses_or_removes_an_existing_temp_path() {
    let d = tmpdir();
    let victim = d.path().join("victim.md");
    std::fs::write(&victim, "keep\n").unwrap();
    let tmp = d.path().join("planted.agent-tmp");
    std::os::unix::fs::symlink(&victim, &tmp).unwrap();
    let err = commit::write_new(&tmp, b"new\n", None).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep\n");
    assert!(
        std::fs::symlink_metadata(&tmp).unwrap().file_type().is_symlink(),
        "a path this call did not create is left in place"
    );
}

// unless_present is checked against the content as earlier edits in the
// same call left it, not against the file as read.
#[tokio::test]
async fn unless_present_sees_earlier_edits_in_the_same_call() {
    let d = tmpdir();
    let f = d.path().join("q.md");
    std::fs::write(&f, "a\n").unwrap();
    let out = call(json!({"edits": [
        {"path": p(&f), "op": "append", "text": "17. x\n"},
        {"path": p(&f), "op": "append", "text": "17. x\n", "unless_present": "17. "}
    ]}))
    .await
    .unwrap();
    assert!(out.contains("1 edit(s) applied, 1 skipped"), "{out}");
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "a\n17. x\n");
}

#[tokio::test]
async fn timestamp_field_never_overwrites_a_given_field() {
    let d = tmpdir();
    let f = d.path().join("e.jsonl");
    std::fs::write(&f, "").unwrap();
    let err = call(json!({"edits": [
        {"path": p(&f), "op": "append_jsonl", "timestamp_field": "ts", "value": {"ts": "mine"}}
    ]}))
    .await
    .unwrap_err();
    assert!(err.contains("edits[0]") && err.contains("`ts` is already in value"), "{err}");
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "");
}
