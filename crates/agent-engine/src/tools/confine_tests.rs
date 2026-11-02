//! `tools = confined`, the in-process layer: in a session whose sandbox
//! policy says `mode = enforce` and `tools = confined`, `write`, `edit` and
//! `multi_edit` write only in or below the policy's all-write directory
//! rules (by identity: symlinks, `..`, a new file through its parent), and
//! refuse every other target with the contact sentence and one
//! content-free log line. Owner rule 27c's refusal comes first and is
//! unchanged. Every tree is a scratch dir outside the baseline directories
//! (`test_scratch_parent`) with a fake `$HOME` and base dir; nothing here
//! writes under a real system path or touches a real `~/.synaps-cli`. No
//! kernel support needed.

use crate::sandbox::landlock::Unavailable;
use crate::sandbox::policy::{test_scratch_parent, KillSwitch, PolicyEnv};
use crate::sandbox::SandboxBinding;
use crate::tools::test_helpers::create_tool_context;
use crate::tools::{EditTool, MultiEditTool, Tool, WriteTool};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{symlink, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const CONTACT: &str = "Ask the Master's desk.";
const CONFINED: &str = "Refused by this session's sandbox (tools = confined)";
const OWNER_27C: &str = "Refused by owner rule 27c";

struct Fx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    env: PolicyEnv,
}

/// home/ ($HOME, with a 27c root: home/.synaps-cli/config), the base dir at
/// home/x/base, the lane the policy grants, and other/, which it doesn't.
fn fx() -> Fx {
    let tmp = tempfile::Builder::new().prefix("sbx-confine-tools-").tempdir_in(test_scratch_parent()).expect("tempdir");
    let root = fs::canonicalize(tmp.path()).expect("canonical tempdir");
    for d in ["home/x/base/sandbox/sessions", "home/.synaps-cli", "lane/sub", "other/inner"] {
        fs::create_dir_all(root.join(d)).expect("mkdir");
    }
    for (f, text) in [("home/.synaps-cli/config", "theme = dark\n"), ("lane/file.txt", "lane old\n"), ("other/file.txt", "other old\n")] {
        fs::write(root.join(f), text).expect("write");
    }
    let env = PolicyEnv { home: root.join("home"), base_dir: root.join("home/x/base") };
    let fx = Fx { _tmp: tmp, root, env };
    fx.policy("c1", &fx.confined(""));
    fx
}

impl Fx {
    fn r(&self, rel: &str) -> String {
        self.root.join(rel).display().to_string()
    }
    fn policy(&self, id: &str, text: &str) {
        fs::write(self.env.policy_path(id).expect("valid id"), text).expect("write policy");
    }
    fn confined(&self, extra: &str) -> String {
        format!("mode = enforce\ntools = confined\nwrite = {}\ncontact = {CONTACT}\n{extra}", self.r("lane"))
    }
    fn binding_abi(&self, id: &str, abi: Result<u32, Unavailable>) -> Arc<SandboxBinding> {
        SandboxBinding::with_env(id, self.env.clone(), Box::leak(Box::new(KillSwitch::new())), Some(abi))
    }
    fn binding(&self, id: &str) -> Arc<SandboxBinding> {
        self.binding_abi(id, Ok(7))
    }
    /// Every entry under `rel`: kind and content (a symlink's target).
    fn snapshot(&self, rel: &str) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        walk(&self.root, &self.root.join(rel), &mut out);
        out
    }
}

fn walk(root: &Path, p: &Path, out: &mut BTreeMap<String, String>) {
    let Ok(meta) = fs::symlink_metadata(p) else { return };
    let rel = p.strip_prefix(root).expect("under the root").display().to_string();
    if meta.file_type().is_symlink() {
        out.insert(rel, format!("symlink -> {}", fs::read_link(p).expect("readlink").display()));
    } else if meta.is_dir() {
        out.insert(rel, "dir".into());
        let mut names: Vec<PathBuf> = fs::read_dir(p).expect("read_dir").map(|e| e.expect("entry").path()).collect();
        names.sort();
        for n in names {
            walk(root, &n, out);
        }
    } else {
        out.insert(rel, format!("file ino={} {:?}", meta.ino(), fs::read(p).expect("read")));
    }
}

async fn call(b: Option<&Arc<SandboxBinding>>, tool: &str, input: Value) -> Result<String, String> {
    call_in(b, tool, input, None).await
}

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

