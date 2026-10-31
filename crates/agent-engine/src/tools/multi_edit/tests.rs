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
        fn finalize(&mut self, index: usize, s: &commit::Staged, p: &commit::Pending) -> commit::Published {
            if index == 1 {
                return Err(std::io::Error::other("injected rename failure"));
            }
            commit::Hooks::finalize(&mut commit::RealHooks, index, s, p)
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
    let err = commit::create_temp_for_test(d.path(), "planted.agent-tmp", b"new\n").unwrap_err();
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

// ---------------------------------------------------------------------------
// Filesystem hardening (review round 1): staging permissions, directory
// handles, non-regular files, no-clobber creation, input limits.

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

/// Every name in `dir`, sorted.
#[cfg(unix)]
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A file of `len` bytes that takes no disk space.
fn sparse(path: &Path, len: u64) {
    std::fs::File::create(path).unwrap().set_len(len).unwrap();
}

#[cfg(unix)]
fn mkfifo(path: &Path) {
    let c = std::ffi::CString::new(p(path)).unwrap();
    // SAFETY: `c` is a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
}

/// Records each temp's permission bits and size once its bytes are written.
#[cfg(unix)]
#[derive(Default)]
struct TempModes {
    seen: Vec<(u32, u64)>,
    fail_finalize_at: Option<usize>,
}

#[cfg(unix)]
impl commit::Hooks for TempModes {
    fn temp_written(&mut self, file: &std::fs::File) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let meta = file.metadata().unwrap();
        self.seen.push((meta.permissions().mode() & 0o7777, meta.len()));
        Ok(())
    }
    fn finalize(&mut self, index: usize, s: &commit::Staged, p: &commit::Pending) -> commit::Published {
        if self.fail_finalize_at == Some(index) {
            return Err(std::io::Error::other("injected rename failure"));
        }
        commit::Hooks::finalize(&mut commit::RealHooks, index, s, p)
    }
}

// An existing file's replacement temp, and a restore's temp, stay
// owner-only while their bytes are written; the final mode comes after.
#[cfg(unix)]
#[test]
fn existing_and_restore_temps_are_owner_only_until_written() {
    use std::os::unix::fs::PermissionsExt;
    let d = tmpdir();
    let a = d.path().join("a.md");
    let b = d.path().join("b.md");
    std::fs::write(&a, "a\n").unwrap();
    std::fs::write(&b, "b\n").unwrap();
    std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o640)).unwrap();
    std::fs::set_permissions(&b, std::fs::Permissions::from_mode(0o600)).unwrap();
    let edits = json!({"edits": [
        {"path": p(&a), "op": "append", "text": "1\n"},
        {"path": p(&b), "op": "append", "text": "2\n"}
    ]});
    let mut hooks = TempModes::default();
    run_with(edits.clone(), &mut hooks).unwrap();
    assert_eq!(hooks.seen, vec![(0o600, 4), (0o600, 4)]);
    assert_eq!((mode_of(&a), mode_of(&b)), (0o640, 0o600));
    // The 2nd file fails: the 1st is put back through an owner-only temp too.
    let mut hooks = TempModes {
        fail_finalize_at: Some(1),
        ..TempModes::default()
    };
    let err = run_with(edits, &mut hooks).unwrap_err();
    assert!(err.contains(&format!("Committed then restored: {}", p(&a))), "{err}");
    assert_eq!(hooks.seen, vec![(0o600, 6), (0o600, 6), (0o600, 4)]);
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "a\n1\n");
    assert_eq!(mode_of(&a), 0o640);
}

/// After staging (or, `late`, inside the first finalize, i.e. after the
/// re-check), moves `dir` to `moved`, puts a symlink to `denied` in its
/// place and plants a file under the staged temp's name in `denied`.
#[cfg(unix)]
struct SwapParent {
    dir: PathBuf,
    moved: PathBuf,
    denied: PathBuf,
    late: bool,
    planted: Option<String>,
}

#[cfg(unix)]
impl SwapParent {
    fn new(root: &Path, late: bool) -> SwapParent {
        let swap = SwapParent {
            dir: root.join("allowed"),
            moved: root.join("moved"),
            denied: root.join("denied"),
            late,
            planted: None,
        };
        std::fs::create_dir(&swap.dir).unwrap();
        std::fs::create_dir(&swap.denied).unwrap();
        swap
    }

    fn swap(&mut self) {
        std::fs::rename(&self.dir, &self.moved).unwrap();
        std::os::unix::fs::symlink(&self.denied, &self.dir).unwrap();
        let temp = leftovers(&self.moved).pop().expect("a staged temp");
        std::fs::write(self.denied.join(&temp), "planted").unwrap();
        self.planted = Some(temp);
    }

