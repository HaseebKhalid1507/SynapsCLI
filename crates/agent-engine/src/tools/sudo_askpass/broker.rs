//! The bash tool call's end of the askpass channel: a private directory
//! holding the helper link, the current token and a listening socket,
//! served for as long as the command runs.

use super::{
    procfs, prompt_text, CallerPolicy, Refusal, SystemPolicy, HELPER_NAME, MAX_PROMPT, MAX_REPLY,
    PROMPT_TITLE, SOCKET_NAME, TOKEN_NAME,
};
use crate::tools::bash::Redactions;
use crate::tools::SecretPromptHandle;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeroize::Zeroizing;

/// The helper's request: the token line, then the sudo prompt line.
const MAX_REQUEST: u64 = 256 + MAX_PROMPT as u64 + 2;
/// The helper writes its request at once; a silent peer is dropped.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// What sudo runs as its askpass program and who may ask.
pub(crate) struct AskpassConfig {
    helper_target: PathBuf,
    policy: Arc<dyn CallerPolicy>,
}

impl AskpassConfig {
    /// The real thing, if this binary dispatches the helper (see
    /// [`super::run_helper_if_invoked`]).
    pub(crate) fn production() -> Option<Self> {
        super::helper::dispatch_available().then(|| Self {
            // The running image itself: still this exact build after the
            // file on disk is replaced by an upgrade.
            helper_target: PathBuf::from(format!("/proc/{}/exe", std::process::id())),
            policy: Arc::new(SystemPolicy),
        })
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn for_tests(helper_target: PathBuf, policy: Arc<dyn CallerPolicy>) -> Self {
        Self {
            helper_target,
            policy,
        }
    }
}

/// Set up before the command starts, so `SUDO_ASKPASS` can be in its env.
pub(crate) struct PreparedBroker {
    dir: tempfile::TempDir,
    listener: std::os::unix::net::UnixListener,
    token: Zeroizing<String>,
    policy: Arc<dyn CallerPolicy>,
}

impl PreparedBroker {
    pub(crate) fn prepare(config: &AskpassConfig) -> std::io::Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("synaps-askpass-")
            .tempdir_in(runtime_base())?;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
        std::os::unix::fs::symlink(&config.helper_target, dir.path().join(HELPER_NAME))?;
        let token = new_token()?;
        write_token(dir.path(), &token)?;
        let socket = dir.path().join(SOCKET_NAME);
        let listener = std::os::unix::net::UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            dir,
            listener,
            token,
            policy: Arc::clone(&config.policy),
        })
    }

    /// What `SUDO_ASKPASS` names.
    pub(crate) fn helper_path(&self) -> PathBuf {
        self.dir.path().join(HELPER_NAME)
    }

    /// Serve requests from sudo running under `root_pid` (the command's
    /// shell). A cancelled prompt cancels `cancel` so the caller can stop
    /// the command, as before.
    pub(crate) fn start(
        self,
        root_pid: Option<u32>,
        prompt: Option<SecretPromptHandle>,
        redactions: Redactions,
        cancel: crate::CancellationToken,
    ) -> std::io::Result<RunningBroker> {
        let listener = tokio::net::UnixListener::from_std(self.listener)?;
        let broker = Broker {
            dir: self.dir.path().to_path_buf(),
            token: self.token,
            policy: self.policy,
            root_pid,
            prompt,
            redactions,
            cancel,
        };
        Ok(RunningBroker {
            task: tokio::spawn(broker.serve(listener)),
            _dir: self.dir,
        })
    }
}

/// Stops serving and removes the directory when the tool call ends.
pub(crate) struct RunningBroker {
    task: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Drop for RunningBroker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// `$XDG_RUNTIME_DIR` when it is ours and private, else the temp dir.
fn runtime_base() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        if dir.is_absolute() {
            if let Ok(meta) = std::fs::metadata(&dir) {
                // SAFETY: geteuid has no preconditions and cannot fail.
                let ours = meta.uid() == unsafe { libc::geteuid() };
                if meta.is_dir() && ours && meta.mode() & 0o077 == 0 {
                    return dir;
                }
            }
        }
    }
    std::env::temp_dir()
}

fn new_token() -> std::io::Result<Zeroizing<String>> {
    use rand::TryRngCore as _;
    use std::fmt::Write as _;
    let mut bytes = Zeroizing::new([0u8; 32]);
    rand::rngs::OsRng
        .try_fill_bytes(bytes.as_mut())
        .map_err(std::io::Error::other)?;
    let mut hex = Zeroizing::new(String::with_capacity(64));
    for b in bytes.iter() {
        let _ = write!(hex, "{b:02x}");
    }
    Ok(hex)
}

