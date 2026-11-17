//! Pins for the write sandbox at the tool level: what each built-in tool
//! does for a session whose confinement is unresolved, or resolved around a
//! workspace. The policy itself is pinned in `sandbox::{policy, confinement,
//! guard}`; these pin that every tool honours it.
//!
//! Fixtures live under `/var/tmp` (the baseline grants `/tmp` whole).

use std::path::Path;

use serde_json::{json, Value};

use super::bash::BashTool;
use super::edit::EditTool;
use super::ls::LsTool;
use super::multi_edit::MultiEditTool;
use super::test_helpers::create_tool_context;
use super::write::WriteTool;
use super::{Tool, ToolContext};
use crate::sandbox::testkit::{appears, kernel_ok, Tree};
use crate::sandbox::{SessionSandbox, SpawnSandbox};

fn ctx(sandbox: SessionSandbox, cwd: &Path) -> ToolContext {
    let mut ctx = create_tool_context();
    ctx.capabilities.sandbox = sandbox;
    ctx.capabilities.cwd = Some(cwd.to_path_buf());
    ctx
}

/// Run `tool` and return its refusal text; panics if it succeeded.
async fn refusal(tool: &dyn Tool, params: Value, ctx: ToolContext) -> String {
    match tool.execute(params, ctx).await {
        Ok(out) => panic!("expected a refusal, the tool said: {out}"),
        Err(e) => e.to_string(),
    }
}

fn s(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn edit_call(path: &Path, old: &str, new: &str) -> Value {
    json!({"path": s(path), "old_string": old, "new_string": new})
}

fn multi_call(edits: &[(&Path, &str, &str)]) -> Value {
    let edits: Vec<Value> = edits
        .iter()
        .map(|(path, anchor, text)| json!({"path": s(path), "op": "replace", "anchor": anchor, "text": text}))
        .collect();
    json!({ "edits": edits })
}

/// A session confined by name only (confinement on, no resolved policy)
/// runs nothing and writes nothing: no tool, no file, no process.
#[tokio::test]
async fn an_unresolved_session_runs_and_writes_nothing() {
    let t = Tree::new();
    let file = t.ws.join("src/f.txt");
    std::fs::write(&file, "old").unwrap();
    let fresh = t.ws.join("src/fresh.txt");
    let marker = t.ws.join("src/marker");
    let unresolved = || ctx(SessionSandbox::Unresolved, &t.ws);

    let why = refusal(&WriteTool, json!({"path": s(&fresh), "content": "x"}), unresolved()).await;
    assert!(why.contains("no resolved policy"), "write: {why}");
    assert!(!fresh.exists());
    let why = refusal(&WriteTool, json!({"path": s(&file), "content": "x"}), unresolved()).await;
    assert!(why.contains("no resolved policy"), "write over: {why}");
    refusal(&EditTool, edit_call(&file, "old", "new"), unresolved()).await;
    refusal(&MultiEditTool, multi_call(&[(&file, "old", "new")]), unresolved()).await;
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "old", "no write tool changed the file");

    let why = refusal(&BashTool, json!({"command": format!("touch {}", s(&marker))}), unresolved()).await;
    assert!(why.contains("no resolved policy"), "bash: {why}");
    assert!(!marker.exists(), "the command never ran");
    refusal(&LsTool, json!({"path": s(&t.ws)}), unresolved()).await;
}