    /// One edit; any path under `denied` overlaps a worker's write scope.
    fn call(&mut self, edit: Value) -> std::result::Result<String, String> {
        let req = parse_request(&json!({ "edits": [edit] })).unwrap();
        let denied = p(&self.denied);
        run(req, fixed_now(), None, &|s: &str| s.starts_with(&denied), self)
    }
}

#[cfg(unix)]
impl commit::Hooks for SwapParent {
    fn after_stage(&mut self) {
        if !self.late {
            self.swap();
        }
    }
    fn finalize(&mut self, index: usize, s: &commit::Staged, p: &commit::Pending) -> commit::Published {
        if self.late && self.planted.is_none() {
            self.swap();
        }
        commit::Hooks::finalize(&mut commit::RealHooks, index, s, p)
    }
}

// Review probe, adopted: the parent directory is swapped for a symlink into
// a refused directory after staging, with a file planted there under the
// temp's name. The call is refused; nothing is published anywhere, the
// planted file is left alone and no temp is left behind.
#[cfg(unix)]
#[test]
fn a_parent_directory_replaced_after_staging_is_refused() {
    let d = tmpdir();
    let mut swap = SwapParent::new(d.path(), false);
    let target = swap.dir.join("new.md");
    let err = swap
        .call(json!({"path": p(&target), "op": "append", "create": true, "text": "wanted"}))
        .unwrap_err();
    assert!(
        err.contains(&format!("the parent directory of {} was replaced", p(&target))),
        "{err}"
    );
    assert!(err.contains("no file was changed"), "{err}");
    let planted = swap.planted.clone().unwrap();
    assert_eq!(entries(&swap.denied), vec![planted.clone()]);
    assert_eq!(std::fs::read_to_string(swap.denied.join(&planted)).unwrap(), "planted");
    assert!(entries(&swap.moved).is_empty(), "{:?}", entries(&swap.moved));
}

// A parent that moves after the re-check (the documented unlocked window)
// cannot redirect the publish: it goes through the verified directory
// handle, so the file lands in the moved directory and the refused
// directory gets nothing.
#[cfg(unix)]
#[test]
fn publishing_goes_through_the_verified_directory_handle() {
    for existing in [false, true] {
        let d = tmpdir();
        let mut swap = SwapParent::new(d.path(), true);
        let target = swap.dir.join("t.md");
        if existing {
            std::fs::write(&target, "old\n").unwrap();
        }
        let out = swap
            .call(json!({"path": p(&target), "op": "append", "create": true, "text": "wanted\n"}))
            .unwrap();
        let want = if existing { "old\nwanted\n" } else { "wanted\n" };
        let landed = std::fs::read_to_string(swap.moved.join("t.md")).unwrap_or_default();
        assert_eq!(landed, want, "existing={existing}: {out}");
        let planted = swap.planted.clone().unwrap();
        assert_eq!(entries(&swap.denied), vec![planted.clone()], "existing={existing}");
        assert_eq!(std::fs::read_to_string(swap.denied.join(&planted)).unwrap(), "planted");
        assert_eq!(entries(&swap.moved), vec!["t.md".to_string()], "existing={existing}");
    }
}

// A staged temp that was replaced under the same name is never published
// or removed; the call is refused.
#[cfg(unix)]
#[test]
fn a_replaced_staged_temp_is_refused_and_left_alone() {
    struct ReplaceTemp(PathBuf);
    impl commit::Hooks for ReplaceTemp {
        fn after_stage(&mut self) {
            let temp = self.0.join(leftovers(&self.0).pop().unwrap());
            // Created first, then renamed over: a new inode, never a reused one.
            let other = self.0.join("other");
            std::fs::write(&other, "planted").unwrap();
            std::fs::rename(&other, &temp).unwrap();
        }
    }
    let d = tmpdir();
    let f = d.path().join("a.md");
    std::fs::write(&f, "orig\n").unwrap();
    let err = run_with(
        json!({"edits": [{"path": p(&f), "op": "append", "text": "new\n"}]}),
        &mut ReplaceTemp(d.path().to_path_buf()),
    )
    .unwrap_err();
    assert!(err.contains(&format!("the staged temp of {} was replaced", p(&f))), "{err}");
    assert!(err.contains("no file was changed"), "{err}");
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "orig\n");
    let left = leftovers(d.path());
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(std::fs::read_to_string(d.path().join(&left[0])).unwrap(), "planted");
}

/// Fails each temp's write once its bytes are in. With `replace`, another
/// file is first renamed over the temp's name.
#[cfg(unix)]
struct FailWrite {
    dir: PathBuf,
    replace: bool,
}