fn assert_confined(what: &str, r: Result<String, String>) {
    match r {
        // (The tool error's display adds "Tool execution failed: " first;
        // multi_edit names the edit and ends "; no file was changed".)
        Err(e) if e.contains(CONFINED) && e.contains(CONTACT) => {}
        other => panic!("{what}: want the confined refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn confined_file_tools_write_inside_the_write_set() {
    let fx = fx();
    let b = fx.binding("c1");
    let runs = [
        ("write", write_in(&fx.r("lane/new.txt"))),
        ("write", write_in(&fx.r("lane/n1/n2/deep.txt"))),
        ("write", write_in(&fx.r("lane/sub/../up.txt"))),
        ("edit", edit_in(&fx.r("lane/file.txt"), "lane old")),
        ("multi_edit", append_in(&fx.r("lane/multi.txt"))),
        ("multi_edit", append_in(&fx.r("lane/sub/multi.txt"))),
    ];
    for (tool, input) in runs {
        let shown = input.to_string();
        call(Some(&b), tool, input).await.unwrap_or_else(|e| panic!("{tool} {shown}: {e}"));
    }
    for rel in ["lane/new.txt", "lane/n1/n2/deep.txt", "lane/up.txt", "lane/multi.txt", "lane/sub/multi.txt"] {
        assert_eq!(fs::read_to_string(fx.root.join(rel)).expect(rel), "NEW-CONTENT-SENTINEL\n", "{rel}");
    }
    assert!(fs::read_to_string(fx.root.join("lane/file.txt")).expect("edited").contains("NEW-CONTENT-SENTINEL"));
    // Relative to a session cwd inside the lane.
    call_in(Some(&b), "write", write_in("rel/new.txt"), Some(fx.root.join("lane"))).await.expect("relative write");
    assert!(fx.root.join("lane/rel/new.txt").is_file());
}

#[tokio::test]
async fn confined_file_tools_refuse_every_target_outside_and_change_nothing() {
    let fx = fx();
    let b = fx.binding("c1");
    symlink(fx.root.join("other"), fx.root.join("lane/to-other")).expect("symlink");
    symlink(fx.root.join("other/file.txt"), fx.root.join("lane/link-out")).expect("symlink");
    let before = (fx.snapshot("other"), fx.snapshot("lane"), fx.snapshot("home"));
    let runs = [
        ("write", write_in(&fx.r("other/x.txt"))),
        ("write", write_in(&fx.r("other/file.txt"))),
        ("write", write_in(&fx.r("other/new1/new2/x.txt"))),
        ("write", write_in(&fx.r("other/inner/x.txt"))),
        ("write", write_in(&fx.r("x.txt"))),
        // A symlinked parent and `..` out of the lane.
        ("write", write_in(&fx.r("lane/to-other/x.txt"))),
        ("write", write_in(&fx.r("lane/to-other/new/x.txt"))),
        ("write", write_in(&fx.r("lane/sub/../../other/x.txt"))),
        ("write", write_in(&fx.r("lane/../other/new/x.txt"))),
        ("edit", edit_in(&fx.r("other/file.txt"), "other old")),
        ("edit", edit_in(&fx.r("lane/to-other/file.txt"), "other old")),
        // multi_edit resolves a final symlink and writes the real file.
        ("multi_edit", append_in(&fx.r("other/file.txt"))),
        ("multi_edit", append_in(&fx.r("other/m.txt"))),
        ("multi_edit", append_in(&fx.r("lane/link-out"))),
        ("multi_edit", append_in(&fx.r("lane/to-other/m.txt"))),
    ];
    for (tool, input) in runs {
        let shown = input.to_string();
        assert_confined(&format!("{tool} {shown}"), call(Some(&b), tool, input).await);
    }
    // Relative to a session cwd outside it.
    assert_confined("relative", call_in(Some(&b), "write", write_in("rel.txt"), Some(fx.root.join("other"))).await);
    assert_eq!((fx.snapshot("other"), fx.snapshot("lane"), fx.snapshot("home")), before, "no file, no directory, no link changed");
}

#[tokio::test]
async fn confined_targets_are_judged_by_identity_not_by_name() {
    let fx = fx();
    let b = fx.binding("c1");
    // A symlink outside that leads into the lane: the real directory is the
    // lane's, as for Landlock.
    symlink(fx.root.join("lane"), fx.root.join("other/to-lane")).expect("symlink");
    call(Some(&b), "write", write_in(&fx.r("other/to-lane/via.txt"))).await.expect("write through a link into the lane");
    assert!(fx.root.join("lane/via.txt").is_file());
    // A final symlink in the lane that leads out is replaced, not followed
    // (`write`, as without a sandbox): the file outside is unchanged.
    symlink(fx.root.join("other/file.txt"), fx.root.join("lane/link-out")).expect("symlink");
    call(Some(&b), "write", write_in(&fx.r("lane/link-out"))).await.expect("replace the link");
    assert!(fs::symlink_metadata(fx.root.join("lane/link-out")).expect("stat").is_file());
    // A second name in the lane of a file outside: the tools replace the
    // name in the lane (temp file + rename), never the file outside.
    fs::hard_link(fx.root.join("other/file.txt"), fx.root.join("lane/hl")).expect("hard link");
    call(Some(&b), "write", write_in(&fx.r("lane/hl"))).await.expect("write the lane's name");
    fs::hard_link(fx.root.join("other/file.txt"), fx.root.join("lane/hl2")).expect("hard link");
    call(Some(&b), "edit", edit_in(&fx.r("lane/hl2"), "other old")).await.expect("edit the lane's name");
    fs::hard_link(fx.root.join("other/file.txt"), fx.root.join("lane/hl3")).expect("hard link");
    // multi_edit refuses a file with a second name on its own (as without a
    // sandbox), before any sandbox check.
    match call(Some(&b), "multi_edit", append_in(&fx.r("lane/hl3"))).await {
        Err(e) if e.contains("has 2 hard links") && !e.contains(CONFINED) => {}
        other => panic!("multi_edit of a second name: {other:?}"),
    }
    fs::remove_file(fx.root.join("lane/hl3")).expect("rm own scratch link");
    assert_eq!(fs::read_to_string(fx.root.join("other/file.txt")).expect("read"), "other old\n");
    assert_eq!(fs::metadata(fx.root.join("other/file.txt")).expect("stat").nlink(), 1, "every lane name was replaced");
}

#[tokio::test]
async fn a_file_rule_admits_no_file_tool_write() {
    // `write_file` grants the file to bash, not a directory: the tools'
    // temp file + rename would need the directory (Landlock refuses it too).
    let fx = fx();
    fx.policy("c2", &fx.confined(&format!("write_file = {}\n", fx.r("other/file.txt"))));
    let b = fx.binding("c2");
    assert_confined("write", call(Some(&b), "write", write_in(&fx.r("other/file.txt"))).await);
    assert_confined("edit", call(Some(&b), "edit", edit_in(&fx.r("other/file.txt"), "other old")).await);
    assert_eq!(fs::read_to_string(fx.root.join("other/file.txt")).expect("read"), "other old\n");
}

#[tokio::test]
async fn owner_rule_27c_keeps_its_refusal_and_line_under_confined() {
    let fx = fx();
    // A 27c root outside the write set: 27c is checked first and wins, for
    // a valid confined file and for a broken one (empty write set).
    fx.policy("bad", &format!("mode = enforce\ntools = confined\nwrite = {}\ncontact = {CONTACT}\n", fx.r("home/x/base")));
    let b1 = fx.binding("c1");
    let bad = fx.binding("bad");
    let cfg = fx.r("home/.synaps-cli/config");
    let (out, guard) = crate::sandbox::log_tests::daemon_log();
    let r = [
        call(Some(&b1), "write", write_in(&cfg)).await,
        call(Some(&b1), "edit", edit_in(&cfg, "dark")).await,
        call(Some(&b1), "multi_edit", append_in(&cfg)).await,
        call(Some(&bad), "write", write_in(&cfg)).await,
    ];
    drop(guard);
    for (i, r) in r.into_iter().enumerate() {
        match r {
            Err(e) if e.contains(OWNER_27C) && !e.contains(CONFINED) => {}
            other => panic!("call {i}: want the 27c refusal, got {other:?}"),
        }
    }
    let text = out.text();
    assert_eq!(text.lines().filter(|l| l.contains("sandbox.protected session=")).count(), 4, "{text}");
    assert!(!text.contains("sandbox.tool_refused"), "{text}");
    assert_eq!(fs::read_to_string(fx.root.join("home/.synaps-cli/config")).expect("read"), "theme = dark\n");
}

#[tokio::test]
async fn confined_refusals_log_one_line_each_without_content() {
    let fx = fx();
    let b = fx.binding("c1");
    let (out, guard) = crate::sandbox::log_tests::daemon_log();
    let refused = [
        call(Some(&b), "write", write_in(&fx.r("other/x.txt"))).await,
        call(Some(&b), "edit", edit_in(&fx.r("other/file.txt"), "other old")).await,
        call(Some(&b), "multi_edit", append_in(&fx.r("other/m.txt"))).await,
    ];
    // The control: an allowed write logs nothing.
    let allowed = call(Some(&b), "write", write_in(&fx.r("lane/ok.txt"))).await;
    drop(guard);
    for r in refused {
        assert_confined("logged call", r);
    }
    allowed.expect("allowed write");
    let text = out.text();
    let lines: Vec<&str> = text.lines().filter(|l| l.contains("sandbox.")).collect();
    let want = ["write", "edit", "multi_edit"].map(|t| format!("sandbox.tool_refused session=c1 tool={t} class=path"));
    assert_eq!(lines.len(), 3, "one line per refusal, no other:\n{text}");
    for (l, w) in lines.iter().zip(want.iter()) {
        assert!(l.contains(" WARN ") && l.ends_with(w.as_str()), "{l}");
    }
    assert!(!text.contains("NEW-CONTENT-SENTINEL") && !text.contains("other old") && !text.contains(&fx.r("other")), "{text}");
    assert!(!text.contains(CONTACT), "{text}");
}

#[tokio::test]
async fn without_the_key_or_with_the_kill_switch_the_tools_write_as_before() {
    for case in ["no key", "tools open", "kill switch", "invalid without the key"] {
        let fx = fx();
        let b = match case {
            "no key" => {
                fx.policy("p", &format!("mode = enforce\nwrite = {}\n", fx.r("lane")));
                fx.binding("p")
            }
            "tools open" => {
                fx.policy("p", &format!("mode = enforce\ntools = open\nwrite = {}\n", fx.r("lane")));
                fx.binding("p")
            }
            "kill switch" => {
                fs::write(fx.env.kill_switch(), b"").expect("touch OFF");
                fx.binding("c1")
            }
            _ => {
                fx.policy("p", "mode = enforce\nwrite = /\n");
                fx.binding("p")
            }
        };
        call(Some(&b), "write", write_in(&fx.r("other/x.txt"))).await.unwrap_or_else(|e| panic!("{case}: write: {e}"));
        call(Some(&b), "edit", edit_in(&fx.r("other/file.txt"), "other old")).await.unwrap_or_else(|e| panic!("{case}: edit: {e}"));
        call(Some(&b), "multi_edit", append_in(&fx.r("other/m.txt"))).await.unwrap_or_else(|e| panic!("{case}: multi_edit: {e}"));
    }
}

#[tokio::test]
async fn an_invalid_confined_file_admits_no_write_at_all() {
    let fx = fx();
    // Broken (the base dir is never grantable), and it says confined: the
    // tools stay confined, with an empty write set.
    fx.policy("bad", &format!("mode = enforce\ntools = confined\nwrite = {}\ncontact = {CONTACT}\n", fx.r("home/x/base")));
    let b = fx.binding("bad");
    let before = (fx.snapshot("other"), fx.snapshot("lane"));
    assert_confined("lane write", call(Some(&b), "write", write_in(&fx.r("lane/x.txt"))).await);
    assert_confined("other write", call(Some(&b), "write", write_in(&fx.r("other/x.txt"))).await);
    assert_confined("edit", call(Some(&b), "edit", edit_in(&fx.r("lane/file.txt"), "lane old")).await);
    assert_confined("multi_edit", call(Some(&b), "multi_edit", append_in(&fx.r("lane/m.txt"))).await);
    assert_eq!((fx.snapshot("other"), fx.snapshot("lane")), before);
}

#[tokio::test]
async fn the_path_check_holds_whatever_the_landlock_abi() {
    let fx = fx();
    for abi in [
        Err(Unavailable::NoLandlock),
        Err(Unavailable::Disabled),
        Ok(2),
        Err(Unavailable::Error(libc::EMFILE)),
        Err(Unavailable::UnsupportedOs),
    ] {
        let shown = format!("{abi:?}");
        let b = fx.binding_abi("c1", abi);
        assert_confined(&shown, call(Some(&b), "write", write_in(&fx.r("other/x.txt"))).await);
        call(Some(&b), "write", write_in(&fx.r("lane/ok.txt"))).await.unwrap_or_else(|e| panic!("{shown}: {e}"));
    }
    assert!(!fx.root.join("other/x.txt").exists());
}