/// With a resolved policy the write tools change only what it allows, and
/// say no before anything is written otherwise.
#[tokio::test]
async fn a_confined_sessions_write_tools_stay_inside_the_policy() {
    use std::os::unix::fs::symlink;
    let t = Tree::new();
    std::fs::create_dir_all(t.ws.join("bin")).unwrap();
    let confined = || ctx(t.session(vec![t.ws.join("bin")], vec![]), &t.ws);

    // Inside the workspace: allowed.
    let inside = t.ws.join("src/e.txt");
    WriteTool.execute(json!({"path": s(&inside), "content": "one two"}), confined()).await.unwrap();
    EditTool.execute(edit_call(&inside, "one", "1"), confined()).await.unwrap();
    MultiEditTool.execute(multi_call(&[(&inside, "two", "2")]), confined()).await.unwrap();
    assert_eq!(std::fs::read_to_string(&inside).unwrap(), "1 2");

    // Outside the policy, or protected inside it: refused, nothing written.
    let victim = t.home.join("victim.txt");
    std::fs::write(&victim, "keep").unwrap();
    let in_plugins = t.ws.join(".synaps/plugins/p/run.sh");
    let on_path = t.ws.join("bin/tool");
    for (what, path) in [
        ("outside the workspace", t.home.join("new.txt")),
        ("the engine base dir", t.base.join("config")),
        ("a project plugin folder", in_plugins.clone()),
        ("a $PATH entry in the workspace", on_path.clone()),
    ] {
        let why = refusal(&WriteTool, json!({"path": s(&path), "content": "x"}), confined()).await;
        assert!(why.contains("sandbox: write refused"), "{what}: {why}");
        assert!(!path.exists(), "{what}: nothing was written");
    }
    assert!(!t.ws.join(".synaps").exists(), "no directory was made on the way to a protected path");

    refusal(&EditTool, edit_call(&victim, "keep", "gone"), confined()).await;
    refusal(&MultiEditTool, multi_call(&[(&victim, "keep", "gone")]), confined()).await;
    // One allowed edit and one refused: the whole call is refused.
    refusal(&MultiEditTool, multi_call(&[(&inside, "1", "uno"), (&victim, "keep", "gone")]), confined()).await;
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
    assert_eq!(std::fs::read_to_string(&inside).unwrap(), "1 2", "a refused call changes nothing");

    // A link inside the workspace to a file outside it is judged by its target.
    let link = t.ws.join("src/lnk");
    symlink(&victim, &link).unwrap();
    refusal(&EditTool, edit_call(&link, "keep", "gone"), confined()).await;
    refusal(&WriteTool, json!({"path": s(&link), "content": "gone"}), confined()).await;
    refusal(&MultiEditTool, multi_call(&[(&link, "keep", "gone")]), confined()).await;
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
}

/// The shell tool's child starts under Landlock: it can write the workspace
/// and not the owner's home, the engine's folders or a protected path inside
/// the workspace (needs a kernel with Landlock; skips loudly without).
#[tokio::test]
async fn a_confined_bash_command_writes_inside_and_not_outside() {
    if !kernel_ok("a_confined_bash_command_writes_inside_and_not_outside") {
        return;
    }
    let t = Tree::new();
    std::fs::create_dir_all(t.ws.join("bin")).unwrap();
    let confined = ctx(t.session(vec![t.ws.join("bin")], vec![]), &t.ws);

    let inside = t.ws.join("src/in.txt");
    let outside = t.home.join("out.txt");
    let in_base = t.base.join("config");
    let on_path = t.ws.join("bin/tool");
    let plugin = t.ws.join(".synaps/plugins/p");
    let script = format!(
        "echo ok > '{}'; echo no > '{}'; echo no > '{}'; echo no > '{}'; mkdir -p '{}'; ls '{}' > /dev/null",
        s(&inside),
        s(&outside),
        s(&in_base),
        s(&on_path),
        s(&plugin),
        s(&t.ws),
    );
    // The command's own exit status is not the point; the files are.
    let _ = BashTool.execute(json!({"command": script}), confined).await;
    assert_eq!(std::fs::read_to_string(&inside).unwrap_or_default().trim(), "ok", "the workspace is writable");
    for (what, path) in [
        ("the owner's home", &outside),
        ("the engine base dir", &in_base),
        ("a $PATH entry in the workspace", &on_path),
        ("a project plugin folder", &plugin),
    ] {
        assert!(!path.exists(), "{what}: a confined command wrote {}", path.display());
    }
}

// Every other place the engine starts a command for a session goes through
// the same sandbox decision. Each pin below uses a confined session whose
// ruleset cannot be built (`session_that_cannot_spawn`): the spawn must be
// refused before anything runs, on any Linux (no Landlock needed). Its undo
// is the same call with confinement off, which does run.