#[cfg(unix)]
impl commit::Hooks for FailWrite {
    fn temp_written(&mut self, _file: &std::fs::File) -> std::io::Result<()> {
        if self.replace {
            let temp = self.dir.join(leftovers(&self.dir).pop().expect("the temp being written"));
            // Created first, then renamed over: a new inode, never a reused one.
            let other = self.dir.join("other");
            std::fs::write(&other, "planted").unwrap();
            std::fs::rename(&other, &temp).unwrap();
        }
        Err(std::io::Error::other("injected write failure"))
    }
}

// A temp whose write fails is removed again, but only while its name still
// holds the file this call created: a file renamed over it is left alone.
#[cfg(unix)]
#[test]
fn a_temp_whose_write_fails_is_removed_only_while_it_is_ours() {
    for replace in [false, true] {
        let d = tmpdir();
        let f = d.path().join("a.md");
        std::fs::write(&f, "orig\n").unwrap();
        let mut hooks = FailWrite {
            dir: d.path().to_path_buf(),
            replace,
        };
        let err = run_with(
            json!({"edits": [{"path": p(&f), "op": "append", "text": "new\n"}]}),
            &mut hooks,
        )
        .unwrap_err();
        assert!(err.contains("injected write failure"), "{err}");
        assert!(err.contains("no file was changed"), "{err}");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "orig\n");
        let left = leftovers(d.path());
        if replace {
            assert_eq!(left.len(), 1, "the file renamed over the temp is kept: {left:?}");
            assert_eq!(std::fs::read_to_string(d.path().join(&left[0])).unwrap(), "planted");
        } else {
            assert!(left.is_empty(), "the failed temp is removed: {left:?}");
        }
    }
}

// A FIFO with no writer is refused at once: opening it never waits.
#[cfg(unix)]
#[test]
fn a_fifo_without_a_writer_is_refused_without_waiting() {
    let d = tmpdir();
    let fifo = d.path().join("pipe");
    mkfifo(&fifo);
    let (tx, rx) = std::sync::mpsc::channel();
    let path = p(&fifo);
    let worker = std::thread::spawn(move || {
        let edit = json!({"edits": [{"path": path, "op": "append", "text": "x"}]});
        let _ = tx.send(run_with(edit, &mut commit::RealHooks));
    });
    let Ok(result) = rx.recv_timeout(std::time::Duration::from_secs(1)) else {
        // Blocked in open: give it a writer so no thread is left behind.
        let writer = std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();
        worker.join().unwrap();
        drop(writer);
        panic!("loading a FIFO with no writer waited for more than 1 s");
    };
    worker.join().unwrap();
    let err = result.unwrap_err();
    assert!(err.contains("not a regular file"), "{err}");
    assert!(err.contains("no file was changed"), "{err}");
}

// A FIFO is refused before anything is read from it: bytes queued in it are
// still there afterwards.
#[cfg(target_os = "linux")]
#[test]
fn a_fifo_is_refused_before_anything_is_read() {
    use std::io::{Read, Write};
    use std::os::unix::fs::OpenOptionsExt;
    let d = tmpdir();
    let fifo = d.path().join("pipe");
    mkfifo(&fifo);
    // Linux: opening a FIFO read-write never blocks; it keeps a writer present.
    let mut holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)
        .unwrap();
    holder.write_all(b"queued").unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let path = p(&fifo);
    let worker = std::thread::spawn(move || {
        let edit = json!({"edits": [{"path": path, "op": "append", "text": "x"}]});
        let _ = tx.send(run_with(edit, &mut commit::RealHooks));
    });
    let Ok(result) = rx.recv_timeout(std::time::Duration::from_secs(1)) else {
        drop(holder); // end of file for a blocked reader
        worker.join().unwrap();
        panic!("loading a FIFO with a writer waited for more than 1 s");
    };
    worker.join().unwrap();
    let err = result.unwrap_err();
    assert!(err.contains("not a regular file"), "{err}");
    let mut buf = [0u8; 16];
    let n = holder.read(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"queued");
}

/// Declares the listed primitives unsupported.
struct NoPrimitive(Vec<commit::Primitive>);

impl commit::Hooks for NoPrimitive {
    fn unsupported(&mut self, primitive: commit::Primitive) -> bool {
        self.0.contains(&primitive)
    }
}

