//! Owner rule 27c, the in-process layer: in a session whose sandbox policy
//! enforces, `write`, `edit` and `multi_edit` refuse JR's paths (the Jev
//! state dir, the Jev guard's config, the Synaps config, the base dir's
//! config and sandbox/), and write everything else exactly as they do
//! without a sandbox. Every tree is a scratch dir outside the baseline
//! directories (`test_scratch_parent`) with a fake `$HOME` and base dir;
//! nothing here touches a real `~/.synaps-cli`. No kernel support needed.

use crate::sandbox::policy::{test_scratch_parent, KillSwitch, PolicyEnv};
use crate::sandbox::SandboxBinding;
use crate::tools::test_helpers::create_tool_context;
use crate::tools::{EditTool, MultiEditTool, Tool, WriteTool};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const REFUSED: &str = "Refused by owner rule 27c";
const ONLY_JR: &str = "written only by JR's app or by JR";

struct Fx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    env: PolicyEnv,
}

/// A fake tree: home/ ($HOME) with the 27c roots under home/.synaps-cli,
/// the base dir at home/x/base (so `$HOME/.synaps-cli` is not the base
/// dir: the roots must be protected whatever the base dir is), a lane dir
/// the policy grants, and another dir it doesn't.
fn fx_with(jev: bool) -> Fx {
    let tmp = tempfile::Builder::new().prefix("sbx-27c-").tempdir_in(test_scratch_parent()).expect("tempdir");
    let root = fs::canonicalize(tmp.path()).expect("canonical tempdir");
    let mut dirs = vec!["home/x/base/sandbox/sessions", "home/.synaps-cli/memory/other", "home/.synaps-cli/plugins/jev/lib", "lane/sub", "other"];
    if jev {
        dirs.extend(["home/.synaps-cli/memory/jev/access/requests", "home/.synaps-cli/memory/jev/access/answers", "home/.synaps-cli/memory/jev/access/used"]);
    }
    for d in dirs {
        fs::create_dir_all(root.join(d)).expect("mkdir");
    }
    let mut files = vec![
        ("home/.synaps-cli/plugins/jev/config", "api_key = KEY-SENTINEL\n"),
        ("home/.synaps-cli/config", "theme = dark\n"),
        ("home/x/base/config", "theme = light\n"),
    ];
    if jev {
        files.push(("home/.synaps-cli/memory/jev/sandbox-mode", "manual\n"));
    }
    for (f, text) in files {
        fs::write(root.join(f), text).expect("write");
    }
    let env = PolicyEnv { home: root.join("home"), base_dir: root.join("home/x/base") };
    let fx = Fx { _tmp: tmp, root, env };
    fx.policy("s1", &format!("mode = enforce\nwrite = {}\n", fx.r("lane")));
    fx
}

fn fx() -> Fx {
    fx_with(true)
}

impl Fx {
    fn r(&self, rel: &str) -> String {
        self.root.join(rel).display().to_string()
    }
    fn policy(&self, id: &str, text: &str) {
        fs::write(self.env.policy_path(id).expect("valid id"), text).expect("write policy");
    }
    /// Session `id`'s binding, with a test-owned kill switch.
    fn binding(&self, id: &str) -> Arc<SandboxBinding> {
        SandboxBinding::with_env(id, self.env.clone(), Box::leak(Box::new(KillSwitch::new())), Some(Ok(7)))
    }
    /// Every entry under `rel`: kind, mode and content (a symlink's target).
    fn snapshot(&self, rel: &str) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        walk(&self.root, &self.root.join(rel), &mut out);
        out
    }
}

fn walk(root: &Path, p: &Path, out: &mut BTreeMap<String, String>) {
    let Ok(meta) = fs::symlink_metadata(p) else { return };
    let rel = p.strip_prefix(root).expect("under the root").display().to_string();
    if rel.ends_with(".agent-tmp") {
        return;
    }
    let mode = meta.mode() & 0o7777;
    if meta.file_type().is_symlink() {
        let to = fs::read_link(p).expect("readlink").display().to_string();
        out.insert(rel, format!("symlink -> {}", to.replace(&root.display().to_string(), "<root>")));
    } else if meta.is_dir() {
        out.insert(rel, format!("dir {mode:o}"));
        let mut names: Vec<PathBuf> = fs::read_dir(p).expect("read_dir").map(|e| e.expect("entry").path()).collect();
        names.sort();
        for n in names {
            walk(root, &n, out);
        }
    } else {
        out.insert(rel, format!("file {mode:o} {:?}", fs::read(p).expect("read")));
    }
}