/// The shell tool and `ls`.
#[tokio::test]
async fn bash_and_ls_are_refused_when_the_sessions_ruleset_cannot_be_built() {
    let t = Tree::new();
    let marker = t.root.join("bash-ran");
    let command = json!({"command": format!("touch '{}'", s(&marker))});
    let session = t.session_that_cannot_spawn();

    let why = refusal(&BashTool, command.clone(), ctx(session.clone(), &t.root)).await;
    assert!(why.contains("sandbox"), "bash: {why}");
    assert!(!marker.exists(), "bash: the command ran anyway");
    let why = refusal(&LsTool, json!({"path": s(&t.root)}), ctx(session, &t.root)).await;
    assert!(why.contains("sandbox"), "ls: {why}");

    // Undo: with confinement off the same calls run.
    BashTool.execute(command, ctx(SessionSandbox::Off, &t.root)).await.unwrap();
    assert!(marker.exists(), "control: the command runs when confinement is off");
    LsTool.execute(json!({"path": s(&t.root)}), ctx(SessionSandbox::Off, &t.root)).await.unwrap();
}

/// A session's extension process.
#[tokio::test]
async fn an_extension_process_is_not_started_when_the_sessions_ruleset_cannot_be_built() {
    use crate::extensions::runtime::process::ProcessExtension;
    let t = Tree::new();
    let marker = t.root.join("extension-ran");
    let run = t.script("extension.sh", &format!("touch '{}'\n", s(&marker)));
    let args = vec![s(&run)];

    let session = t.session_that_cannot_spawn();
    let Err(why) = ProcessExtension::spawn_confined("example", "sh", &args, Some(t.root.clone()), session).await else {
        panic!("the extension process started in a session that cannot spawn");
    };
    assert!(why.contains("sandbox"), "{why}");
    assert!(!marker.exists(), "the extension ran anyway");

    // Undo: with confinement off it starts and runs.
    let started = ProcessExtension::spawn_confined("example", "sh", &args, Some(t.root.clone()), SessionSandbox::Off).await;
    assert!(started.is_ok(), "control: the extension starts when confinement is off");
    assert!(appears(&marker).await, "control: the extension ran");
}

fn spawn_pty(t: &Tree, script: &Path, sandbox: SpawnSandbox) -> crate::Result<super::shell::pty::PtyHandle> {
    let prepared = (std::env::vars_os().collect(), Some(t.root.clone()), None);
    super::shell::pty::PtyHandle::spawn_prepared(
        &format!("sh {}", script.display()),
        t.root.to_str(),
        24,
        80,
        prepared,
        sandbox,
    )
}

/// A shell session's PTY.
#[tokio::test]
async fn a_pty_shell_is_not_started_when_the_sessions_ruleset_cannot_be_built() {
    let t = Tree::new();
    let marker = t.root.join("pty-ran");
    let run = t.script("pty.sh", &format!("touch '{}'\n", s(&marker)));

    let session = t.session_that_cannot_spawn();
    assert!(spawn_pty(&t, &run, session.for_spawn()).is_err(), "the PTY started in a session that cannot spawn");
    assert!(!marker.exists(), "the shell ran anyway");

    // Undo: with confinement off it starts and runs.
    let handle = spawn_pty(&t, &run, SessionSandbox::Off.for_spawn());
    assert!(handle.is_ok(), "control: the PTY starts when confinement is off");
    assert!(appears(&marker).await, "control: the shell ran");
}

/// A shell session's PTY child really starts under Landlock (its spawn runs
/// on a confined thread, not through `pre_exec`). Needs a kernel with
/// Landlock; skips loudly without.
#[tokio::test]
async fn a_confined_pty_shell_writes_inside_and_not_outside() {
    if !kernel_ok("a_confined_pty_shell_writes_inside_and_not_outside") {
        return;
    }
    let t = Tree::new();
    let outside = t.home.join("pty-out.txt");
    let inside = t.ws.join("src/pty-in.txt");
    let run = t.script("pty.sh", &format!("echo no > '{}'\necho ok > '{}'\n", s(&outside), s(&inside)));

    let confined = spawn_pty(&t, &run, t.session(vec![], vec![]).for_spawn());
    assert!(confined.is_ok(), "the confined PTY must start");
    assert!(appears(&inside).await, "the workspace is writable");
    assert!(!outside.exists(), "a confined PTY shell wrote outside the workspace");
    drop(confined);

    // Undo: the same script with confinement off writes the outside file.
    let loose = spawn_pty(&t, &run, SessionSandbox::Off.for_spawn());
    assert!(loose.is_ok());
    assert!(appears(&outside).await, "control: the script writes there when confinement is off");
}