// With neither an exclusive rename nor a hard link, a new file is refused;
// it is never created by check-then-rename.
#[test]
fn creating_fails_closed_without_a_no_replace_primitive() {
    let d = tmpdir();
    let f = d.path().join("new.md");
    let err = run_with(
        json!({"edits": [{"path": p(&f), "op": "append", "create": true, "text": "x\n"}]}),
        &mut NoPrimitive(vec![commit::Primitive::RenameNoReplace, commit::Primitive::Link]),
    )
    .unwrap_err();
    assert!(
        err.contains(&format!(
            "cannot create {} without risking an overwrite on this filesystem",
            p(&f)
        )),
        "{err}"
    );
    assert!(!f.exists());
    assert!(leftovers(d.path()).is_empty(), "{:?}", leftovers(d.path()));
}

// A new file published by a hard link is published even when its temp name
// then can't be removed (a second link to the new file): the call succeeds
// and its result names the temp left behind.
#[cfg(unix)]
#[test]
fn a_temp_left_after_a_hard_link_publish_is_named_in_the_result() {
    use std::os::unix::fs::MetadataExt;
    let d = tmpdir();
    let f = d.path().join("new.md");
    let out = run_with(
        json!({"edits": [{"path": p(&f), "op": "append", "create": true, "text": "x\n"}]}),
        &mut NoPrimitive(vec![commit::Primitive::RenameNoReplace, commit::Primitive::UnlinkTemp]),
    )
    .unwrap();
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "x\n");
    let left = leftovers(d.path());
    assert_eq!(left.len(), 1, "the temp name is still there: {left:?}");
    let temp = std::fs::canonicalize(d.path()).unwrap().join(&left[0]);
    let ino = |path: &Path| std::fs::metadata(path).unwrap().ino();
    assert_eq!(ino(&temp), ino(&f), "the leftover is a link to the new file");
    assert!(
        out.contains(&format!("{} published; could not remove temp {}:", p(&f), p(&temp))),
        "{out}"
    );
    // Accounted as written, with exactly one warning, and the cleanup error.
    assert!(out.starts_with("multi_edit: 1 file(s) written, 1 edit(s) applied, 0 skipped"), "{out}");
    let target = std::fs::canonicalize(d.path()).unwrap().join("new.md");
    assert!(out.lines().any(|l| l == format!("written: {}", target.display())), "{out}");
    let warnings: Vec<&str> = out.lines().filter(|l| l.starts_with("warning: ")).collect();
    assert_eq!(warnings.len(), 1, "{out}");
    let unsupported = std::io::Error::from(std::io::ErrorKind::Unsupported).to_string();
    assert!(warnings[0].ends_with(&format!(": {unsupported}")), "{out}");
    assert_eq!(out.lines().last(), Some(warnings[0]), "{out}");
}

/// The 1st new file is published by a hard link whose temp can't be removed
/// (the existing seam); the 2nd file's finalize then fails. With `foreign`,
/// a file not made by this call takes the 1st temp's name just before. The
/// 2nd file's own temp is staged by then too, so the 1st file's temp is
/// found by that file's name, `first`.
#[cfg(unix)]
struct LeftoverThenFail {
    dir: PathBuf,
    first: &'static str,
    foreign: bool,
}

#[cfg(unix)]
impl commit::Hooks for LeftoverThenFail {
    fn finalize(&mut self, index: usize, s: &commit::Staged, p: &commit::Pending) -> commit::Published {
        if index == 1 {
            if self.foreign {
                let lead = format!("{}.", self.first);
                let left: Vec<String> =
                    leftovers(&self.dir).into_iter().filter(|n| n.starts_with(&lead)).collect();
                assert_eq!(left.len(), 1, "the 1st file's temp: {left:?}");
                let planted = self.dir.join("planted");
                std::fs::write(&planted, "foreign\n").unwrap();
                std::fs::rename(&planted, self.dir.join(&left[0])).unwrap();
            }
            return Err(std::io::Error::other("injected rename failure"));
        }
        commit::finalize(s, p, &mut |primitive| {
            matches!(
                primitive,
                commit::Primitive::RenameNoReplace | commit::Primitive::UnlinkTemp
            )
        })
    }
}

/// The temp a "<display> published; could not remove temp <temp>: <why>"
/// diagnostic in `text` names.
#[cfg(unix)]
fn named_temp(text: &str, display: &str) -> PathBuf {
    let lead = format!("{display} published; could not remove temp ");
    let start = text.find(&lead).unwrap_or_else(|| panic!("no diagnostic for {display}: {text}")) + lead.len();
    let len = text[start..].find(": ").unwrap_or_else(|| panic!("no cleanup error: {text}"));
    PathBuf::from(&text[start..start + len])
}