async fn call(b: Option<&Arc<SandboxBinding>>, tool: &str, input: Value) -> Result<String, String> {
    call_in(b, tool, input, None).await
}

/// `call` in a session whose cwd is `cwd` (a relative path is resolved there).
async fn call_in(b: Option<&Arc<SandboxBinding>>, tool: &str, input: Value, cwd: Option<PathBuf>) -> Result<String, String> {
    let mut ctx = create_tool_context();
    ctx.capabilities.sandbox = b.cloned();
    ctx.capabilities.cwd = cwd;
    let r = match tool {
        "write" => WriteTool.execute(input, ctx).await,
        "edit" => EditTool.execute(input, ctx).await,
        "multi_edit" => MultiEditTool.execute(input, ctx).await,
        other => panic!("no tool {other}"),
    };
    r.map_err(|e| e.to_string())
}

fn write_in(path: &str) -> Value {
    json!({ "path": path, "content": "NEW-CONTENT-SENTINEL\n" })
}

fn edit_in(path: &str, old: &str) -> Value {
    json!({ "path": path, "old_string": old, "new_string": "NEW-CONTENT-SENTINEL" })
}

fn append_in(path: &str) -> Value {
    json!({ "edits": [{ "path": path, "op": "append", "text": "NEW-CONTENT-SENTINEL\n", "create": true }] })
}

/// The trees a refusal must leave exactly as they were.
fn protected_state(fx: &Fx) -> (BTreeMap<String, String>, BTreeMap<String, String>, BTreeMap<String, String>) {
    (fx.snapshot("home/.synaps-cli"), fx.snapshot("home/x/base/sandbox"), fx.snapshot("home/x/base/config"))
}

