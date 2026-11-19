//! Owner claim pins. Each "app" is a copy of `/bin/sh` (its own inode) held
//! at `read` on a pipe, so its `/proc/<pid>/exe` is that copy; the claimant
//! pid is handed in as the frame's writer would be (`frame_creds` pins the
//! per-frame tgid itself). The app path is handed to [`OwnerClaims::new`]
//! as the config value; only
//! `the_env_app_path_is_taken_at_start_and_no_tool_command_inherits_it`
//! sets [`APP_EXE_ENV`] (and takes it straight back out).

use super::*;
use crate::session::wire::{ClientFrame, OwnerKey};
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, ChildStdin, Command, Stdio};

type Verdict = (bool, Option<Reason>);
const BOUND: Verdict = (true, None);
const G: Duration = Duration::from_secs(15);

fn refused(reason: Reason) -> Verdict {
    (false, Some(reason))
}

fn verdict(frame: DaemonFrame) -> Verdict {
    match frame {
        DaemonFrame::OwnerClaim { owner_bound, reason } => (owner_bound, reason),
        other => panic!("not an owner_claim frame: {other:?}"),
    }
}

/// A copy of `/bin/sh` at `path` (a fresh inode, renamed into place).
fn install(path: &Path, mode: u32) {
    let tmp = path.with_extension("new");
    std::fs::copy("/bin/sh", &tmp).expect("copy /bin/sh");
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

struct Image {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn image() -> Image {
    image_mode(0o755)
}

fn image_mode(mode: u32) -> Image {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("example-app");
    install(&path, mode);
    Image { _dir: dir, path }
}

/// A running app process; killed and reaped on drop.
struct App {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl App {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// A line on the test app's stdin (its script's next `read`).
    fn say(&mut self, line: &str) {
        use std::io::Write;
        let stdin = self.stdin.as_mut().expect("stdin");
        stdin.write_all(format!("{line}\n").as_bytes()).unwrap();
        stdin.flush().unwrap();
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(path: &Path) -> App {
    run_with(path, &[])
}

/// `path -c 'read x'` with a clean loader/webview env plus `env`.
fn run_with(path: &Path, env: &[(&str, &str)]) -> App {
    run_script(path, "read x", env)
}

/// `path -c script` with a clean loader/webview env plus `env`.
fn run_script(path: &Path, script: &str, env: &[(&str, &str)]) -> App {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut cmd = Command::new(path);
        cmd.args(["-c", script]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
        for name in REFUSED_ENV {
            cmd.env_remove(name);
        }
        cmd.env_remove(APP_EXE_ENV);
        for (k, v) in env {
            cmd.env(k, v);
        }
        match cmd.spawn() {
            Ok(mut child) => {
                let stdin = child.stdin.take();
                return App { child, stdin };
            }
            // A parallel test's fork may hold the copy's write fd until its exec.
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && attempt < 100 => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("spawn {}: {e}", path.display()),
        }
    }
}

fn claims(settle: Duration, path: &Path) -> Arc<OwnerClaims> {
    OwnerClaims::new(settle, None, Some(path.display().to_string()))
}

/// One connection: its link and frame queue.
struct Conn {
    guard: LinkGuard,
    rx: mpsc::Receiver<DaemonFrame>,
    _tx: mpsc::Sender<DaemonFrame>,
}

impl Conn {
    fn new(claims: &Arc<OwnerClaims>) -> Self {
        let (tx, rx) = mpsc::channel(16);
        let guard = claims.link(&tx);
        Self { guard, rx, _tx: tx }
    }

    fn id(&self) -> LinkId {
        self.guard.id
    }

    /// The claim's reply (queued on the link by `claim`); the link's socket
    /// offset when it passes the image check is [`MARK`].
    fn claim(&mut self, claims: &OwnerClaims, digest: [u8; 32], pid: Option<u32>) -> Verdict {
        self.claim_at(claims, digest, pid, || Some(MARK))
    }

    fn claim_at(&mut self, claims: &OwnerClaims, digest: [u8; 32], pid: Option<u32>, horizon: impl FnOnce() -> Option<u64>) -> Verdict {
        match claims.claim(self.id(), digest, pid, horizon).expect("a claim on a fresh link is answered") {
            Delivery::Queued => {}
            other => panic!("the reply was not queued: {other:?}"),
        }
        let reply = verdict(self.rx.try_recv().expect("the reply is queued"));
        self.quiet();
        reply
    }

    /// The next pushed frame, if any.
    fn pushed(&mut self) -> Option<Verdict> {
        self.rx.try_recv().ok().map(verdict)
    }

    fn quiet(&mut self) {
        if let Ok(frame) = self.rx.try_recv() {
            panic!("an unexpected frame: {frame:?}");
        }
    }

    fn attached_bound(&self, claims: &OwnerClaims) -> Option<bool> {
        let mut seen = None;
        let delivery = claims.send_bound(self.id(), |bound| {
            seen = Some(bound);
            DaemonFrame::OwnerClaim { owner_bound: false, reason: None }
        });
        assert!(matches!(delivery, Delivery::Queued), "{delivery:?}");
        seen.expect("built")
    }
}

const K1: [u8; 32] = [0x11; 32];
const K2: [u8; 32] = [0x22; 32];
/// A link's socket offset when its claim passes the check; a frame that
/// starts there is past the mark.
const MARK: u64 = 4096;

async fn pass(d: Duration) {
    tokio::time::advance(d).await;
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

/// Real time: wait for the lapse watcher.
async fn until_state(claims: &OwnerClaims, want: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while claims.state_name() != want {
        assert!(tokio::time::Instant::now() < deadline, "state {} never became {want}", claims.state_name());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The env `SYNAPS_DAEMON_OWNER_APP_EXE` wins over `daemon.owner_app_exe`
/// whenever it is set and non-empty; the config value is then ignored.
#[test]
fn owner_app_path_env_wins_and_the_config_is_then_ignored() {
    let p = |s: &str| Some(PathBuf::from(s));
    assert_eq!(app_exe(Some("/env/example-app"), Some("/config/example-app")), p("/env/example-app"));
    assert_eq!(app_exe(Some(" /env/example-app "), Some("/config/example-app")), p("/env/example-app"));
    assert_eq!(app_exe(Some("/env/example-app"), None), p("/env/example-app"));
    assert_eq!(app_exe(None, Some("/config/example-app")), p("/config/example-app"));
    assert_eq!(app_exe(Some(""), Some("/config/example-app")), p("/config/example-app"));
    assert_eq!(app_exe(Some("   "), Some("/config/example-app")), p("/config/example-app"));
    assert_eq!(app_exe(None, None), None);
    assert_eq!(app_exe(Some(""), Some(" ")), None);
    let c = OwnerClaims::new(G, Some("/env/example-app".into()), Some("/config/example-app".into()));
    assert_eq!(c.app_path(), p("/env/example-app"), "the config value is ignored");
    let c = OwnerClaims::new(G, None, Some("/config/example-app".into()));
    assert_eq!(c.app_path(), p("/config/example-app"));
}

/// The env app path is read once, at the daemon's start, and taken out of
/// the daemon's environment, so a tool command the daemon runs after that
/// (inheriting its env, as with no client env) lacks it.
#[tokio::test]
async fn the_env_app_path_is_taken_at_start_and_no_tool_command_inherits_it() {
    use crate::tools::Tool;
    const APP: &str = "/opt/example-app/current/example-app";
    std::env::set_var(APP_EXE_ENV, APP);
    let taken = take_app_exe_env();
    let ctx = crate::tools::test_helpers::create_tool_context();
    assert!(ctx.capabilities.env.is_none(), "the command inherits the daemon's env");
    let command = format!("echo \"app=${{{APP_EXE_ENV}:-unset}}\"");
    let out = crate::tools::BashTool.execute(serde_json::json!({ "command": command, "timeout": 30 }), ctx).await.unwrap();
    assert!(out.contains("app=unset"), "a tool command inherited the owner app path: {out}");
    assert_eq!(taken.as_deref(), Some(APP));
    assert!(std::env::var_os(APP_EXE_ENV).is_none());
    assert_eq!(take_app_exe_env(), None, "read once");
    let c = OwnerClaims::new(G, taken, None);
    assert_eq!(c.app_path(), Some(PathBuf::from(APP)));
    assert_eq!(c.env_exe(), Some(APP), "kept for a reload to hand on");
}

/// The env app path, gone from the environment, reaches the next image of a
/// reload explicitly, in both exec paths (source pin: a real reload execs).
#[test]
fn a_reload_hands_the_env_app_path_to_the_next_image() {
    let src = include_str!("reload.rs");
    for head in ["fn exec_self(", "async fn exec_successor("] {
        let body = &src[src.find(head).expect(head)..];
        let body = &body[..body.find("\n}\n").expect("the function's end")];
        assert!(body.contains("state.owner.env_exe()") && body.contains("owner_claim::APP_EXE_ENV, app)"), "{head}");
    }
}

/// The daemon takes the env app path out of its environment before the host
/// boot starts any process (extensions, MCP servers), so none inherits it
/// (source pin: `run_foreground` is the real daemon's start).
#[test]
fn the_daemon_takes_the_env_app_path_before_it_starts_any_process() {
    let src = include_str!("mod.rs");
    let body = &src[src.find("pub async fn run_foreground(").expect("run_foreground")..];
    let body = &body[..body.find("\n}\n").expect("the function's end")];
    let taken = body.find("opts.owner_app_exe_env = owner_claim::take_app_exe_env();").expect("run_foreground takes the env app path");
    let boot = body.find("EngineHost::boot_and_install(").expect("the host boot");
    assert!(taken < boot, "taken before the host boot");
}

/// `main` moves the env app path out of the environment before it builds
/// its runtime (source pin in tests/daemon_owner_claim.rs), so a client
/// that starts a daemon hands it on explicitly, on both platforms' spawn
/// (source pin: no unit test may stash, the stash is process-wide).
#[test]
fn a_client_that_starts_a_daemon_hands_the_stashed_app_path_on() {
    let src = include_str!("mod.rs");
    let heads: Vec<usize> = src.match_indices("pub fn spawn_detached(").map(|(at, _)| at).collect();
    assert_eq!(heads.len(), 2, "unix and windows");
    for at in heads {
        let body = &src[at..];
        let body = &body[..body.find("\n}\n").expect("the function's end")];
        assert!(
            body.contains("owner_claim::stashed_app_exe_env()") && body.contains("owner_claim::APP_EXE_ENV, app)"),
            "spawn_detached at byte {at} does not hand the stashed app path on"
        );
    }
}

/// No path set: every claim is `unsupported`; nothing registers.
#[tokio::test]
async fn no_owner_app_path_answers_unsupported() {
    let c = OwnerClaims::new(Duration::ZERO, None, None);
    let img = image();
    let app = run(&img.path);
    let mut a = Conn::new(&c);
    let mut quiet = Conn::new(&c);
    assert_eq!(a.claim(&c, K1, Some(app.pid())), refused(Reason::Unsupported));
    assert_eq!(c.state_name(), "vacant");
    assert!(!c.is_owner(a.id(), Some(app.pid()), MARK));
    assert_eq!(a.attached_bound(&c), Some(false));
    assert_eq!(quiet.attached_bound(&c), None);
    let _ = a.rx.try_recv();
    let _ = quiet.rx.try_recv();
    quiet.quiet();
}

/// A claim 90 s after the daemon starts registers and settles for G from
/// that registration; Settling marks nothing; then Bound is pushed to the
/// proved link only, and only frames from the registrant's tgid count.
#[tokio::test(start_paused = true)]
async fn a_claim_long_after_start_settles_for_g_then_binds() {
    let img = image();
    let app = run(&img.path);
    let c = claims(G, &img.path);
    let mut quiet = Conn::new(&c);
    pass(Duration::from_secs(90)).await;
    let mut a = Conn::new(&c);
    assert_eq!(a.claim(&c, K1, Some(app.pid())), refused(Reason::Settling));
    assert_eq!(c.state_name(), "settling");
    assert!(!c.is_owner(a.id(), Some(app.pid()), MARK), "Settling marks nothing");
    assert_eq!(a.attached_bound(&c), Some(false));
    let _ = a.rx.try_recv();
    pass(G - Duration::from_secs(1)).await;
    assert_eq!(c.state_name(), "settling");
    assert_eq!(a.pushed(), None);
    pass(Duration::from_secs(1)).await;
    assert_eq!(c.state_name(), "bound");
    assert_eq!(a.pushed(), Some(BOUND));
    assert!(c.is_owner(a.id(), Some(app.pid()), MARK));
    assert!(!c.is_owner(a.id(), None, MARK), "a frame of mixed or unknown writers is not the owner's");
    assert!(!c.is_owner(a.id(), Some(std::process::id()), MARK), "another tgid on the bound link is not the owner");
    assert!(!c.is_owner(quiet.id(), Some(app.pid()), MARK), "a link that never proved the key is not the owner's");
    assert_eq!(a.attached_bound(&c), Some(true));
    assert_eq!(quiet.attached_bound(&c), None, "compat: owner_bound only on a claimed link");
    let _ = a.rx.try_recv();
    let _ = quiet.rx.try_recv();
    quiet.quiet();
}

/// A second key while Settling: contested, nobody binds when G passes.
#[tokio::test(start_paused = true)]
async fn a_second_key_while_settling_is_contested_and_nothing_binds() {
    let img = image();
    let app = run(&img.path);
    let c = claims(G, &img.path);
    let mut a = Conn::new(&c);
    let mut b = Conn::new(&c);
    assert_eq!(a.claim(&c, K1, Some(app.pid())), refused(Reason::Settling));
    assert_eq!(b.claim(&c, K2, Some(app.pid())), refused(Reason::Contested));
    assert_eq!(a.pushed(), Some(refused(Reason::Contested)));
    pass(G * 2).await;
    assert_eq!(c.state_name(), "contested");
    assert_eq!(a.pushed(), None);
    assert!(!c.is_owner(a.id(), Some(app.pid()), MARK));
    assert_eq!(a.claim(&c, K1, Some(app.pid())), refused(Reason::Contested), "after the push it may claim again: still contested");
}

/// A second key while Bound: contested; every claimed link is told; a link
/// that never claimed hears nothing.
#[tokio::test]
async fn a_second_key_while_bound_is_contested_and_only_claimed_links_hear_it() {
    let img = image();
    let app = run(&img.path);
    let c = claims(Duration::ZERO, &img.path);
    let mut a = Conn::new(&c);
    let mut b = Conn::new(&c);
    let mut quiet = Conn::new(&c);
    assert_eq!(a.claim(&c, K1, Some(app.pid())), BOUND);
    assert!(c.is_owner(a.id(), Some(app.pid()), MARK));
    assert_eq!(b.claim(&c, K2, Some(app.pid())), refused(Reason::Contested));
    assert_eq!(a.pushed(), Some(refused(Reason::Contested)));
    assert_eq!(c.state_name(), "contested");
    assert!(!c.is_owner(a.id(), Some(app.pid()), MARK));
    quiet.quiet();
    assert_eq!(quiet.attached_bound(&c), None);
    let _ = quiet.rx.try_recv();
    quiet.quiet();
}

/// The right key from another process is `not_the_registrant`, told only
/// to that link; the bound app stays bound.
#[tokio::test]
async fn the_key_from_another_process_is_not_the_registrant_and_the_owner_stays_bound() {
    let img = image();
    let app = run(&img.path);
    let other = run(&img.path);
    let c = claims(Duration::ZERO, &img.path);
    let mut a = Conn::new(&c);
    let mut b = Conn::new(&c);
    assert_eq!(a.claim(&c, K1, Some(app.pid())), BOUND);
    assert_eq!(b.claim(&c, K1, Some(other.pid())), refused(Reason::NotTheRegistrant));
    assert_eq!(a.pushed(), None, "the bound link is not told");
    assert_eq!(c.state_name(), "bound");
    assert!(c.is_owner(a.id(), Some(app.pid()), MARK), "the bound app stays bound");
    assert!(!c.is_owner(b.id(), Some(other.pid()), MARK));
    assert_eq!(b.attached_bound(&c), Some(false));
    assert_eq!(a.attached_bound(&c), Some(true));
}

/// A process running another image is `not_the_app` (before any state is
/// read: it can't contest or learn the state).
#[tokio::test]
async fn another_image_is_not_the_app_and_cannot_contest() {
    let img = image();
    let stranger_img = image();
    let app = run(&img.path);
    let stranger = run(&stranger_img.path);
    let c = claims(Duration::ZERO, &img.path);
    let mut s = Conn::new(&c);
    assert_eq!(s.claim(&c, K2, Some(stranger.pid())), refused(Reason::NotTheApp));
    assert_eq!(c.state_name(), "vacant");
    let mut a = Conn::new(&c);
    assert_eq!(a.claim(&c, K1, Some(app.pid())), BOUND);
    let mut s2 = Conn::new(&c);
    assert_eq!(s2.claim(&c, K2, Some(stranger.pid())), refused(Reason::NotTheApp));
    assert_eq!(c.state_name(), "bound", "a stranger's key never contests");
    assert_eq!(a.pushed(), None);
    let mut t = Conn::new(&c);
    assert_eq!(t.claim(&c, K2, Some(std::process::id())), refused(Reason::NotTheApp));
    let mut u = Conn::new(&c);
    assert_eq!(u.claim(&c, K2, None), refused(Reason::NotTheApp), "a frame of unknown writers");
}

/// The path is resolved at each claim: once it names another file, a
/// process still running the old file is `not_the_app`.
#[tokio::test]
async fn the_path_resolves_at_each_claim_so_a_replaced_file_is_not_the_app() {
    let img = image();
    let old = run(&img.path);
    let c = claims(Duration::ZERO, &img.path);
    install(&img.path, 0o755);
    let mut a = Conn::new(&c);
    assert_eq!(a.claim(&c, K1, Some(old.pid())), refused(Reason::NotTheApp));
    let new = run(&img.path);
    let mut b = Conn::new(&c);
    assert_eq!(b.claim(&c, K1, Some(new.pid())), BOUND);
}

/// An install repoints `current` while the daemon is up: the old app's
/// claim lapses at its exit, then the new image's claim registers and binds
/// (no contested), with no daemon restart.
#[tokio::test]
async fn an_install_that_repoints_current_hands_over_at_the_old_apps_exit() {
    let root = tempfile::tempdir().unwrap();
    for release in ["a", "b"] {
        std::fs::create_dir_all(root.path().join(release)).unwrap();
        install(&root.path().join(release).join("example-app"), 0o755);
    }
    let current = root.path().join("current");
    std::os::unix::fs::symlink(root.path().join("a"), &current).unwrap();
    let stable = current.join("example-app");
    let c = claims(Duration::ZERO, &stable);
    let mut old = run(&stable);
    let mut a = Conn::new(&c);
    assert_eq!(a.claim(&c, K1, Some(old.pid())), BOUND);

    let next = root.path().join("current.next");
    std::os::unix::fs::symlink(root.path().join("b"), &next).unwrap();
    std::fs::rename(&next, &current).unwrap();
    assert!(c.is_owner(a.id(), Some(old.pid()), MARK), "the running app's bound link holds until it exits");
    let mut a2 = Conn::new(&c);
    assert_eq!(a2.claim(&c, K1, Some(old.pid())), refused(Reason::NotTheApp), "the old image no longer matches");

    old.stop();
    until_state(&c, "vacant").await;
    assert_eq!(a.pushed(), None, "a lapse pushes nothing");
    assert!(!c.is_owner(a.id(), Some(old.pid()), MARK));

    let new = run(&stable);
    let mut b = Conn::new(&c);
    assert_eq!(b.claim(&c, K2, Some(new.pid())), BOUND, "a new key registers: no contested");
    assert!(c.is_owner(b.id(), Some(new.pid()), MARK));
    assert_eq!(a.pushed(), None);
}

/// The registration holds while the registrant lives, links or none
/// (another key is then contested); its exit lapses it to Vacant at once.
#[tokio::test]
async fn a_live_registrant_holds_with_no_links_and_its_exit_lapses_it() {
    let img = image();
    let mut app = run(&img.path);
    let c = claims(Duration::ZERO, &img.path);
    {
        let mut a = Conn::new(&c);
        assert_eq!(a.claim(&c, K1, Some(app.pid())), BOUND);
    }
    assert_eq!(c.state_name(), "bound", "no links left; the registrant lives");
    let mut b = Conn::new(&c);
    assert_eq!(b.claim(&c, K2, Some(app.pid())), refused(Reason::Contested));

    let c = claims(Duration::ZERO, &img.path);
    let mut a = Conn::new(&c);
    assert_eq!(a.claim(&c, K1, Some(app.pid())), BOUND);
    app.stop();
    until_state(&c, "vacant").await;
    assert!(!c.is_owner(a.id(), Some(app.pid()), MARK));
    let next = run(&img.path);
    assert_eq!(a.claim(&c, K2, Some(next.pid())), BOUND, "after a lapse the link may claim again");
}

/// A claimant whose exe can't be read or stat'd (a non-dumpable process:
/// here one exec'd from a file it can't read) fails closed.
#[tokio::test]
async fn a_claimant_whose_exe_cannot_be_read_fails_closed() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root reads every /proc/<pid>/exe");
        return;
    }
    let img = image_mode(0o111);
    let app = run(&img.path);
    assert!(std::fs::metadata(format!("/proc/{}/exe", app.pid())).is_err(), "the child is not dumpable");
    assert_eq!(check_image(app.pid(), &img.path).err(), Some("the claimant's exe can't be read (fail closed)"));
    let c = claims(Duration::ZERO, &img.path);
    let mut a = Conn::new(&c);
    assert_eq!(a.claim(&c, K1, Some(app.pid())), refused(Reason::NotTheApp));
    assert_eq!(c.state_name(), "vacant");
}

/// A claimant started with a loader or webview remote-control env var is
/// `not_the_app`; an empty one, or an unrelated toolkit module var, is not
/// refused.
#[tokio::test]
async fn a_claimant_started_with_a_loader_or_webview_control_env_is_not_the_app() {
    let img = image();
    for name in REFUSED_ENV {
        let app = run_with(&img.path, &[(name, "/nonexistent/example")]);
        let c = claims(Duration::ZERO, &img.path);
        let mut a = Conn::new(&c);
        assert_eq!(a.claim(&c, K1, Some(app.pid())), refused(Reason::NotTheApp), "{name}");
        assert_eq!(c.state_name(), "vacant", "{name}");
    }
    let app = run_with(&img.path, &[("LD_PRELOAD", ""), ("GTK_MODULES", "canberra-gtk-module")]);
    let c = claims(Duration::ZERO, &img.path);
    let mut a = Conn::new(&c);
    assert_eq!(a.claim(&c, K1, Some(app.pid())), BOUND);

    assert!(environ_refused(b"HOME=/h\0LD_PRELOAD=/x.so\0"));
    assert!(environ_refused(b"TAURI_WEBVIEW_AUTOMATION=true"));
    assert!(!environ_refused(b"LD_PRELOAD=\0HOME=/h\0"));
    assert!(!environ_refused(b"LD_PRELOADX=1\0XLD_PRELOAD=1\0GTK_MODULES=x\0"));
}

/// Contested survives a reload; only a reset clears it; the reset is
/// denial only (nothing binds by itself) and is pushed to claimed links
/// only; a claim after it still needs the image check and G.
#[tokio::test(start_paused = true)]
async fn contested_survives_a_reload_and_a_reset_only_denies() {
    let img = image();
    let app = run(&img.path);
    let c = claims(G, &img.path);
    let mut a = Conn::new(&c);
    let mut b = Conn::new(&c);
    assert_eq!(a.claim(&c, K1, Some(app.pid())), refused(Reason::Settling));
    assert_eq!(b.claim(&c, K2, Some(app.pid())), refused(Reason::Contested));
    let carry = c.carry().expect("contested is carried");
    assert_eq!(carry, OwnerCarry { state: CarriedState::Contested, registrant: None });

    let c = claims(G, &img.path);
    c.restore(Some(&carry));
    assert_eq!(c.state_name(), "contested");
    let mut a = Conn::new(&c);
    let mut quiet = Conn::new(&c);
    let cli = Conn::new(&c);
    assert_eq!(a.claim(&c, K1, Some(app.pid())), refused(Reason::Contested));

    assert_eq!(verdict(c.reset(cli.id())), refused(Reason::Reset));
    assert_eq!(c.state_name(), "vacant");
    assert_eq!(a.pushed(), Some(refused(Reason::Reset)));
    quiet.quiet();
    pass(G * 4).await;
    assert_eq!(c.state_name(), "vacant", "a reset never binds by itself");
    assert!(!c.is_owner(a.id(), Some(app.pid()), MARK));

    let mut t = Conn::new(&c);
    assert_eq!(t.claim(&c, K1, Some(std::process::id())), refused(Reason::NotTheApp), "the image check still applies");
    assert_eq!(a.claim(&c, K1, Some(app.pid())), refused(Reason::Settling), "and the settle window");
    pass(G).await;
    assert_eq!(a.pushed(), Some(BOUND));
}

/// A reload carries the digest (never the key); the new image reopens the
/// live registrant (links re-prove the key, no new registration); a
/// registrant gone by then leaves it Vacant; a carried Settling restarts G.
#[tokio::test(start_paused = true)]
async fn a_reload_carries_only_the_digest_and_reopens_the_registrant() {
    let img = image();
    let mut app = run(&img.path);
    let mut key = OwnerKey::from_bytes([0x5a; 32]);
    let key_hex = key.to_hex().to_string();
    let digest = key.digest_and_wipe();
    let c = claims(Duration::ZERO, &img.path);
    let mut a = Conn::new(&c);
    assert_eq!(a.claim(&c, digest, Some(app.pid())), BOUND);
    let carry = c.carry().expect("bound is carried");
    let text = serde_json::to_string(&carry).unwrap();
    assert!(!text.contains(&key_hex), "the key is never carried");
    assert!(text.contains(&hex(&digest)), "{text}");

    let c2 = claims(Duration::ZERO, &img.path);
    c2.restore(Some(&carry));
    assert_eq!(c2.state_name(), "bound");
    let mut a2 = Conn::new(&c2);
    assert!(!c2.is_owner(a2.id(), Some(app.pid()), MARK), "a link re-proves the key on the new image");
    assert_eq!(a2.claim(&c2, digest, Some(app.pid())), BOUND);
    assert!(c2.is_owner(a2.id(), Some(app.pid()), MARK));

    let settling = OwnerCarry { state: CarriedState::Settling, ..carry.clone() };
    let c3 = claims(G, &img.path);
    c3.restore(Some(&settling));
    assert_eq!(c3.state_name(), "settling");
    pass(G - Duration::from_secs(1)).await;
    assert_eq!(c3.state_name(), "settling", "a carried Settling restarts a full G");
    pass(Duration::from_secs(1)).await;
    assert_eq!(c3.state_name(), "bound");

    app.stop();
    let c4 = claims(Duration::ZERO, &img.path);
    c4.restore(Some(&carry));
    assert_eq!(c4.state_name(), "vacant", "the registrant is gone");
}

/// `Attached.owner_bound` is absent unless the link claimed, then whether
/// it is bound now.
#[tokio::test]
async fn attached_owner_bound_follows_the_links_claim() {
    let img = image();
    let app = run(&img.path);
    let stranger_img = image();
    let stranger = run(&stranger_img.path);
    let c = claims(Duration::ZERO, &img.path);
    let quiet = Conn::new(&c);
    let mut a = Conn::new(&c);
    let mut s = Conn::new(&c);
    assert_eq!(quiet.attached_bound(&c), None);
    assert_eq!(a.attached_bound(&c), None);
    let _ = a.rx.try_recv();
    assert_eq!(a.claim(&c, K1, Some(app.pid())), BOUND);
    assert_eq!(a.attached_bound(&c), Some(true));
    assert_eq!(s.claim(&c, K1, Some(stranger.pid())), refused(Reason::NotTheApp));
    assert_eq!(s.attached_bound(&c), Some(false));
}

/// A second claim on a link whose claim is live is refused (the caller
/// sends an `Error`); a pushed unbind frees it.
#[tokio::test]
async fn a_link_claims_once_until_a_push_unbinds_it() {
    let img = image();
    let app = run(&img.path);
    let c = claims(Duration::ZERO, &img.path);
    let mut a = Conn::new(&c);
    assert_eq!(a.claim(&c, K1, Some(app.pid())), BOUND);
    assert_eq!(c.claim(a.id(), K1, Some(app.pid()), || Some(MARK)).err(), Some(DUPLICATE_CLAIM));
    let mut s = Conn::new(&c);
    assert_eq!(s.claim(&c, K1, None), refused(Reason::NotTheApp));
    assert_eq!(c.claim(s.id(), K1, Some(app.pid()), || Some(MARK)).err(), Some(DUPLICATE_CLAIM), "a refusal is an answer too");
    let cli = Conn::new(&c);
    let _ = c.reset(cli.id());
    assert_eq!(a.pushed(), Some(refused(Reason::Reset)));
    let mut d = Conn::new(&c);
    assert_eq!(d.claim(&c, K1, Some(app.pid())), BOUND);
    assert!(c.is_owner(d.id(), Some(app.pid()), MARK));
    assert!(!c.is_owner(a.id(), Some(app.pid()), MARK), "a link the reset unbound counts only once it proves the key again");
    assert_eq!(a.claim(&c, K1, Some(app.pid())), BOUND);
    assert!(c.is_owner(a.id(), Some(app.pid()), MARK));
}

/// A frame counts from its link's mark on (the link's socket offset once
/// the claim's image check passed). One that starts before the mark was
/// written before the check, perhaps by the image the claimant ran before
/// an `execve` into the app (`read x; exec example-app` with a marked
/// Submit queued behind the claim), so it is nobody's. The mark is read
/// only once the check passed; a link whose queued bytes can't be measured
/// fails closed.
#[tokio::test]
async fn a_frame_that_starts_before_the_links_mark_is_not_the_owners() {
    let img = image();
    let app = run(&img.path);
    let stranger_img = image();
    let stranger = run(&stranger_img.path);
    let c = claims(Duration::ZERO, &img.path);
    let mut s = Conn::new(&c);
    let unread = || -> Option<u64> { panic!("the mark is read only once the check passed") };
    assert_eq!(s.claim_at(&c, K2, Some(stranger.pid()), unread), refused(Reason::NotTheApp));
    let mut u = Conn::new(&c);
    assert_eq!(u.claim_at(&c, K1, Some(app.pid()), || None), refused(Reason::NotTheApp), "no horizon: fail closed");
    assert_eq!(c.state_name(), "vacant", "nothing registered");
    let mut a = Conn::new(&c);
    assert_eq!(a.claim(&c, K1, Some(app.pid())), BOUND);
    assert!(!c.is_owner(a.id(), Some(app.pid()), 0), "written before the check");
    assert!(!c.is_owner(a.id(), Some(app.pid()), MARK - 1), "written before the check");
    assert!(c.is_owner(a.id(), Some(app.pid()), MARK));
    assert!(c.is_owner(a.id(), Some(app.pid()), MARK + 1));
}

/// Each marked frame stats the registrant's exe again: once it `execve`s
/// another image (same pid, same start time, its pidfd still live, so no
/// lapse) none of its frames is the owner's, past the mark or not.
#[tokio::test]
async fn a_registrant_that_execs_another_image_is_no_longer_the_owner() {
    let img = image();
    let mut app = run_script(&img.path, "read x; exec sleep 30", &[]);
    let c = claims(Duration::ZERO, &img.path);
    let mut a = Conn::new(&c);
    assert_eq!(a.claim(&c, K1, Some(app.pid())), BOUND);
    assert!(c.is_owner(a.id(), Some(app.pid()), MARK + 1));
    let exe = |pid: u32| std::fs::read_link(format!("/proc/{pid}/exe")).ok();
    let before = exe(app.pid());
    app.say("go");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while exe(app.pid()) == before {
        assert!(std::time::Instant::now() < deadline, "the app never exec'd");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(c.state_name(), "bound", "the same instance: no lapse");
    assert!(!c.is_owner(a.id(), Some(app.pid()), MARK + 1), "it runs another image now");
}

/// A reload's restore runs the image check again, against the app path as
/// the new image reads it (dev/ino, start environ), as the same instance;
/// else Vacant: the path names another file now, the pid is another
/// instance, or the carried pid runs another image.
#[tokio::test]
async fn a_reload_rechecks_the_carried_registrant_else_vacant() {
    let img = image();
    let app = run(&img.path);
    let c = claims(Duration::ZERO, &img.path);
    let mut a = Conn::new(&c);
    assert_eq!(a.claim(&c, K1, Some(app.pid())), BOUND);
    let carry = c.carry().expect("bound is carried");
    let r = carry.registrant.clone().expect("a registrant");

    let other = image();
    let c2 = claims(Duration::ZERO, &other.path);
    c2.restore(Some(&carry));
    assert_eq!(c2.state_name(), "vacant", "the app path names another file now");

    let reused = OwnerCarry {
        state: CarriedState::Bound,
        registrant: Some(CarriedRegistrant { start_ticks: r.start_ticks + 1, ..r.clone() }),
    };
    let c3 = claims(Duration::ZERO, &img.path);
    c3.restore(Some(&reused));
    assert_eq!(c3.state_name(), "vacant", "the pid is another instance");

    let stranger_img = image();
    let stranger = run(&stranger_img.path);
    let (start, _) = stat_of(stranger.pid()).expect("the stranger's start time");
    let forged = OwnerCarry {
        state: CarriedState::Bound,
        registrant: Some(CarriedRegistrant { pid: stranger.pid(), start_ticks: start, ..r.clone() }),
    };
    let c4 = claims(Duration::ZERO, &img.path);
    c4.restore(Some(&forged));
    assert_eq!(c4.state_name(), "vacant", "the carried pid runs another image");

    let c5 = claims(Duration::ZERO, &img.path);
    c5.restore(Some(&carry));
    assert_eq!(c5.state_name(), "bound", "still the app: carried");
}

/// A `/proc/<pid>/stat` line splits after the last `)` (the command name
/// may hold spaces and parentheses); field 22 is the start time and a
/// zombie counts as exited.
#[test]
fn a_stat_line_names_its_start_time_past_a_tricky_command_name() {
    let tail = "0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 4242 0 0";
    assert_eq!(parse_stat(&format!("7 (a) b (c) S 1 7 7 {tail}")), Some((4242, false)));
    assert_eq!(parse_stat(&format!("7 (x y) Z 1 7 7 {tail}")), Some((4242, true)));
    assert_eq!(parse_stat("7 (a) S 1 7"), None, "too short");
    assert_eq!(parse_stat("no parenthesis"), None);
    let (start, exited) = stat_of(std::process::id()).expect("this process");
    assert!(start > 0 && !exited);
    assert!(pidfd(std::process::id(), start).is_some(), "this instance");
    assert!(pidfd(std::process::id(), start + 1).is_none(), "another instance of the pid");
}

/// A state-changing push is never lost to a full queue: it is held and
/// sent as soon as there is room, in order; a later push while one is held
/// replaces it (each states the link's whole claim), so the link always
/// ends on the current state; with room, a push is queued at once.
#[tokio::test]
async fn a_push_that_finds_the_queue_full_goes_out_once_there_is_room() {
    let img = image();
    let app = run(&img.path);
    let c = claims(Duration::ZERO, &img.path);
    let (tx, mut rx) = mpsc::channel(1);
    let link = c.link(&tx);
    let filler = || DaemonFrame::OwnerClaim { owner_bound: false, reason: None };
    async fn next(rx: &mut mpsc::Receiver<DaemonFrame>) -> Verdict {
        verdict(tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("sent once there is room").unwrap())
    }
    tx.try_send(filler()).unwrap();
    assert!(matches!(c.claim(link.id, K1, Some(app.pid()), || Some(MARK)), Ok(Delivery::Queued)), "held");
    assert_eq!(next(&mut rx).await, (false, None), "the frame queued before it");
    assert_eq!(next(&mut rx).await, BOUND, "the held reply");

    tx.try_send(filler()).unwrap();
    let mut b = Conn::new(&c);
    assert_eq!(b.claim(&c, K2, Some(app.pid())), refused(Reason::Contested));
    let _ = c.reset(b.id());
    assert_eq!(next(&mut rx).await, (false, None));
    assert_eq!(next(&mut rx).await, refused(Reason::Reset), "the latest state replaced the held one");
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(rx.try_recv().is_err(), "nothing else");

    let mut d = Conn::new(&c);
    assert_eq!(d.claim(&c, K1, Some(app.pid())), BOUND);
    let _ = c.reset(d.id());
    assert_eq!(rx.try_recv().map(verdict).ok(), Some(refused(Reason::Reset)), "room: queued at once");
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// One log line per claim names the pid, exe and verdict; no log line ever
/// holds the key or its digest.
#[tokio::test]
async fn a_claim_logs_pid_exe_and_verdict_never_the_key_or_digest() {
    use tracing_subscriber::filter::{EnvFilter, LevelFilter};
    let img = image();
    let app = run(&img.path);
    let stranger_img = image();
    let stranger = run(&stranger_img.path);
    let mut key = OwnerKey::from_bytes([0xc3; 32]);
    let key_hex = key.to_hex().to_string();
    let digest = key.digest_and_wipe();
    let capture = Capture::default();
    // The daemon log file's own filter (`with_crate_directives` over the
    // ERROR default an unset `RUST_LOG` gives), so this is what reaches the
    // daemon log.
    let unset = EnvFilter::builder().with_default_directive(LevelFilter::ERROR.into()).parse_lossy("");
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(crate::logging::with_crate_directives(unset))
        .with_ansi(false)
        .with_writer(capture.clone())
        .finish();
    // A second live dispatcher keeps tracing-core off its `has_just_one`
    // shortcut, under which a parallel test that reached the shared
    // "owner claim" callsite first (with no capture) cached `never` for it.
    let _second = tracing::Dispatch::new(tracing_subscriber::registry());
    tracing::callsite::rebuild_interest_cache();
    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let c = claims(Duration::ZERO, &img.path);
        let mut a = Conn::new(&c);
        let mut s = Conn::new(&c);
        assert_eq!(a.claim(&c, digest, Some(app.pid())), BOUND);
        assert_eq!(s.claim(&c, [0x3c; 32], Some(stranger.pid())), refused(Reason::NotTheApp));
        let mut t = Conn::new(&c);
        assert_eq!(t.claim(&c, K2, Some(app.pid())), refused(Reason::Contested));
        let _ = c.carry();
        let _ = c.reset(t.id());
    }
    let log = String::from_utf8_lossy(&capture.0.lock().unwrap()).into_owned();
    let claims_logged: Vec<&str> = log.lines().filter(|l| l.contains("owner claim") && l.contains("verdict")).collect();
    assert_eq!(claims_logged.len(), 3, "{log}");
    assert!(claims_logged[0].contains(&format!("pid={}", app.pid())) && claims_logged[0].contains("verdict=\"bound\""), "{log}");
    let exe = std::fs::canonicalize(&img.path).unwrap();
    assert!(claims_logged[0].contains(&exe.display().to_string()), "{log}");
    assert!(claims_logged[1].contains(&format!("pid={}", stranger.pid())) && claims_logged[1].contains("not_the_app"), "{log}");
    assert!(claims_logged[2].contains("contested"), "{log}");
    for secret in [key_hex, hex(&digest), hex(&[0x3c; 32]), hex(&K2)] {
        assert!(!log.contains(&secret), "a key or digest reached the log: {log}");
    }
}

/// The decoded key wipes on hashing, never prints, and only 64 lower-case
/// hex digits decode.
#[test]
fn an_owner_key_is_lower_case_hex_wiped_after_hashing_and_never_printed() {
    use sha2::Digest;
    let bytes: [u8; 32] = std::array::from_fn(|i| (i as u8).wrapping_mul(37).wrapping_add(0xa1));
    let text = OwnerKey::from_bytes(bytes).to_hex().to_string();
    assert_eq!(text.len(), 64);
    assert_eq!(text, text.to_ascii_lowercase());
    let frame: ClientFrame = serde_json::from_str(&format!(r#"{{"type":"claim_owner","key":"{text}"}}"#)).expect("lower-case hex decodes");
    let ClientFrame::ClaimOwner { mut key } = frame else { panic!("claim_owner") };
    assert_eq!(key.held(), &bytes);
    assert_eq!(format!("{key:?}"), "OwnerKey([redacted])");
    assert!(!format!("{:?}", ClientFrame::ClaimOwner { key: key.clone() }).contains(&text[..16]));
    let digest = key.digest_and_wipe();
    assert_eq!(digest, <[u8; 32]>::from(sha2::Sha256::digest(bytes)));
    assert_eq!(key.held(), &[0u8; 32], "wiped before the verdict is reached");

    for bad in [text.to_ascii_uppercase(), text[..63].to_string(), format!("{text}0"), format!("{}g", &text[..63]), String::new()] {
        assert!(OwnerKey::parse(&bad).is_none(), "{bad}");
        let err = serde_json::from_str::<ClientFrame>(&format!(r#"{{"type":"claim_owner","key":"{bad}"}}"#)).unwrap_err();
        assert!(err.to_string().contains("claim_owner key must be 64 lower-case hex digits"), "{err}");
        assert!(!err.to_string().contains(&text[..16]), "the refusal never echoes the key: {err}");
    }
}

/// The key's bytes live boxed inside the wiping wrapper: moving the key
/// (out of the decoded frame, into a queue) moves the pointer, so no
/// unwiped copy of the 32 bytes is left where it was.
#[test]
fn an_owner_key_lives_boxed_so_a_move_leaves_no_copy() {
    let text = OwnerKey::from_bytes([0x7e; 32]).to_hex().to_string();
    let frame: ClientFrame = serde_json::from_str(&format!(r#"{{"type":"claim_owner","key":"{text}"}}"#)).expect("decodes");
    let ClientFrame::ClaimOwner { key } = frame else { panic!("claim_owner") };
    let at = key.held().as_ptr();
    let moved = Box::new(key);
    assert_eq!(moved.held().as_ptr(), at, "the bytes stayed where they were decoded");
    assert_eq!(moved.held(), &[0x7e; 32]);
}