// A temp left over by an earlier file is not forgotten when a later file
// fails: the error carries its diagnostic, the identity-checked cleanup is
// tried again, and the error says it then went.
#[cfg(unix)]
#[test]
fn an_earlier_leftover_temp_is_reported_and_retried_when_a_later_file_fails() {
    let d = tmpdir();
    let dir = std::fs::canonicalize(d.path()).unwrap();
    let (a, b) = (d.path().join("a.md"), d.path().join("b.md"));
    let err = run_with(
        json!({"edits": [
            {"path": p(&a), "op": "append", "create": true, "text": "x\n"},
            {"path": p(&b), "op": "append", "create": true, "text": "y\n"}
        ]}),
        &mut LeftoverThenFail { dir: dir.clone(), first: "a.md", foreign: false },
    )
    .unwrap_err();
    assert!(err.contains("writing ") && err.contains("injected rename failure"), "{err}");
    assert!(err.contains(&format!("Committed then restored: {}", p(&a))), "{err}");
    assert!(err.contains("Restore FAILED: none"), "{err}");
    assert!(err.contains(&format!("Never written: {}", p(&b))), "{err}");
    // The earlier diagnostic, whole: the temp's name and the cleanup error.
    let lead = format!("{} published; could not remove temp ", p(&a));
    assert!(err.contains(&lead), "the earlier leftover is not reported: {err}");
    let temp = named_temp(&err, &p(&a));
    assert_eq!(temp.parent(), Some(dir.as_path()), "{err}");
    let unsupported = std::io::Error::from(std::io::ErrorKind::Unsupported).to_string();
    assert!(
        err.contains(&format!("could not remove temp {}: {unsupported}; removed on retry", temp.display())),
        "{err}"
    );
    // Both targets are absent again, and the retry removed the temp.
    assert!(!a.exists() && !b.exists());
    assert!(!temp.exists(), "{}", temp.display());
    assert!(leftovers(d.path()).is_empty(), "{:?}", leftovers(d.path()));
}

// The retry is identity-checked too: a file that took the temp's name in
// the meantime is left alone, and the error names it as still there.
#[cfg(unix)]
#[test]
fn a_taken_over_leftover_temp_is_left_alone_and_named_when_a_later_file_fails() {
    let d = tmpdir();
    let dir = std::fs::canonicalize(d.path()).unwrap();
    let (a, b) = (d.path().join("a.md"), d.path().join("b.md"));
    let err = run_with(
        json!({"edits": [
            {"path": p(&a), "op": "append", "create": true, "text": "x\n"},
            {"path": p(&b), "op": "append", "create": true, "text": "y\n"}
        ]}),
        &mut LeftoverThenFail { dir: dir.clone(), first: "a.md", foreign: true },
    )
    .unwrap_err();
    assert!(err.contains(&format!("Committed then restored: {}", p(&a))), "{err}");
    assert!(err.contains("Restore FAILED: none"), "{err}");
    assert!(err.contains(&format!("Never written: {}", p(&b))), "{err}");
    let lead = format!("{} published; could not remove temp ", p(&a));
    assert!(err.contains(&lead), "the earlier leftover is not reported: {err}");
    let temp = named_temp(&err, &p(&a));
    assert!(
        err.contains(&format!("could not remove temp {}: ", temp.display()))
            && err.contains("; still there: another file has taken its name; left as is"),
        "{err}"
    );
    assert!(!a.exists() && !b.exists());
    assert_eq!(std::fs::read_to_string(&temp).unwrap(), "foreign\n", "the foreign file is kept");
    assert_eq!(leftovers(d.path()).len(), 1, "{:?}", leftovers(d.path()));
}

/// File 0 is published cleanly; files 1 and 2 by hard links whose temps
/// can't be removed (the existing seam); file 3's finalize fails.
#[cfg(unix)]
struct WarnedThenFail;

#[cfg(unix)]
impl commit::Hooks for WarnedThenFail {
    fn finalize(&mut self, index: usize, s: &commit::Staged, p: &commit::Pending) -> commit::Published {
        match index {
            0 => commit::finalize(s, p, &mut |_| false),
            3 => Err(std::io::Error::other("injected rename failure")),
            _ => commit::finalize(s, p, &mut |primitive| {
                matches!(
                    primitive,
                    commit::Primitive::RenameNoReplace | commit::Primitive::UnlinkTemp
                )
            }),
        }
    }
}