fn assert_refused(what: &str, r: Result<String, String>) {
    match r {
        Err(e) if e.contains(REFUSED) && e.contains(ONLY_JR) => {}
        other => panic!("{what}: want the 27c refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn protected_targets_are_refused_by_each_tool_in_an_enforcing_session() {
    let fx = fx();
    let b = fx.binding("s1");
    let before = protected_state(&fx);
    // (target, a string it holds now: `None` = it doesn't exist yet)
    let targets = [
        ("home/.synaps-cli/memory/jev/access/answers/a1.json", None),
        ("home/.synaps-cli/memory/jev/access/answers/new/deep.json", None),
        ("home/.synaps-cli/memory/jev/sandbox-mode", Some("manual")),
        ("home/.synaps-cli/memory/jev/new-file", None),
        ("home/.synaps-cli/plugins/jev/config", Some("KEY-SENTINEL")),
        ("home/.synaps-cli/config", Some("dark")),
        ("home/x/base/config", Some("light")),
        ("home/x/base/sandbox/sessions/s1.conf", Some("enforce")),
        ("home/x/base/sandbox/OFF", None),
        ("home/x/base/sandbox/sessions/other.conf", None),
    ];
    let mut bad = Vec::new();
    for (rel, holds) in targets {
        let p = fx.r(rel);
        let mut runs = vec![("write", write_in(&p))];
        // multi_edit never makes a directory: under a missing one it fails
        // on its own (the state check below shows nothing was made).
        if !rel.contains("/new/") {
            runs.push(("multi_edit", append_in(&p)));
        }
        if let Some(old) = holds {
            runs.push(("edit", edit_in(&p, old)));
        }
        for (tool, input) in runs {
            match call(Some(&b), tool, input).await {
                Err(e) if e.contains(REFUSED) && e.contains(ONLY_JR) => {}
                other => bad.push(format!("{tool} {rel}: {other:?}")),
            }
        }
    }
    assert!(bad.is_empty(), "{} not refused:\n{}", bad.len(), bad.join("\n"));
    assert_eq!(protected_state(&fx), before, "a refusal changes nothing (no file, no directory)");
}

#[tokio::test]
async fn sessions_that_dont_enforce_write_those_paths_as_before() {
    // No binding, a binding without a policy, `mode = off`, and the kill
    // switch: the tools behave exactly as without this rule.
    for case in ["no binding", "no policy", "mode off", "kill switch"] {
        let fx = fx();
        let b = match case {
            "no binding" => None,
            "no policy" => Some(fx.binding("s2")),
            "mode off" => {
                fx.policy("s3", "mode = off\n");
                Some(fx.binding("s3"))
            }
            _ => {
                fs::write(fx.env.kill_switch(), "").expect("create OFF");
                Some(fx.binding("s1"))
            }
        };
        let cfg = fx.r("home/.synaps-cli/plugins/jev/config");
        let answer = fx.r("home/.synaps-cli/memory/jev/access/answers/a1.json");
        call(b.as_ref(), "write", write_in(&answer)).await.unwrap_or_else(|e| panic!("{case}: write: {e}"));
        call(b.as_ref(), "edit", edit_in(&cfg, "KEY-SENTINEL")).await.unwrap_or_else(|e| panic!("{case}: edit: {e}"));
        call(b.as_ref(), "multi_edit", append_in(&fx.r("home/.synaps-cli/config"))).await.unwrap_or_else(|e| panic!("{case}: multi_edit: {e}"));
        assert_eq!(fs::read_to_string(&answer).expect("written"), "NEW-CONTENT-SENTINEL\n", "{case}");
        assert!(fs::read_to_string(&cfg).expect("edited").contains("NEW-CONTENT-SENTINEL"), "{case}");
    }
}

#[tokio::test]
async fn an_invalid_policy_fails_closed_for_protected_targets_only() {
    let fx = fx();
    fx.policy("s1", "mode = sideways\n");
    let b = fx.binding("s1");
    assert_refused("write the jev config", call(Some(&b), "write", write_in(&fx.r("home/.synaps-cli/plugins/jev/config"))).await);
    call(Some(&b), "write", write_in(&fx.r("other/fine.txt"))).await.expect("any other write works as before");
}

#[tokio::test]
async fn a_symlink_into_a_protected_root_is_refused() {
    let fx = fx();
    let b = fx.binding("s1");
    symlink(fx.root.join("home/.synaps-cli/memory/jev"), fx.root.join("lane/to-jev")).expect("symlink");
    symlink(fx.root.join("home/.synaps-cli/plugins/jev/config"), fx.root.join("lane/cfg")).expect("symlink");
    symlink(fx.root.join("home/x/base/sandbox"), fx.root.join("lane/to-sandbox")).expect("symlink");
    let before = protected_state(&fx);
    let lane_before = fx.snapshot("lane");
    let runs = [
        ("write", write_in(&fx.r("lane/to-jev/access/answers/s.json"))),
        ("write", write_in(&fx.r("lane/to-jev/access/answers/new/s.json"))),
        ("edit", edit_in(&fx.r("lane/to-jev/sandbox-mode"), "manual")),
        ("multi_edit", append_in(&fx.r("lane/to-jev/access/answers/s.json"))),
        ("write", write_in(&fx.r("lane/cfg"))),
        ("edit", edit_in(&fx.r("lane/cfg"), "KEY-SENTINEL")),
        ("multi_edit", append_in(&fx.r("lane/cfg"))),
        ("write", write_in(&fx.r("lane/to-sandbox/OFF"))),
        ("write", write_in(&fx.r("lane/sub/../to-jev/x.json"))),
    ];
    for (tool, input) in runs {
        let shown = input.to_string();
        assert_refused(&format!("{tool} {shown}"), call(Some(&b), tool, input).await);
    }
    assert_eq!(protected_state(&fx), before);
    assert_eq!(fx.snapshot("lane"), lane_before, "the symlinks themselves are left as they were");
}

#[tokio::test]
async fn a_missing_root_is_protected_by_the_name_it_would_get() {
    let fx = fx_with(false);
    let b = fx.binding("s1");
    assert_refused(
        "write under the missing jev dir",
        call(Some(&b), "write", write_in(&fx.r("home/.synaps-cli/memory/jev/access/answers/x.json"))).await,
    );
    assert_refused("write the missing jev dir's name", call(Some(&b), "write", write_in(&fx.r("home/.synaps-cli/memory/jev"))).await);
    assert!(!fx.root.join("home/.synaps-cli/memory/jev").exists(), "no directory was made in its place");
}

#[tokio::test]
async fn a_refused_write_under_missing_parents_makes_no_directory() {
    // memory/ and plugins/ are missing too: the whole target is checked
    // before the first directory is made, so the refusal ("Nothing was
    // changed") leaves no new directory on the way.
    let fx = fx_with(false);
    for d in ["home/.synaps-cli/memory", "home/.synaps-cli/plugins"] {
        fs::remove_dir_all(fx.root.join(d)).expect("remove own scratch dir");
    }
    let b = fx.binding("s1");
    let before = fx.snapshot("home");
    for rel in ["home/.synaps-cli/memory/jev/access/answers/x.json", "home/.synaps-cli/plugins/jev/config", "home/x/base/sandbox/new/s9.conf"] {
        assert_refused(rel, call(Some(&b), "write", write_in(&fx.r(rel))).await);
    }
    assert_eq!(fx.snapshot("home"), before, "no directory was made");
    assert!(!fx.root.join("home/.synaps-cli/memory").exists() && !fx.root.join("home/.synaps-cli/plugins").exists());
}

#[tokio::test]
async fn a_second_name_of_a_protected_file_is_refused() {
    let fx = fx();
    let b = fx.binding("s1");
    fs::hard_link(fx.root.join("home/.synaps-cli/plugins/jev/config"), fx.root.join("lane/hl")).expect("hard link");
    assert_refused("write", call(Some(&b), "write", write_in(&fx.r("lane/hl"))).await);
    assert_refused("edit", call(Some(&b), "edit", edit_in(&fx.r("lane/hl"), "KEY-SENTINEL")).await);
    assert_eq!(fs::read_to_string(fx.root.join("home/.synaps-cli/plugins/jev/config")).expect("read"), "api_key = KEY-SENTINEL\n");
}

#[tokio::test]
async fn refusals_log_one_line_each_without_content() {
    let fx = fx();
    let b = fx.binding("s1");
    let cfg = fx.r("home/.synaps-cli/plugins/jev/config");
    // This test's runtime is current-thread: the tools log on this thread,
    // where the capture is the default subscriber.
    let (out, guard) = crate::sandbox::log_tests::daemon_log();
    let refused = [
        call(Some(&b), "write", write_in(&cfg)).await,
        call(Some(&b), "edit", edit_in(&cfg, "KEY-SENTINEL")).await,
        call(Some(&b), "multi_edit", append_in(&cfg)).await,
    ];
    // The control: the same writes without a sandbox log no such line.
    let other = fx_with(true);
    let plain = call(None, "write", write_in(&other.r("home/.synaps-cli/plugins/jev/config"))).await;
    drop(guard);
    for r in refused {
        assert_refused("logged call", r);
    }
    plain.expect("unsandboxed write");
    let text = out.text();
    let lines: Vec<&str> = text.lines().filter(|l| l.contains("sandbox.protected")).collect();
    let tools: Vec<&str> = lines.iter().map(|l| l.rsplit("tool=").next().unwrap_or("")).collect();
    assert_eq!(tools, vec!["write", "edit", "multi_edit"], "one line per refusal, no other:\n{text}");
    assert!(lines.iter().all(|l| l.contains(" WARN ") && l.contains("sandbox.protected session=s1 tool=")), "{text}");
    assert!(!text.contains("NEW-CONTENT-SENTINEL") && !text.contains("KEY-SENTINEL"), "no content in the log:\n{text}");
    assert!(lines.iter().all(|l| !l.contains("plugins/jev/config")), "no path in the line:\n{text}");
}

/// One tree for the comparison: a lane with an executable, a read-only
/// file, symlinked final names, a directory symlink, a read-only dir.
fn parity_tree(fx: &Fx) {
    let lane = fx.root.join("lane");
    fs::write(lane.join("exec.sh"), "#!/bin/sh\necho old\n").expect("write");
    fs::set_permissions(lane.join("exec.sh"), fs::Permissions::from_mode(0o750)).expect("chmod own scratch file");
    fs::write(lane.join("private.txt"), "secret old\n").expect("write");
    fs::set_permissions(lane.join("private.txt"), fs::Permissions::from_mode(0o640)).expect("chmod own scratch file");
    fs::write(lane.join("real.txt"), "real old\n").expect("write");
    symlink("real.txt", lane.join("link.txt")).expect("symlink");
    fs::write(lane.join("real2.txt"), "real2 old\n").expect("write");
    symlink(lane.join("real2.txt"), lane.join("link2.txt")).expect("symlink");
    symlink(lane.join("gone.txt"), lane.join("dangling.txt")).expect("symlink");
    fs::write(lane.join("file.txt"), "plain\n").expect("write");
    symlink(fx.root.join("other"), lane.join("dlink")).expect("symlink");
    fs::create_dir(lane.join("ro")).expect("mkdir");
    fs::set_permissions(lane.join("ro"), fs::Permissions::from_mode(0o555)).expect("chmod own scratch dir");
    // A live directory whose real name ends like the kernel's mark for an
    // unlinked one.
    fs::create_dir(lane.join("notes (deleted)")).expect("mkdir");
    fs::write(lane.join("notes (deleted)/a.txt"), "notes old\n").expect("write");
    fs::set_permissions(lane.join("notes (deleted)/a.txt"), fs::Permissions::from_mode(0o640)).expect("chmod own scratch file");
}

/// Every non-protected case, as each tool gives it: (tool, input, the
/// session's cwd, for a relative path).
fn parity_cases(fx: &Fx) -> Vec<(&'static str, Value, Option<PathBuf>)> {
    let absolute = vec![
        ("write", write_in(&fx.r("lane/n1/n2/new.txt"))),
        ("write", write_in(&fx.r("lane/exec.sh"))),
        ("write", write_in(&fx.r("lane/link.txt"))),
        ("write", write_in(&fx.r("lane/dangling.txt"))),
        ("write", write_in(&fx.r("lane/dlink/through.txt"))),
        ("write", write_in(&fx.r("lane/sub/../dotdot.txt"))),
        ("write", write_in(&fx.r("lane/file.txt/x"))),
        ("write", write_in(&fx.r("lane/file.txt/a/b"))),
        ("write", write_in(&format!("{}/", fx.r("lane/slash")))),
        ("write", write_in(&fx.r("lane/ro/x.txt"))),
        ("write", write_in(&fx.r("lane/sub"))),
        ("write", write_in(&fx.r("lane/sub/.."))),
        ("edit", edit_in(&fx.r("lane/private.txt"), "old")),
        ("edit", edit_in(&fx.r("lane/link2.txt"), "old")),
        ("edit", edit_in(&fx.r("lane/missing.txt"), "old")),
        ("edit", edit_in(&fx.r("lane/nodir/missing.txt"), "old")),
        ("edit", edit_in(&fx.r("lane/real.txt"), "no such text")),
        ("edit", edit_in(&fx.r("lane/sub"), "old")),
        ("edit", edit_in(&fx.r("lane/dangling.txt"), "old")),
        ("multi_edit", append_in(&fx.r("lane/real.txt"))),
        ("multi_edit", append_in(&fx.r("lane/made-by-multi.txt"))),
        ("multi_edit", append_in(&fx.r("lane/link2.txt"))),
        // In a live directory named like an unlinked one.
        ("write", write_in(&fx.r("lane/notes (deleted)/w.txt"))),
        ("edit", edit_in(&fx.r("lane/notes (deleted)/a.txt"), "old")),
        ("multi_edit", append_in(&fx.r("lane/notes (deleted)/a.txt"))),
    ];
    let mut cases: Vec<_> = absolute.into_iter().map(|(tool, input)| (tool, input, None)).collect();
    // Relative to the session's cwd.
    let lane = Some(fx.root.join("lane"));
    cases.extend([
        ("write", write_in("rel/n/new.txt"), lane.clone()),
        ("edit", edit_in("file.txt", "plain"), lane.clone()),
        ("multi_edit", append_in("rel-multi.txt"), lane),
    ]);
    cases
}

#[tokio::test]
async fn enforcing_writes_elsewhere_match_the_unsandboxed_results() {
    let plain = fx();
    let boxed = fx();
    parity_tree(&plain);
    parity_tree(&boxed);
    let b = boxed.binding("s1");
    let norm = |fx: &Fx, s: String| s.replace(&fx.root.display().to_string(), "<root>");
    let mut diffs = Vec::new();
    for ((tool, a, cwd_a), (_, z, cwd_z)) in parity_cases(&plain).into_iter().zip(parity_cases(&boxed)) {
        let shown = norm(&plain, a.to_string());
        let ra = call_in(None, tool, a, cwd_a).await.map(|s| norm(&plain, s)).map_err(|s| norm(&plain, s));
        let rz = call_in(Some(&b), tool, z, cwd_z).await.map(|s| norm(&boxed, s)).map_err(|s| norm(&boxed, s));
        if ra != rz {
            diffs.push(format!("{tool} {shown}:\n  unsandboxed {ra:?}\n  enforcing   {rz:?}"));
        }
    }
    for fx in [&plain, &boxed] {
        fs::set_permissions(fx.root.join("lane/ro"), fs::Permissions::from_mode(0o755)).expect("chmod own scratch dir back");
    }
    assert!(diffs.is_empty(), "{} result(s) differ:\n{}", diffs.len(), diffs.join("\n"));
    for rel in ["lane", "other"] {
        assert_eq!(boxed.snapshot(rel), plain.snapshot(rel), "the {rel} trees end the same (names, kinds, modes, content)");
    }
}