/// Publish `token` atomically (rename), mode 0600.
fn write_token(dir: &Path, token: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let next = dir.join("token.next");
    let _ = std::fs::remove_file(&next);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&next)?;
    file.write_all(token.as_bytes())?;
    std::fs::rename(&next, dir.join(TOKEN_NAME))
}

fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

struct Broker {
    dir: PathBuf,
    token: Zeroizing<String>,
    policy: Arc<dyn CallerPolicy>,
    root_pid: Option<u32>,
    prompt: Option<SecretPromptHandle>,
    redactions: Redactions,
    cancel: crate::CancellationToken,
}

impl Broker {
    /// One request at a time: the user answers one prompt at a time anyway.
    async fn serve(mut self, listener: tokio::net::UnixListener) {
        loop {
            let mut stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(e) => {
                    tracing::warn!("sudo askpass: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            let mut reply = Zeroizing::new(Vec::with_capacity(MAX_REPLY));
            match self.answer(&mut stream).await {
                Ok(secret) => {
                    reply.push(b'+');
                    reply.extend_from_slice(secret.as_bytes());
                }
                Err(refusal) => {
                    tracing::warn!("sudo askpass: request refused: {refusal}");
                    reply.push(b'-');
                    reply.extend_from_slice(refusal.0.as_bytes());
                }
            }
            reply.truncate(MAX_REPLY);
            let _ = tokio::time::timeout(REQUEST_TIMEOUT, stream.write_all(&reply)).await;
            let _ = stream.shutdown().await;
        }
    }

    async fn answer(
        &mut self,
        stream: &mut tokio::net::UnixStream,
    ) -> Result<Zeroizing<String>, Refusal> {
        let cred = stream
            .peer_cred()
            .map_err(|e| Refusal::new(format!("peer credentials unreadable: {e}")))?;
        // SAFETY: geteuid has no preconditions and cannot fail.
        if cred.uid() != unsafe { libc::geteuid() } {
            return Err(Refusal::new("the requester is another user"));
        }
        let helper = cred
            .pid()
            .and_then(|pid| u32::try_from(pid).ok())
            .filter(|pid| *pid > 0)
            .ok_or_else(|| Refusal::new("the requester has no pid"))?;
        let (token, sudo_prompt) = read_request(stream).await?;
        if !same_bytes(token.as_bytes(), self.token.as_bytes()) {
            return Err(Refusal::new("stale or invalid token"));
        }
        // One-shot: whatever happens next, this token is spent.
        self.token = new_token().map_err(|e| Refusal::new(format!("token rotation: {e}")))?;
        write_token(&self.dir, &self.token)
            .map_err(|e| Refusal::new(format!("token rotation: {e}")))?;

        self.policy.check_helper(helper)?;
        let sudo = procfs::status(helper)
            .map_err(|e| Refusal::new(format!("cannot inspect the requester: {e}")))?
            .ppid;
        self.policy.check_sudo(sudo)?;
        let root = self
            .root_pid
            .ok_or_else(|| Refusal::new("the command has no process"))?;
        self.policy.check_ancestry(sudo, root)?;
        let argv = procfs::cmdline(sudo)
            .map_err(|e| Refusal::new(format!("cannot read what sudo will run: {e}")))?;

        let Some(prompt) = &self.prompt else {
            return Err(Refusal::new(
                "no interactive client to ask for the sudo password",
            ));
        };
        match prompt
            .prompt(PROMPT_TITLE.to_string(), prompt_text(&argv, &sudo_prompt))
            .await
        {
            Some(secret) => {
                let secret = Zeroizing::new(secret);
                if !secret.is_empty() {
                    self.redactions
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push(secret.clone());
                }
                Ok(secret)
            }
            None => {
                self.cancel.cancel();
                Err(Refusal::new("the sudo password prompt was cancelled"))
            }
        }
    }
}

async fn read_request(
    stream: &mut tokio::net::UnixStream,
) -> Result<(Zeroizing<String>, String), Refusal> {
    let mut raw = Zeroizing::new(Vec::new());
    let mut limited = (&mut *stream).take(MAX_REQUEST);
    tokio::time::timeout(REQUEST_TIMEOUT, limited.read_to_end(&mut raw))
        .await
        .map_err(|_| Refusal::new("the request timed out"))?
        .map_err(|e| Refusal::new(format!("unreadable request: {e}")))?;
    let newline = raw
        .iter()
        .position(|b| *b == b'\n')
        .ok_or_else(|| Refusal::new("malformed request"))?;
    let token = Zeroizing::new(String::from_utf8_lossy(&raw[..newline]).into_owned());
    let rest = &raw[newline + 1..];
    let prompt = String::from_utf8_lossy(rest.strip_suffix(b"\n").unwrap_or(rest)).into_owned();
    Ok((token, prompt))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::sudo_askpass::helper::request_secret;
    use std::os::unix::fs::FileTypeExt;
    use std::sync::Mutex;

    /// Approves or refuses by fiat; records what it was asked about.
    #[derive(Default)]
    struct Fiat {
        refuse_sudo: bool,
        refuse_helper: bool,
        real_ancestry: bool,
        seen: Mutex<Vec<(&'static str, u32)>>,
    }

    impl CallerPolicy for Fiat {
        fn check_sudo(&self, pid: u32) -> Result<(), Refusal> {
            self.seen.lock().unwrap().push(("sudo", pid));
            if self.refuse_sudo {
                Err(Refusal::new("the parent process is not sudo"))
            } else {
                Ok(())
            }
        }
        fn check_helper(&self, pid: u32) -> Result<(), Refusal> {
            self.seen.lock().unwrap().push(("helper", pid));
            if self.refuse_helper {
                Err(Refusal::new(
                    "the requester is not the synaps askpass helper",
                ))
            } else {
                Ok(())
            }
        }
        fn check_ancestry(&self, pid: u32, root: u32) -> Result<(), Refusal> {
            if self.real_ancestry {
                if procfs::descends_from(pid, root) {
                    Ok(())
                } else {
                    Err(Refusal::new(
                        "sudo is not running inside this bash tool call",
                    ))
                }
            } else {
                Ok(())
            }
        }
    }

    struct Harness {
        broker: Option<RunningBroker>,
        dir: PathBuf,
        /// Answers for the prompts to come, in order.
        answers: tokio::sync::mpsc::UnboundedSender<Option<String>>,
        /// Bodies of the prompts raised so far.
        shown: Arc<Mutex<Vec<String>>>,
        redactions: Redactions,
        cancel: crate::CancellationToken,
    }

    impl Harness {
        fn answer(&self, answer: Option<&str>) {
            self.answers.send(answer.map(str::to_string)).unwrap();
        }

        fn shown(&self) -> Vec<String> {
            self.shown.lock().unwrap().clone()
        }
    }

    /// A running broker whose requester (this test process) is judged by
    /// `policy`, with `root` as the command's pid. A responder plays the
    /// user: it records each prompt and gives the next queued answer.
    fn harness(policy: Fiat, root: Option<u32>, with_prompt: bool) -> Harness {
        let config = AskpassConfig::for_tests(PathBuf::from("/bin/false"), Arc::new(policy));
        let prepared = PreparedBroker::prepare(&config).unwrap();
        let dir = prepared.dir.path().to_path_buf();
        let (tx, mut prompts) =
            tokio::sync::mpsc::unbounded_channel::<crate::tools::SecretPromptRequest>();
        let (answers, mut queued) = tokio::sync::mpsc::unbounded_channel::<Option<String>>();
        let shown = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&shown);
        tokio::spawn(async move {
            while let Some(req) = prompts.recv().await {
                assert_eq!(req.title, PROMPT_TITLE);
                seen.lock().unwrap().push(req.prompt.clone());
                let answer = queued.recv().await.flatten();
                let _ = req.response_tx.send(answer);
            }
        });
        let redactions = Redactions::default();
        let cancel = crate::CancellationToken::new();
        let broker = prepared
            .start(
                root,
                with_prompt.then(|| SecretPromptHandle::new(tx)),
                redactions.clone(),
                cancel.clone(),
            )
            .unwrap();
        Harness {
            broker: Some(broker),
            dir,
            answers,
            shown,
            redactions,
            cancel,
        }
    }

    fn me() -> Option<u32> {
        // The requester is this process; its parent plays sudo, and the
        // command's root is that parent, so a real ancestry check passes.
        // SAFETY: getppid has no preconditions.
        Some(unsafe { libc::getppid() } as u32)
    }

    async fn ask(dir: &Path, prompt: &str) -> Result<Zeroizing<Vec<u8>>, Refusal> {
        let (dir, prompt) = (dir.to_path_buf(), prompt.to_string());
        tokio::task::spawn_blocking(move || request_secret(&dir, &prompt))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_verified_request_gets_the_answer_and_it_is_redacted() {
        let h = harness(Fiat::default(), me(), true);
        h.answer(Some("swordfish"));
        let secret = ask(&h.dir, "[sudo] password for me: ").await.unwrap();
        assert_eq!(&secret[..], b"swordfish");
        let shown = h.shown();
        assert_eq!(shown.len(), 1);
        assert!(
            shown[0].starts_with("sudo is asking for your password to run:\nsudo "),
            "{shown:?}"
        );
        assert!(shown[0].ends_with("\n[sudo] password for me:"), "{shown:?}");
        assert!(!shown[0].contains("swordfish"));
        let redactions = h.redactions.lock().unwrap();
        assert_eq!(redactions.len(), 1);
        assert_eq!(redactions[0].as_str(), "swordfish");
        assert!(!h.cancel.is_cancelled());
    }

    /// The prompt names what the parent (sudo) runs, from its argv.
    #[tokio::test]
    async fn the_prompt_shows_the_sudo_command_line() {
        let h = harness(Fiat::default(), me(), true);
        h.answer(Some("x"));
        ask(&h.dir, "p").await.unwrap();
        // SAFETY: getppid has no preconditions.
        let parent = procfs::cmdline(unsafe { libc::getppid() } as u32).unwrap();
        let expected = super::super::display_command(&parent, 240);
        let shown = h.shown();
        assert!(shown[0].contains(&expected), "{shown:?} lacks {expected:?}");
    }

    #[tokio::test]
    async fn the_token_is_one_shot_and_a_wrong_one_is_refused() {
        let h = harness(Fiat::default(), me(), true);
        let first = std::fs::read_to_string(h.dir.join(TOKEN_NAME)).unwrap();
        h.answer(Some("a"));
        ask(&h.dir, "p").await.unwrap();
        let second = std::fs::read_to_string(h.dir.join(TOKEN_NAME)).unwrap();
        assert_ne!(first, second, "rotated after use");
        assert_eq!(second.len(), 64);

        // Replaying the spent token, or guessing: refused, nobody is asked.
        std::fs::write(h.dir.join(TOKEN_NAME), &first).unwrap();
        let err = ask(&h.dir, "p").await.unwrap_err();
        assert!(err.0.contains("stale or invalid token"), "{err}");
        std::fs::write(h.dir.join(TOKEN_NAME), "0".repeat(64)).unwrap();
        let err = ask(&h.dir, "p").await.unwrap_err();
        assert!(err.0.contains("stale or invalid token"), "{err}");
        assert_eq!(h.shown().len(), 1, "no prompt for a bad token");

        // The current token still works, once.
        std::fs::write(h.dir.join(TOKEN_NAME), &second).unwrap();
        h.answer(Some("b"));
        assert_eq!(&ask(&h.dir, "p").await.unwrap()[..], b"b");
        std::fs::write(h.dir.join(TOKEN_NAME), &second).unwrap();
        assert!(ask(&h.dir, "p").await.is_err());
        assert_eq!(h.shown().len(), 2);
    }

    #[tokio::test]
    async fn a_parent_that_is_not_sudo_is_refused_without_a_prompt() {
        let h = harness(
            Fiat {
                refuse_sudo: true,
                ..Fiat::default()
            },
            me(),
            true,
        );
        let err = ask(&h.dir, "p").await.unwrap_err();
        assert!(err.0.contains("not sudo"), "{err}");
        assert!(h.shown().is_empty());
    }

    #[tokio::test]
    async fn a_requester_that_is_not_the_helper_is_refused_without_a_prompt() {
        let h = harness(
            Fiat {
                refuse_helper: true,
                ..Fiat::default()
            },
            me(),
            true,
        );
        let err = ask(&h.dir, "p").await.unwrap_err();
        assert!(err.0.contains("not the synaps askpass helper"), "{err}");
        assert!(h.shown().is_empty());
    }

    /// The broker judges the socket peer itself: the helper's pid comes
    /// from the kernel, and the sudo it checks is that pid's parent.
    #[tokio::test]
    async fn the_checks_are_made_on_the_kernel_reported_peer() {
        let policy = Arc::new(Fiat::default());
        let config = AskpassConfig::for_tests(PathBuf::from("/bin/false"), policy.clone());
        let prepared = PreparedBroker::prepare(&config).unwrap();
        let dir = prepared.dir.path().to_path_buf();
        let _broker = prepared
            .start(
                me(),
                None,
                Redactions::default(),
                crate::CancellationToken::new(),
            )
            .unwrap();
        ask(&dir, "p").await.unwrap_err();
        // SAFETY: getppid has no preconditions.
        let parent = unsafe { libc::getppid() } as u32;
        assert_eq!(
            *policy.seen.lock().unwrap(),
            [("helper", std::process::id()), ("sudo", parent)]
        );
    }

    /// A sudo outside the command's process tree (another tool call, another
    /// session, any other process) is refused: here the "command" is a
    /// process that is not an ancestor of the requester.
    #[tokio::test]
    async fn a_sudo_outside_this_tool_call_is_refused() {
        let mut other = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let h = harness(
            Fiat {
                real_ancestry: true,
                ..Fiat::default()
            },
            Some(other.id()),
            true,
        );
        let err = ask(&h.dir, "p").await.unwrap_err();
        let _ = other.kill();
        let _ = other.wait();
        assert!(
            err.0.contains("not running inside this bash tool call"),
            "{err}"
        );
        assert!(h.shown().is_empty());

        let h = harness(
            Fiat {
                real_ancestry: true,
                ..Fiat::default()
            },
            me(),
            true,
        );
        h.answer(Some("ok"));
        assert_eq!(
            &ask(&h.dir, "p").await.unwrap()[..],
            b"ok",
            "inside: allowed"
        );
    }

    #[tokio::test]
    async fn without_a_prompt_capability_it_fails_closed() {
        let h = harness(Fiat::default(), me(), false);
        let err = ask(&h.dir, "p").await.unwrap_err();
        assert!(err.0.contains("no interactive client"), "{err}");
        assert!(
            !h.cancel.is_cancelled(),
            "sudo reports its own error; the command goes on"
        );
    }

    #[tokio::test]
    async fn a_cancelled_prompt_refuses_and_cancels_the_command() {
        let h = harness(Fiat::default(), me(), true);
        h.answer(None);
        let err = ask(&h.dir, "p").await.unwrap_err();
        assert!(err.0.contains("cancelled"), "{err}");
        assert!(h.cancel.is_cancelled());
        assert!(h.redactions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_garbled_request_does_not_wedge_the_broker() {
        let h = harness(Fiat::default(), me(), true);
        let mut garbled = tokio::net::UnixStream::connect(h.dir.join(SOCKET_NAME))
            .await
            .unwrap();
        garbled.write_all(b"no newline here").await.unwrap();
        garbled.shutdown().await.unwrap();
        let mut reply = Vec::new();
        garbled.read_to_end(&mut reply).await.unwrap();
        assert_eq!(reply, b"-malformed request");

        h.answer(Some("still"));
        assert_eq!(&ask(&h.dir, "p").await.unwrap()[..], b"still");
    }

    #[tokio::test]
    async fn the_directory_socket_and_token_are_private_and_go_away() {
        let mut h = harness(Fiat::default(), me(), true);
        let dir = std::fs::symlink_metadata(&h.dir).unwrap();
        assert!(dir.is_dir());
        assert_eq!(dir.mode() & 0o777, 0o700);
        // SAFETY: geteuid has no preconditions.
        assert_eq!(dir.uid(), unsafe { libc::geteuid() });
        let socket = std::fs::symlink_metadata(h.dir.join(SOCKET_NAME)).unwrap();
        assert!(socket.file_type().is_socket());
        assert_eq!(socket.mode() & 0o777, 0o600);
        let token = std::fs::symlink_metadata(h.dir.join(TOKEN_NAME)).unwrap();
        assert!(token.is_file());
        assert_eq!(token.mode() & 0o777, 0o600);
        let link = h.dir.join(HELPER_NAME);
        assert_eq!(std::fs::read_link(&link).unwrap(), Path::new("/bin/false"));

        h.answer(Some("x"));
        ask(&h.dir, "p").await.unwrap();
        let token = std::fs::symlink_metadata(h.dir.join(TOKEN_NAME)).unwrap();
        assert_eq!(token.mode() & 0o777, 0o600, "a rotated token stays private");

        drop(h.broker.take());
        assert!(!h.dir.exists(), "removed when the tool call ends");
    }

    #[test]
    fn the_production_link_targets_the_running_image() {
        // Test harnesses do not dispatch the helper: no production config.
        if !super::super::helper::dispatch_available() {
            assert!(AskpassConfig::production().is_none());
        }
        let config = AskpassConfig {
            helper_target: PathBuf::from(format!("/proc/{}/exe", std::process::id())),
            policy: Arc::new(SystemPolicy),
        };
        let meta = std::fs::metadata(&config.helper_target).unwrap();
        let ours = std::fs::metadata(std::env::current_exe().unwrap()).unwrap();
        assert_eq!((meta.dev(), meta.ino()), (ours.dev(), ours.ino()));
    }
}