// A clean first publication, then two files that each left a temp over, in
// two directories, then a failure: each leftover is reported and retried in
// its own file's directory, and the clean file is named in none of them.
#[cfg(unix)]
#[test]
fn earlier_leftovers_in_two_directories_are_each_retried_when_a_later_file_fails() {
    let d = tmpdir();
    std::fs::create_dir(d.path().join("p")).unwrap();
    std::fs::create_dir(d.path().join("q")).unwrap();
    let p_dir = std::fs::canonicalize(d.path().join("p")).unwrap();
    let q_dir = std::fs::canonicalize(d.path().join("q")).unwrap();
    let a = d.path().join("p").join("a.md");
    let b = d.path().join("p").join("b.md");
    let c = d.path().join("q").join("c.md");
    let e = d.path().join("q").join("e.md");
    let err = run_with(
        json!({"edits": [
            {"path": p(&a), "op": "append", "create": true, "text": "w\n"},
            {"path": p(&b), "op": "append", "create": true, "text": "x\n"},
            {"path": p(&c), "op": "append", "create": true, "text": "y\n"},
            {"path": p(&e), "op": "append", "create": true, "text": "z\n"}
        ]}),
        &mut WarnedThenFail,
    )
    .unwrap_err();
    assert!(err.contains(&format!("writing {} failed: injected rename failure", p(&e))), "{err}");
    assert!(
        err.contains(&format!("Committed then restored: {}, {}, {}.", p(&a), p(&b), p(&c))),
        "{err}"
    );
    assert!(err.contains("Restore FAILED: none"), "{err}");
    assert!(err.contains(&format!("Never written: {}.", p(&e))), "{err}");
    assert!(
        !err.contains(&format!("{} published; could not remove temp", p(&a))),
        "the clean first file is named as a leftover: {err}"
    );
    let unsupported = std::io::Error::from(std::io::ErrorKind::Unsupported).to_string();
    for (file, dir) in [(&b, &p_dir), (&c, &q_dir)] {
        let lead = format!("{} published; could not remove temp ", p(file));
        assert!(err.contains(&lead), "an earlier leftover is not reported: {err}");
        let temp = named_temp(&err, &p(file));
        assert_eq!(temp.parent(), Some(dir.as_path()), "{err}");
        assert!(
            err.contains(&format!(
                "could not remove temp {}: {unsupported}; removed on retry",
                temp.display()
            )),
            "{err}"
        );
        assert!(!temp.exists(), "a leftover not retried in its own directory: {}", temp.display());
    }
    assert!(!a.exists() && !b.exists() && !c.exists() && !e.exists());
    assert!(leftovers(&p_dir).is_empty(), "{:?}", leftovers(&p_dir));
    assert!(leftovers(&q_dir).is_empty(), "{:?}", leftovers(&q_dir));
}

/// Makes a directory inside a test's own TempDir read-only while held; put
/// back on drop, so the TempDir is removed even when the test panics.
#[cfg(unix)]
struct ReadOnlyDir(PathBuf);

#[cfg(unix)]
impl ReadOnlyDir {
    fn new(dir: &Path) -> Self {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        ReadOnlyDir(dir.to_path_buf())
    }
}

