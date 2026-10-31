//! The `synaps-sudo-askpass` helper: the program sudo runs for `sudo -A`.
//!
//! It checks its parent is the system sudo before anything else, asks the
//! bash tool call that set it up (socket and token next to its own link),
//! and writes the answer to stdout, which must be sudo's pipe. It never
//! prompts by itself and fails closed: any doubt is exit 1, stdout empty.

use super::{
    sanitize_line, CallerPolicy, Refusal, SystemPolicy, HELPER_NAME, MAX_PROMPT, MAX_REPLY,
    SOCKET_NAME, TOKEN_NAME,
};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use zeroize::Zeroizing;

static DISPATCH: AtomicBool = AtomicBool::new(false);

/// Whether this binary routes `synaps-sudo-askpass` to the helper, i.e.
/// `main` called [`run_helper_if_invoked`]. Test harnesses and embedders do
/// not, so the bash tool must not point sudo at them.
pub(crate) fn dispatch_available() -> bool {
    DISPATCH.load(Ordering::SeqCst)
}

/// Call first thing in `main`. Returns the exit code when this process was
/// started as the askpass helper (argv[0] is the helper link), `None`
/// otherwise; either way it records that this binary can be the helper.
pub fn run_helper_if_invoked() -> Option<i32> {
    DISPATCH.store(true, Ordering::SeqCst);
    let mut args = std::env::args_os();
    let argv0 = std::path::PathBuf::from(args.next()?);
    if argv0.file_name() != Some(std::ffi::OsStr::new(HELPER_NAME)) {
        return None;
    }
    let prompt = args
        .next()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    Some(helper_main(
        &argv0,
        &prompt,
        &SystemPolicy,
        stdout_is_pipe(),
        &mut Stdout,
    ))
}

/// fd 1, unbuffered: the password must not linger in a std buffer.
struct Stdout;

impl Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // SAFETY: writes `buf.len()` initialised bytes from a live slice.
        let n = unsafe { libc::write(1, buf.as_ptr().cast(), buf.len()) };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn stdout_is_pipe() -> bool {
    // SAFETY: fstat writes into a zeroed, correctly sized stat buffer.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        libc::fstat(1, &mut st) == 0 && st.st_mode & libc::S_IFMT == libc::S_IFIFO
    }
}

pub(crate) fn helper_main(
    argv0: &Path,
    prompt: &str,
    policy: &dyn CallerPolicy,
    stdout_is_pipe: bool,
    out: &mut dyn Write,
) -> i32 {
    match ask(argv0, prompt, policy, stdout_is_pipe) {
        Ok(secret) => {
            let mut line = Zeroizing::new(Vec::with_capacity(secret.len() + 1));
            line.extend_from_slice(&secret);
            line.push(b'\n');
            match out.write_all(&line).and_then(|()| out.flush()) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("{HELPER_NAME}: cannot hand the password to sudo: {e}");
                    1
                }
            }
        }
        Err(refusal) => {
            eprintln!("{HELPER_NAME}: {refusal}");
            1
        }
    }
}

fn ask(
    argv0: &Path,
    prompt: &str,
    policy: &dyn CallerPolicy,
    stdout_is_pipe: bool,
) -> Result<Zeroizing<Vec<u8>>, Refusal> {
    // SAFETY: getppid has no preconditions and cannot fail.
    let parent = unsafe { libc::getppid() } as u32;
    policy.check_sudo(parent)?;
    if !stdout_is_pipe {
        return Err(Refusal::new("stdout is not a pipe to sudo"));
    }
    let dir = argv0
        .parent()
        .filter(|_| argv0.is_absolute())
        .ok_or_else(|| Refusal::new("not started through the synaps askpass link"))?;
    request_secret(dir, prompt)
}