#[cfg(unix)]
impl Drop for ReadOnlyDir {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

/// The error a real unlink in read-only `dir` gets here, or `None` (with
/// one skip line) where a read-only directory doesn't stop it, e.g. as root.
#[cfg(unix)]
fn unlink_error_in_read_only(dir: &Path) -> Option<String> {
    let probe = dir.join("probe");
    std::fs::write(&probe, "").unwrap();
    let denied = {
        let _read_only = ReadOnlyDir::new(dir);
        std::fs::remove_file(&probe).err().map(|e| e.to_string())
    };
    let _ = std::fs::remove_file(&probe);
    if denied.is_none() {
        eprintln!("EACCES not real, case skipped: a read-only directory does not stop an unlink here");
    }
    denied
}

/// Publishes each new file by a hard link, its directory read-only from
/// just after the link until that finalize returns: the real
/// identity-checked unlink of the temp fails.
#[cfg(unix)]
struct UnlinkDenied(PathBuf);

#[cfg(unix)]
impl commit::Hooks for UnlinkDenied {
    fn finalize(&mut self, _: usize, s: &commit::Staged, p: &commit::Pending) -> commit::Published {
        let mut read_only = None;
        let published = commit::finalize(s, p, &mut |primitive| match primitive {
            commit::Primitive::RenameNoReplace => true,
            commit::Primitive::Link => false,
            commit::Primitive::UnlinkTemp => {
                read_only = Some(ReadOnlyDir::new(&self.0));
                false
            }
        });
        drop(read_only);
        published
    }
}

// The real unlink of each hard-link publish's temp fails, for two files in
// one call: both are published and counted, and each temp is named, in
// order, with the unlink's own error, as the last lines of the result.
#[cfg(unix)]
#[test]
fn temps_the_real_unlink_cannot_remove_are_each_named_on_success() {
    use std::os::unix::fs::MetadataExt;
    let d = tmpdir();
    let dir = std::fs::canonicalize(d.path()).unwrap();
    let Some(denied) = unlink_error_in_read_only(&dir) else {
        return;
    };
    let (a, b) = (d.path().join("a.md"), d.path().join("b.md"));
    let out = run_with(
        json!({"edits": [
            {"path": p(&a), "op": "append", "create": true, "text": "x\n"},
            {"path": p(&b), "op": "append", "create": true, "text": "y\n"}
        ]}),
        &mut UnlinkDenied(dir.clone()),
    )
    .unwrap();
    assert!(out.starts_with("multi_edit: 2 file(s) written, 2 edit(s) applied, 0 skipped"), "{out}");
    let warnings: Vec<&str> = out.lines().filter(|l| l.starts_with("warning: ")).collect();
    assert_eq!(warnings.len(), 2, "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(&lines[lines.len() - 2..], &warnings[..], "the warnings come last: {out}");
    for (warning, (f, bytes)) in warnings.iter().zip([(&a, "x\n"), (&b, "y\n")]) {
        assert_eq!(std::fs::read_to_string(f).unwrap(), bytes);
        let temp = named_temp(warning, &p(f));
        assert!(warning.ends_with(&format!(": {denied}")), "{warning}");
        assert_eq!(temp.parent(), Some(dir.as_path()), "{warning}");
        assert_eq!(std::fs::read_to_string(&temp).unwrap(), bytes);
        let ino = |path: &Path| std::fs::metadata(path).unwrap().ino();
        assert_eq!(ino(&temp), ino(f), "the leftover is a link to {}", f.display());
    }
    assert_eq!(leftovers(d.path()).len(), 2, "{:?}", leftovers(d.path()));
}

// Without the exclusive rename, a hard link still creates the file without
// replacing anything, and the temp name is removed.
#[test]
fn creating_falls_back_to_a_hard_link() {
    let d = tmpdir();
    let f = d.path().join("new.md");
    run_with(
        json!({"edits": [{"path": p(&f), "op": "append", "create": true, "text": "x\n"}]}),
        &mut NoPrimitive(vec![commit::Primitive::RenameNoReplace]),
    )
    .unwrap();
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "x\n");
    assert!(leftovers(d.path()).is_empty(), "{:?}", leftovers(d.path()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(std::fs::metadata(&f).unwrap().nlink(), 1);
    }
}

/// Counts reads of existing files.
#[derive(Default)]
struct CountReads(usize);

impl commit::Hooks for CountReads {
    fn before_read(&mut self) {
        self.0 += 1;
    }
}

// A file over the per-file limit is refused from its size, before any byte
// is read.
#[test]
fn an_oversized_file_is_refused_before_it_is_read() {
    let d = tmpdir();
    let f = d.path().join("big.md");
    sparse(&f, MAX_FILE_BYTES as u64 + 1);
    let mut reads = CountReads::default();
    let err = run_with(
        json!({"edits": [{"path": p(&f), "op": "append", "text": "x"}]}),
        &mut reads,
    )
    .unwrap_err();
    assert!(err.contains(&format!("larger than {MAX_FILE_BYTES} bytes")), "{err}");
    assert_eq!(reads.0, 0);
}

// A file that grows past the limit between its size check and the read is
// still refused: at most limit + 1 bytes are read.
#[test]
fn a_file_that_grows_after_its_size_check_is_refused() {
    struct Grow(PathBuf);
    impl commit::Hooks for Grow {
        fn before_read(&mut self) {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(&self.0).unwrap();
            f.write_all(b"z").unwrap();
        }
    }
    let d = tmpdir();
    let f = d.path().join("grows.md");
    sparse(&f, MAX_FILE_BYTES as u64);
    let err = run_with(
        json!({"edits": [{"path": p(&f), "op": "append", "text": "x"}]}),
        &mut Grow(f.clone()),
    )
    .unwrap_err();
    assert!(err.contains(&format!("larger than {MAX_FILE_BYTES} bytes")), "{err}");
    assert_eq!(std::fs::metadata(&f).unwrap().len(), MAX_FILE_BYTES as u64 + 1);
}

// The call's input limit counts the files read so far and is checked from
// the next file's size, before reading it.
#[test]
fn the_call_input_limit_is_checked_before_each_read() {
    let d = tmpdir();
    let mut edits = Vec::new();
    for i in 0..4 {
        let f = d.path().join(format!("f{i}.md"));
        sparse(&f, 15 * 1024 * 1024);
        // The files hold NUL bytes, so these edits are skipped.
        edits.push(json!({"path": p(&f), "op": "append", "text": "x", "unless_present": "\u{0}"}));
    }
    let last = d.path().join("f4.md");
    sparse(&last, 5 * 1024 * 1024);
    edits.push(json!({"path": p(&last), "op": "append", "text": "x"}));
    let mut reads = CountReads::default();
    let err = run_with(json!({ "edits": edits }), &mut reads).unwrap_err();
    assert!(err.starts_with("edits[4]"), "{err}");
    assert!(
        err.contains(&format!("the files this call reads would exceed {MAX_TOTAL_BYTES} bytes")),
        "{err}"
    );
    assert_eq!(reads.0, 4);
}

// Review of #268, gojo's footgun 1: an `exact` insert_after of a line lands
// on its own line, not glued into the anchor's line.
#[tokio::test]
async fn insert_after_an_exact_anchor_inserts_whole_lines_after_its_line() {
    let d = tmpdir();
    let m = d.path().join("m.txt");
    std::fs::write(&m, "line one\nline two\n").unwrap();
    let out = call(json!({"edits": [
        {"path": p(&m), "op": "insert_after", "anchor": "line one", "text": "line 1.5\n"}
    ]}))
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&m).unwrap(), "line one\nline 1.5\nline two\n");
    assert!(out.contains("insert_after L2 +9 B"), "{out}");
    // Mid-line anchor: moved to the line boundary, and the result says so.
    let out = call(json!({"edits": [
        {"path": p(&m), "op": "insert_before", "anchor": "two", "text": "line 1.75\n"}
    ]}))
    .await
    .unwrap();
    assert!(out.contains("inserted before the anchor's line"), "{out}");
    assert_eq!(
        std::fs::read_to_string(&m).unwrap(),
        "line one\nline 1.5\nline 1.75\nline two\n"
    );
}

// Footgun 2: `append` repairs a missing final newline like `append_jsonl`.
#[tokio::test]
async fn append_adds_the_missing_final_newline_and_says_so() {
    let d = tmpdir();
    let c = d.path().join("c.log");
    std::fs::write(&c, "x\ny").unwrap();
    let out = call(json!({"edits": [{"path": p(&c), "op": "append", "text": "z\n"}]}))
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&c).unwrap(), "x\ny\nz\n");
    assert!(out.contains("added missing newline before the text"), "{out}");
}

// Footgun 3: a CRLF file stays CRLF; an LF anchor still matches it.
#[tokio::test]
async fn a_crlf_file_stays_crlf() {
    let d = tmpdir();
    let f = d.path().join("app.ini");
    std::fs::write(&f, "[a]\r\nother = 2\r\n").unwrap();
    let params = json!({"edits": [
        {"path": p(&f), "op": "insert_after", "anchor": "other", "anchor_mode": "line_prefix",
         "text": "added = 3\n", "unless_present": "added = 3\n"},
        {"path": p(&f), "op": "replace", "anchor": "[a]\nother", "text": "[a]\n; note\nother"}
    ]});
    call(params.clone()).await.unwrap();
    let want = "[a]\r\n; note\r\nother = 2\r\nadded = 3\r\n";
    assert_eq!(std::fs::read_to_string(&f).unwrap(), want);
    // The guard is read as CRLF too, so a re-run skips instead of doubling.
    let again = call(json!({"edits": [params["edits"][0].clone()]})).await.unwrap();
    assert!(again.contains("1 skipped"), "{again}");
    assert_eq!(std::fs::read_to_string(&f).unwrap(), want);
}

// Footgun 4: every hard limit is in the description the model sees, and
// matches the constant that enforces it.
#[test]
fn the_description_states_every_hard_limit() {
    let desc = MultiEditTool.description();
    const MIB: usize = 1024 * 1024;
    for limit in [
        format!("{MAX_EDITS} edits"),
        format!("{MAX_FILES} files"),
        format!("{} MiB per file", MAX_FILE_BYTES / MIB),
        format!("{} MiB total", MAX_TOTAL_BYTES / MIB),
        format!("{} MiB per field", MAX_TEXT_BYTES / MIB),
        "UTF-8".to_string(),
    ] {
        assert!(desc.contains(&limit), "description lacks {limit:?}: {desc}");
    }
    assert_eq!(MultiEditTool.parameters()["properties"]["edits"]["maxItems"], json!(MAX_EDITS));
}

// Item 5: the schema is sent with every request. #268 shipped 1,824 bytes
// (395 description + 1,429 compact JSON schema), 747 tokens on Opus 5.5
// (`count_tokens`, against an `edit`-only baseline); the review cut it to
// 1,536 bytes, 671 tokens. Growing it again should be a decision.
#[test]
fn the_schema_stays_compact() {
    let tool = MultiEditTool;
    let bytes = tool.description().len() + serde_json::to_string(&tool.parameters()).unwrap().len();
    assert!(bytes <= 1_600, "multi_edit description + schema is {bytes} bytes");
}