/// Ask the broker listening in `dir` for the password.
pub(crate) fn request_secret(dir: &Path, prompt: &str) -> Result<Zeroizing<Vec<u8>>, Refusal> {
    check_private(dir, true)?;
    let token_path = dir.join(TOKEN_NAME);
    check_private(&token_path, false)?;
    let mut token = Zeroizing::new(Vec::new());
    std::fs::File::open(&token_path)
        .and_then(|f| f.take(256).read_to_end(&mut token))
        .map_err(|e| Refusal::new(format!("cannot read the askpass token: {e}")))?;

    let mut stream = std::os::unix::net::UnixStream::connect(dir.join(SOCKET_NAME))
        .map_err(|e| Refusal::new(format!("cannot reach the bash tool call: {e}")))?;
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
    let mut request = Zeroizing::new(Vec::new());
    request.extend_from_slice(token.trim_ascii());
    request.push(b'\n');
    request.extend(
        bounded(prompt, MAX_PROMPT)
            .bytes()
            .map(|b| if b == b'\n' { b' ' } else { b }),
    );
    request.push(b'\n');
    stream
        .write_all(&request)
        .and_then(|()| stream.shutdown(std::net::Shutdown::Write))
        .map_err(|e| Refusal::new(format!("cannot ask the bash tool call: {e}")))?;

    let mut reply = Zeroizing::new(Vec::with_capacity(MAX_REPLY + 1));
    (&stream)
        .take(MAX_REPLY as u64 + 1)
        .read_to_end(&mut reply)
        .map_err(|e| Refusal::new(format!("no answer from the bash tool call: {e}")))?;
    if reply.len() > MAX_REPLY {
        return Err(Refusal::new("oversized answer from the bash tool call"));
    }
    match reply.first() {
        Some(b'+') => Ok(Zeroizing::new(reply[1..].to_vec())),
        Some(b'-') => Err(Refusal::new(sanitize_line(
            &String::from_utf8_lossy(&reply[1..]),
            200,
        ))),
        _ => Err(Refusal::new("the bash tool call gave no answer")),
    }
}

fn bounded(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// `path` exists as itself (not a symlink), is a directory or a regular
/// file as asked, belongs to this user and is closed to everyone else.
fn check_private(path: &Path, dir: bool) -> Result<(), Refusal> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| Refusal::new(format!("{}: {e}", path.display())))?;
    let kind = if dir { meta.is_dir() } else { meta.is_file() };
    // SAFETY: geteuid has no preconditions and cannot fail.
    if !kind || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        return Err(Refusal::new(format!(
            "{} is not private to this user",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Policy(Result<(), Refusal>);

    impl CallerPolicy for Policy {
        fn check_sudo(&self, _pid: u32) -> Result<(), Refusal> {
            self.0.clone()
        }
        fn check_helper(&self, _pid: u32) -> Result<(), Refusal> {
            self.0.clone()
        }
    }

    fn accept() -> Policy {
        Policy(Ok(()))
    }

    /// A private dir with a token and a listening socket; `serve` answers
    /// one connection with `reply` and returns what it was sent.
    struct Fixture {
        dir: tempfile::TempDir,
        listener: std::os::unix::net::UnixListener,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let token = dir.path().join(TOKEN_NAME);
            std::fs::write(&token, "abc123\n").unwrap();
            std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
            let listener =
                std::os::unix::net::UnixListener::bind(dir.path().join(SOCKET_NAME)).unwrap();
            listener.set_nonblocking(true).unwrap();
            Self { dir, listener }
        }

        fn argv0(&self) -> std::path::PathBuf {
            self.dir.path().join(HELPER_NAME)
        }

        fn serve(&self, reply: &[u8]) -> std::thread::JoinHandle<Vec<u8>> {
            let listener = self.listener.try_clone().unwrap();
            let reply = reply.to_vec();
            std::thread::spawn(move || {
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(conn) => break conn,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                        Err(e) => panic!("accept: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                let mut request = Vec::new();
                stream.read_to_end(&mut request).unwrap();
                stream.write_all(&reply).unwrap();
                request
            })
        }

        fn was_contacted(&self) -> bool {
            self.listener.accept().is_ok()
        }
    }

    #[test]
    fn hands_the_answer_to_stdout_with_a_newline() {
        let fx = Fixture::new();
        let server = fx.serve(b"+swordfish");
        let mut out = Vec::new();
        let code = helper_main(
            &fx.argv0(),
            "[sudo] password for me: ",
            &accept(),
            true,
            &mut out,
        );
        assert_eq!(code, 0);
        assert_eq!(out, b"swordfish\n");
        assert_eq!(
            server.join().unwrap(),
            b"abc123\n[sudo] password for me: \n"
        );
    }

    #[test]
    fn a_parent_that_is_not_sudo_is_refused_before_anything_else() {
        let fx = Fixture::new();
        let mut out = Vec::new();
        let policy = Policy(Err(Refusal::new("the parent process is not sudo")));
        assert_eq!(helper_main(&fx.argv0(), "p", &policy, true, &mut out), 1);
        assert!(out.is_empty());
        assert!(!fx.was_contacted(), "refused before connecting");
    }

    #[test]
    fn stdout_must_be_a_pipe() {
        let fx = Fixture::new();
        let mut out = Vec::new();
        assert_eq!(helper_main(&fx.argv0(), "p", &accept(), false, &mut out), 1);
        assert!(out.is_empty());
        assert!(!fx.was_contacted());
    }

    #[test]
    fn a_relative_link_path_is_refused() {
        let mut out = Vec::new();
        let code = helper_main(Path::new(HELPER_NAME), "p", &accept(), true, &mut out);
        assert_eq!(code, 1);
        assert!(out.is_empty());
    }

    #[test]
    fn a_refusal_writes_nothing() {
        let fx = Fixture::new();
        let server = fx.serve(b"-no interactive client\x1b[2J");
        let err = request_secret(fx.dir.path(), "p").unwrap_err();
        server.join().unwrap();
        assert_eq!(err.0, r"no interactive client\u{1b}[2J");
        let mut out = Vec::new();
        let server = fx.serve(b"-cancelled");
        assert_eq!(helper_main(&fx.argv0(), "p", &accept(), true, &mut out), 1);
        server.join().unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn an_oversized_or_empty_answer_is_refused() {
        let fx = Fixture::new();
        let server = fx.serve(&vec![b'+'; MAX_REPLY + 2]);
        assert!(request_secret(fx.dir.path(), "p")
            .unwrap_err()
            .0
            .contains("oversized"));
        server.join().unwrap();
        let server = fx.serve(b"");
        assert!(request_secret(fx.dir.path(), "p")
            .unwrap_err()
            .0
            .contains("no answer"));
        server.join().unwrap();
    }

    /// The prompt is bounded and cannot break the request framing.
    #[test]
    fn the_forwarded_prompt_is_one_bounded_line() {
        let fx = Fixture::new();
        let server = fx.serve(b"+x");
        let prompt = format!("a\nb{}", "é".repeat(MAX_PROMPT));
        request_secret(fx.dir.path(), &prompt).unwrap();
        let request = server.join().unwrap();
        let text = String::from_utf8(request).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text:?}");
        assert!(lines[1].starts_with("a b"));
        assert!(lines[1].len() <= MAX_PROMPT);
    }

    #[test]
    fn the_directory_and_token_must_be_private() {
        let fx = Fixture::new();
        let token = fx.dir.path().join(TOKEN_NAME);

        std::fs::set_permissions(fx.dir.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
        let err = request_secret(fx.dir.path(), "p").unwrap_err();
        assert!(err.0.contains("not private"), "{err}");
        std::fs::set_permissions(fx.dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = request_secret(fx.dir.path(), "p").unwrap_err();
        assert!(err.0.contains("not private"), "{err}");

        std::fs::remove_file(&token).unwrap();
        let elsewhere = tempfile::NamedTempFile::new().unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), &token).unwrap();
        let err = request_secret(fx.dir.path(), "p").unwrap_err();
        assert!(err.0.contains("not private"), "a symlinked token: {err}");
        assert!(!fx.was_contacted());
    }
}
