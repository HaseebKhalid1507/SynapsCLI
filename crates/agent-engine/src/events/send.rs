//! Strict send: deliver one event to exactly one live session and confirm it
//! from the session's receipt.
//!
//! The target is a session id or a session name, matched exactly. Nothing is
//! ever written anywhere else: no fallback, no broadcast, no prefix match. A
//! send is `sent` only when the session's listener acked this event's id,
//! which means the event is in that session's queue (not that a turn ran).
//! Anything that cannot be confirmed either way is `uncertain`.

use std::time::Duration;

use chrono::Utc;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

use super::registry::{self, SessionRegistration};
use super::socket::{NackReason, Receipt, MAX_PAYLOAD};
use super::types::{Event, EventChannel, EventContent, EventSender, EventSource, Severity};

/// How long a strict send waits to connect.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a strict send spends writing the event.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a strict send waits for the receipt.
pub const RECEIPT_TIMEOUT: Duration = Duration::from_secs(5);
/// After a failed write, how long a strict send still waits for a nack.
const NACK_GRACE: Duration = Duration::from_secs(1);
/// The longest receipt line read.
const RECEIPT_MAX: usize = 4096;
/// The longest source, in characters.
pub const SOURCE_MAX: usize = 64;

/// What happened to one strict send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SendResult {
    /// The session acked this event: it is in the session's queue.
    Sent,
    /// Nothing was queued.
    NotSent,
    /// The event may or may not be queued.
    Uncertain,
    /// The request itself was wrong; nothing was tried.
    Usage,
}

/// Why a strict send was not (or not certainly) sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Why {
    /// No live session has that id or name.
    NoLiveSession,
    /// More than one live session has that id or name.
    Ambiguous,
    /// The session registry could not be read.
    RegistryUnreadable,
    /// The session's card names a socket outside the run dir.
    SocketOutsideRunDir,
    /// The session's card names something that is not a socket.
    NotASocket,
    /// The session runs a build without receipts.
    ReceiverTooOld,
    /// The event is larger than a session accepts.
    Oversized,
    /// No connection to the session.
    ConnectFailed,
    /// The event could not be written whole.
    WriteFailed,
    /// The session refused the event as not an event.
    NackInvalid,
    /// The session refused the event as too large.
    NackOversized,
    /// The session's queue is full.
    NackQueueFull,
    /// The session did not get the whole event in time.
    NackTimeout,
    /// The session refused a sender running as another user.
    NackForeignUid,
    /// The session refused the event for a reason this build doesn't know.
    NackUnknown,
    /// The session closed without a readable receipt.
    NoReceipt,
    /// No receipt in time.
    ReceiptTimeout,
    /// The receipt names another event.
    ReceiptMismatch,
}

/// The result of one strict send, as `synaps send --strict --json` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outcome {
    pub result: SendResult,
    /// The id or name the caller asked for.
    pub target: String,
    /// The session the target resolved to, if it did.
    pub session_id: Option<String>,
    /// The event's id when it was sent or may have been; `None` when nothing
    /// was queued.
    pub event_id: Option<String>,
    pub why: Option<Why>,
    pub detail: Option<String>,
    /// From the ack: the session matched this sender's process to a session.
    /// It authenticates nothing.
    pub peer_verified: Option<bool>,
}

impl Outcome {
    fn new(result: SendResult, target: &str) -> Self {
        Outcome {
            result,
            target: target.to_string(),
            session_id: None,
            event_id: None,
            why: None,
            detail: None,
            peer_verified: None,
        }
    }

    /// A request refused before anything was tried.
    pub fn usage(target: &str, detail: impl Into<String>) -> Self {
        Outcome {
            detail: Some(detail.into()),
            ..Outcome::new(SendResult::Usage, target)
        }
    }

    /// A message refused as larger than a session accepts, before anything
    /// was tried: not sent.
    pub fn oversized(target: &str, detail: impl Into<String>) -> Self {
        Outcome::new(SendResult::NotSent, target).not_sent(Why::Oversized, detail)
    }

    fn not_sent(mut self, why: Why, detail: impl Into<String>) -> Self {
        self.result = SendResult::NotSent;
        self.event_id = None;
        self.why = Some(why);
        self.detail = Some(detail.into());
        self
    }

    fn uncertain(mut self, event_id: &str, why: Why, detail: impl Into<String>) -> Self {
        self.result = SendResult::Uncertain;
        self.event_id = Some(event_id.to_string());
        self.why = Some(why);
        self.detail = Some(detail.into());
        self
    }

    fn nack(self, reason: NackReason, detail: Option<String>) -> Self {
        let why = match reason {
            NackReason::Oversized => Why::NackOversized,
            NackReason::Invalid => Why::NackInvalid,
            NackReason::QueueFull => Why::NackQueueFull,
            NackReason::Timeout => Why::NackTimeout,
            NackReason::ForeignUid => Why::NackForeignUid,
            NackReason::Unknown => Why::NackUnknown,
        };
        let detail = detail.unwrap_or_else(|| "the session refused the event".to_string());
        self.not_sent(why, detail)
    }

    /// `synaps send`'s exit code: 0 sent, 2 usage, 3 not sent, 5 uncertain.
    pub fn exit_code(&self) -> i32 {
        match self.result {
            SendResult::Sent => 0,
            SendResult::Usage => 2,
            SendResult::NotSent => 3,
            SendResult::Uncertain => 5,
        }
    }
}

/// `low`, `medium`, `high` or `critical`, in any case. Anything else is an
/// error, never a default.
pub fn parse_severity(s: &str) -> Result<Severity, String> {
    match s.to_ascii_lowercase().as_str() {
        "low" => Ok(Severity::Low),
        "medium" => Ok(Severity::Medium),
        "high" => Ok(Severity::High),
        "critical" => Ok(Severity::Critical),
        _ => Err(format!(
            "severity must be low, medium, high or critical, not {:?}",
            clip(s, 40)
        )),
    }
}

/// 1 to [`SOURCE_MAX`] characters of `A-Z a-z 0-9 . _ : @ -`.
pub fn validate_source(s: &str) -> Result<(), String> {
    let ok = (1..=SOURCE_MAX).contains(&s.chars().count())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '@' | '-'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "source must be 1 to {SOURCE_MAX} characters of A-Z a-z 0-9 . _ : @ -, not {:?}",
            clip(s, 80)
        ))
    }
}

fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// One event for a strict send, checked.
#[derive(Debug, Clone)]
pub struct StrictMessage {
    source: String,
    severity: Severity,
    body: String,
    channel: Option<String>,
    content_type: String,
    sender: Option<EventSender>,
}

impl StrictMessage {
    /// The error is a usage message: a bad source or severity, or a body
    /// that is empty or only whitespace.
    pub fn new(source: &str, severity: &str, body: String) -> Result<Self, String> {
        validate_source(source)?;
        let severity = parse_severity(severity)?;
        if body.trim().is_empty() {
            return Err("the message is empty".to_string());
        }
        Ok(StrictMessage {
            source: source.to_string(),
            severity,
            body,
            channel: None,
            content_type: "message".to_string(),
            sender: None,
        })
    }

    pub fn with_channel(mut self, channel: Option<String>) -> Self {
        self.channel = channel;
        self
    }

    pub fn with_content_type(mut self, content_type: String) -> Self {
        self.content_type = content_type;
        self
    }

    /// The sending session, for a sender running in the receiving process.
    pub fn with_sender(mut self, sender: EventSender) -> Self {
        self.sender = Some(sender);
        self
    }

    /// A new event with a fresh id.
    pub fn to_event(&self) -> Event {
        Event {
            id: Uuid::new_v4().to_string(),
            timestamp: Utc::now(),
            source: EventSource {
                source_type: self.source.clone(),
                name: self.source.clone(),
                callback: None,
            },
            channel: self.channel.as_ref().map(|c| EventChannel {
                id: c.clone(),
                name: c.clone(),
            }),
            sender: self.sender.clone(),
            content: EventContent {
                text: self.body.clone(),
                content_type: self.content_type.clone(),
                severity: Some(self.severity.clone()),
                data: None,
                disclosure: None,
            },
            expects_response: false,
            reply_to: None,
            peer: None,
        }
    }
}

/// The time limits of one delivery.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timeouts {
    pub connect: Duration,
    pub write: Duration,
    pub receipt: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            connect: CONNECT_TIMEOUT,
            write: WRITE_TIMEOUT,
            receipt: RECEIPT_TIMEOUT,
        }
    }
}

/// Send `msg` to the one live session whose id or name is `target`, and
/// confirm it from the session's receipt. Nothing is created or written on
/// this side: not the run dir, not an inbox.
pub async fn send_strict(target: &str, msg: &StrictMessage) -> Outcome {
    send_strict_in(&registry::run_dir_path(), target, msg, Timeouts::default()).await
}

pub(crate) async fn send_strict_in(
    dir: &std::path::Path,
    target: &str,
    msg: &StrictMessage,
    timeouts: Timeouts,
) -> Outcome {
    let outcome = Outcome::new(SendResult::NotSent, target);
    let event = msg.to_event();
    let json = match serde_json::to_string(&event) {
        Ok(json) => json,
        Err(e) => {
            return outcome.not_sent(Why::WriteFailed, format!("cannot encode the event: {e}"))
        }
    };
    if json.len() + 1 > MAX_PAYLOAD {
        return outcome.not_sent(
            Why::Oversized,
            format!(
                "the event is {} bytes encoded; a session takes at most {} with its newline",
                json.len(),
                MAX_PAYLOAD
            ),
        );
    }
    let reg = match registry::resolve_exact_in(target, dir) {
        Ok(reg) => reg,
        Err((why, detail)) => return outcome.not_sent(why, detail),
    };
    deliver(outcome, &reg, &json, &event.id, timeouts).await
}

async fn deliver(
    mut outcome: Outcome,
    reg: &SessionRegistration,
    json: &str,
    event_id: &str,
    timeouts: Timeouts,
) -> Outcome {
    outcome.session_id = Some(reg.session_id.clone());
    let stream = match tokio::time::timeout(timeouts.connect, connect(&reg.socket_path)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => return outcome.not_sent(Why::ConnectFailed, format!("cannot connect: {e}")),
        Err(_) => {
            return outcome.not_sent(
                Why::ConnectFailed,
                format!("no connection within {} s", timeouts.connect.as_secs_f32()),
            )
        }
    };
    exchange(stream, outcome, json, event_id, timeouts).await
}

/// Write the event, end the request, read the receipt.
async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    outcome: Outcome,
    json: &str,
    event_id: &str,
    timeouts: Timeouts,
) -> Outcome {
    let wrote = tokio::time::timeout(timeouts.write, stream.write_all(json.as_bytes())).await;
    if !matches!(wrote, Ok(Ok(()))) {
        // A part of a compact JSON object never parses, so nothing was
        // queued. The session may still say why.
        let error = match wrote {
            Ok(Err(e)) => e.to_string(),
            _ => format!("not written within {} s", timeouts.write.as_secs_f32()),
        };
        if let Ok(Receipt::Nack { reason, detail }) = read_receipt(&mut stream, NACK_GRACE).await {
            return outcome.nack(reason, detail);
        }
        return outcome.not_sent(
            Why::WriteFailed,
            format!("the event was not written whole: {error}"),
        );
    }
    // The whole event is out: from here, anything short of a receipt is
    // uncertain. The newline ends the request where a half-close can't.
    let newline = tokio::time::timeout(timeouts.write, async {
        stream.write_all(b"\n").await?;
        stream.flush().await
    })
    .await;
    let _ = tokio::time::timeout(timeouts.write, stream.shutdown()).await;
    let note = match newline {
        Ok(Ok(())) => String::new(),
        Ok(Err(e)) => format!(" (the closing newline was not written: {e})"),
        Err(_) => " (the closing newline was not written in time)".to_string(),
    };
    match read_receipt(&mut stream, timeouts.receipt).await {
        Ok(Receipt::Ack {
            event_id: acked,
            peer_verified,
        }) if acked == event_id => Outcome {
            result: SendResult::Sent,
            event_id: Some(acked),
            peer_verified: Some(peer_verified),
            ..outcome
        },
        Ok(Receipt::Ack {
            event_id: acked, ..
        }) => outcome.uncertain(
            event_id,
            Why::ReceiptMismatch,
            format!(
                "the receipt names event {:?}, not this one",
                clip(&acked, 80)
            ),
        ),
        Ok(Receipt::Nack { reason, detail }) => outcome.nack(reason, detail),
        Err(ReadMiss::Timeout) => outcome.uncertain(
            event_id,
            Why::ReceiptTimeout,
            format!(
                "no receipt within {} s{note}",
                timeouts.receipt.as_secs_f32()
            ),
        ),
        Err(ReadMiss::Closed) => outcome.uncertain(
            event_id,
            Why::NoReceipt,
            format!("the session closed the connection without a receipt{note}"),
        ),
        Err(ReadMiss::Unreadable(line)) => outcome.uncertain(
            event_id,
            Why::NoReceipt,
            format!("unreadable receipt {:?}{note}", clip(&line, 120)),
        ),
    }
}

#[derive(Debug)]
enum ReadMiss {
    Timeout,
    Closed,
    Unreadable(String),
}

/// One receipt line: up to the first newline or the end of the stream, at
/// most [`RECEIPT_MAX`] bytes.
async fn read_receipt<S: AsyncRead + Unpin>(
    stream: &mut S,
    within: Duration,
) -> Result<Receipt, ReadMiss> {
    let mut buf = Vec::with_capacity(256);
    let read = tokio::time::timeout(within, read_line(stream, &mut buf)).await;
    let complete = buf.contains(&b'\n');
    match read {
        Err(_) if !complete => return Err(ReadMiss::Timeout),
        Ok(Err(_)) if buf.is_empty() => return Err(ReadMiss::Closed),
        _ => {}
    }
    let line = match buf.iter().position(|&b| b == b'\n') {
        Some(end) => &buf[..end],
        None => &buf[..],
    };
    let text = String::from_utf8_lossy(line);
    if text.trim().is_empty() {
        return Err(ReadMiss::Closed);
    }
    serde_json::from_str::<Receipt>(text.trim())
        .map_err(|_| ReadMiss::Unreadable(text.into_owned()))
}

async fn read_line<S: AsyncRead + Unpin>(stream: &mut S, buf: &mut Vec<u8>) -> std::io::Result<()> {
    let mut chunk = [0u8; 512];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.contains(&b'\n') || buf.len() >= RECEIPT_MAX {
            return Ok(());
        }
    }
}

/// The card's socket path, through [`super::socket::connect_session_socket`]:
/// a Unix socket, or on Windows the named pipe derived from it, opened only
/// when the server runs under this account (the same owner check as the
/// daemon pipe).
async fn connect(socket_path: &str) -> std::io::Result<super::socket::SessionClient> {
    super::socket::connect_session_socket(socket_path).await
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::events::queue::EventQueue;
    use crate::events::registry::SessionEndpoint;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::net::UnixListener;

    /// Short receipt wait, so the silent-server case is quick.
    fn quick() -> Timeouts {
        Timeouts {
            connect: Duration::from_secs(2),
            write: Duration::from_secs(5),
            receipt: Duration::from_millis(300),
        }
    }

    fn msg(body: &str) -> StrictMessage {
        StrictMessage::new("sender-a", "high", body.to_string()).unwrap()
    }

    /// A scratch run dir, named by its canonical path, which is what the
    /// resolver compares a card's socket with.
    struct RunDir {
        /// Held for its Drop: the dir goes when the test ends.
        _dir: tempfile::TempDir,
        path: PathBuf,
    }

    impl RunDir {
        fn path(&self) -> &Path {
            &self.path
        }
    }

    fn run_dir() -> RunDir {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().canonicalize().unwrap();
        RunDir { _dir: tmp, path }
    }

    fn session(
        dir: &Path,
        id: &str,
        name: Option<&str>,
        queue: Arc<EventQueue>,
    ) -> SessionEndpoint {
        SessionEndpoint::start_in(dir, id, name.map(Into::into), queue).unwrap()
    }

    /// A card for `id` naming `socket`, owned by `pid`.
    fn write_card(dir: &Path, id: &str, socket: &Path, pid: u32, receipts: u32) {
        let card = serde_json::json!({
            "kind": "session",
            "session_id": id,
            "name": null,
            "socket_path": socket.to_string_lossy(),
            "pid": pid,
            "started_at": Utc::now(),
            "receipts": receipts,
        });
        std::fs::write(dir.join(format!("{id}.json")), card.to_string()).unwrap();
    }

    fn listing(dir: &Path) -> Vec<PathBuf> {
        let mut names: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        names.sort();
        names
    }

    /// What a hand-made listener does with each connection.
    #[derive(Clone)]
    enum Fake {
        /// Reads the request to its end and closes: a build without receipts.
        NoReply,
        /// Reads the request to its end, then holds the connection.
        Silent,
        /// Reads the request to its end, then writes this line.
        Reply(&'static str),
        /// Reads a few bytes, writes this line if any, and closes.
        CloseEarly(Option<&'static str>),
    }

    /// A listener at `path`, with a card for `id` in `dir`. Returns the
    /// connection count and the bytes of the last request read.
    fn fake_session(
        dir: &Path,
        id: &str,
        path: PathBuf,
        receipts: u32,
        fake: Fake,
    ) -> (Arc<AtomicUsize>, Arc<std::sync::Mutex<Vec<u8>>>) {
        let listener = UnixListener::bind(&path).unwrap();
        write_card(dir, id, &path, std::process::id(), receipts);
        let accepted = Arc::new(AtomicUsize::new(0));
        let request = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (count, seen) = (accepted.clone(), request.clone());
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                count.fetch_add(1, Ordering::SeqCst);
                let fake = fake.clone();
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    if let Fake::CloseEarly(reply) = fake {
                        let mut few = [0u8; 10];
                        let _ = stream.read_exact(&mut few).await;
                        if let Some(line) = reply {
                            let _ = stream.write_all(line.as_bytes()).await;
                        }
                        return;
                    }
                    let _ = stream.read_to_end(&mut buf).await;
                    *seen.lock().unwrap() = buf;
                    match fake {
                        Fake::Silent => tokio::time::sleep(Duration::from_secs(3)).await,
                        Fake::Reply(line) => {
                            let _ = stream.write_all(line.as_bytes()).await;
                        }
                        _ => {}
                    }
                });
            }
        });
        (accepted, request)
    }

    #[tokio::test]
    async fn an_acked_event_is_sent_and_arrives_verbatim() {
        let tmp = run_dir();
        let queue = Arc::new(EventQueue::new(10));
        let _s = session(tmp.path(), "s-1", Some("alpha"), queue.clone());
        let body = "run $(rm -rf ~) `id` \"quoted\" 'single' \\n\nsecond line";
        for target in ["s-1", "alpha"] {
            let out = send_strict_in(tmp.path(), target, &msg(body), quick()).await;
            assert_eq!(out.result, SendResult::Sent, "{out:?}");
            assert_eq!(out.target, target);
            assert_eq!(out.session_id.as_deref(), Some("s-1"));
            assert_eq!((out.why, out.detail.as_deref()), (None, None));
            // The listener runs in this process and no sender is named.
            assert_eq!(out.peer_verified, Some(false));
            let queued = queue.pop().expect("queued");
            assert_eq!(out.event_id.as_deref(), Some(queued.id.as_str()));
            assert_eq!(queued.content.text, body);
            assert_eq!(queued.source.source_type, "sender-a");
            assert_eq!(queued.content.severity, Some(Severity::High));
            assert!(queue.is_empty());
        }
    }

    #[tokio::test]
    async fn a_prefix_never_matches() {
        let tmp = run_dir();
        let queue = Arc::new(EventQueue::new(10));
        let _s = session(tmp.path(), "s-1234", Some("alpha-long"), queue.clone());
        for target in ["s-12", "s-", "alpha", "S-1234"] {
            let out = send_strict_in(tmp.path(), target, &msg("hi"), quick()).await;
            assert_eq!(
                (out.result, out.why),
                (SendResult::NotSent, Some(Why::NoLiveSession)),
                "{target}"
            );
            assert_eq!((out.session_id, out.event_id), (None, None));
        }
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn a_name_that_is_another_sessions_id_is_ambiguous() {
        let tmp = run_dir();
        let (q1, q2) = (Arc::new(EventQueue::new(10)), Arc::new(EventQueue::new(10)));
        let _a = session(tmp.path(), "s-1", None, q1.clone());
        let _b = session(tmp.path(), "s-2", Some("s-1"), q2.clone());
        let out = send_strict_in(tmp.path(), "s-1", &msg("hi"), quick()).await;
        assert_eq!(
            (out.result, out.why),
            (SendResult::NotSent, Some(Why::Ambiguous)),
            "{out:?}"
        );
        let detail = out.detail.unwrap();
        assert!(detail.contains("s-1, s-2"), "{detail}");
        assert!(q1.is_empty() && q2.is_empty());
    }

    #[tokio::test]
    async fn a_stale_card_is_counted_and_left_in_place() {
        let tmp = run_dir();
        let dead_pid = i32::MAX as u32 + 1;
        write_card(
            tmp.path(),
            "gone",
            &tmp.path().join("gone.sock"),
            dead_pid,
            1,
        );
        let before = listing(tmp.path());
        let out = send_strict_in(tmp.path(), "gone", &msg("hi"), quick()).await;
        assert_eq!(
            (out.result, out.why),
            (SendResult::NotSent, Some(Why::NoLiveSession))
        );
        assert!(out.detail.unwrap().contains("(1 stale registration)"));
        // Read only: the card is still there, nothing was added.
        assert_eq!(listing(tmp.path()), before);
    }

    #[tokio::test]
    async fn a_socket_outside_the_run_dir_gets_no_connection() {
        let tmp = run_dir();
        let other = tempfile::tempdir().unwrap();
        let (accepted, _) = fake_session(
            tmp.path(),
            "away",
            other.path().join("x.sock"),
            1,
            Fake::Reply("{}\n"),
        );
        let out = send_strict_in(tmp.path(), "away", &msg("hi"), quick()).await;
        assert_eq!(
            (out.result, out.why),
            (SendResult::NotSent, Some(Why::SocketOutsideRunDir)),
            "{out:?}"
        );
        assert_eq!(accepted.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_card_naming_a_regular_file_is_refused() {
        let tmp = run_dir();
        let path = tmp.path().join("plain.sock");
        std::fs::write(&path, b"").unwrap();
        write_card(tmp.path(), "plain", &path, std::process::id(), 1);
        let out = send_strict_in(tmp.path(), "plain", &msg("hi"), quick()).await;
        assert_eq!(
            (out.result, out.why),
            (SendResult::NotSent, Some(Why::NotASocket)),
            "{out:?}"
        );
    }

    // Every `*.json` in the run dir is opened before it is known to be a
    // card: a FIFO there must not hang the send, and a file over the card
    // limit is not read as a card.
    #[test]
    fn a_fifo_or_an_oversized_file_in_the_run_dir_is_no_card() {
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = run_dir();
        let dir = tmp.path().to_path_buf();
        // A FIFO with no writer: a blocking open of it would never return.
        let fifo = dir.join("fifo.json");
        let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        // A card for a live session, padded past the limit. What a read cut
        // at the limit leaves would still parse.
        let socket = dir.join("pad.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let card = serde_json::json!({
            "kind": "session",
            "session_id": "pad",
            "name": null,
            "socket_path": socket.to_string_lossy(),
            "pid": std::process::id(),
            "started_at": Utc::now(),
            "receipts": 1,
        });
        let padded = format!("{card}{}", " ".repeat(64 * 1024));
        std::fs::write(dir.join("pad.json"), padded).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let query_dir = dir.clone();
        std::thread::spawn(move || {
            let found = registry::resolve_exact_in("pad", &query_dir).map(|r| r.session_id);
            let _ = tx.send(found);
        });
        let resolved = rx.recv_timeout(Duration::from_secs(2));
        if resolved.is_err() {
            // Release a blocked open, so the thread ends.
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo);
        }
        let resolved = resolved.expect("the registry was read within 2 s");
        let (why, detail) = resolved.expect_err("an oversized card is no card");
        assert_eq!(why, Why::NoLiveSession, "{detail}");
    }

    #[tokio::test]
    async fn a_session_without_receipts_is_too_old_and_gets_no_connection() {
        let tmp = run_dir();
        let (accepted, _) = fake_session(
            tmp.path(),
            "old",
            tmp.path().join("old.sock"),
            0,
            Fake::NoReply,
        );
        let out = send_strict_in(tmp.path(), "old", &msg("hi"), quick()).await;
        assert_eq!(
            (out.result, out.why),
            (SendResult::NotSent, Some(Why::ReceiverTooOld)),
            "{out:?}"
        );
        assert_eq!(out.session_id, None);
        assert_eq!(accepted.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_missing_run_dir_is_no_live_session_and_stays_missing() {
        let tmp = run_dir();
        let absent = tmp.path().join("run");
        let out = send_strict_in(&absent, "s-1", &msg("hi"), quick()).await;
        assert_eq!(
            (out.result, out.why),
            (SendResult::NotSent, Some(Why::NoLiveSession)),
            "{out:?}"
        );
        assert!(!absent.exists());
    }

    // A live owner whose socket is gone (a session restarting): the
    // connect says so.
    #[tokio::test]
    async fn a_missing_socket_is_a_failed_connect() {
        let tmp = run_dir();
        write_card(
            tmp.path(),
            "gap",
            &tmp.path().join("gap.sock"),
            std::process::id(),
            1,
        );
        let out = send_strict_in(tmp.path(), "gap", &msg("hi"), quick()).await;
        assert_eq!(
            (out.result, out.why),
            (SendResult::NotSent, Some(Why::ConnectFailed)),
            "{out:?}"
        );
        assert_eq!(
            (out.session_id.as_deref(), out.event_id),
            (Some("gap"), None)
        );
    }

    #[tokio::test]
    async fn an_oversized_event_is_refused_before_connecting() {
        let tmp = run_dir();
        let (accepted, _) = fake_session(
            tmp.path(),
            "big",
            tmp.path().join("big.sock"),
            1,
            Fake::NoReply,
        );
        let out = send_strict_in(tmp.path(), "big", &msg(&"x".repeat(MAX_PAYLOAD)), quick()).await;
        assert_eq!(
            (out.result, out.why),
            (SendResult::NotSent, Some(Why::Oversized)),
            "{out:?}"
        );
        assert_eq!(accepted.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_full_queue_is_not_sent() {
        let tmp = run_dir();
        let queue = Arc::new(EventQueue::new(1));
        queue.push(Event::simple("first", "first", None)).unwrap();
        let _s = session(tmp.path(), "full", None, queue.clone());
        let out = send_strict_in(tmp.path(), "full", &msg("hi"), quick()).await;
        assert_eq!(
            (out.result, out.why),
            (SendResult::NotSent, Some(Why::NackQueueFull)),
            "{out:?}"
        );
        assert_eq!(out.event_id, None);
        assert_eq!(queue.len(), 1);
    }

    #[tokio::test]
    async fn a_session_that_closes_without_a_receipt_is_uncertain() {
        let tmp = run_dir();
        let (_, seen) = fake_session(
            tmp.path(),
            "old",
            tmp.path().join("old.sock"),
            1,
            Fake::NoReply,
        );
        let out = send_strict_in(tmp.path(), "old", &msg("hi"), quick()).await;
        assert_eq!(
            (out.result, out.why),
            (SendResult::Uncertain, Some(Why::NoReceipt)),
            "{out:?}"
        );
        let id = out.event_id.expect("an uncertain send names its event");
        // The whole event and its newline were written.
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.last(), Some(&b'\n'));
        let event: Event = serde_json::from_slice(&seen).unwrap();
        assert_eq!(event.id, id);
    }

    #[tokio::test]
    async fn a_silent_session_is_uncertain_after_the_receipt_timeout() {
        let tmp = run_dir();
        fake_session(
            tmp.path(),
            "quiet",
            tmp.path().join("quiet.sock"),
            1,
            Fake::Silent,
        );
        let started = std::time::Instant::now();
        let out = send_strict_in(tmp.path(), "quiet", &msg("hi"), quick()).await;
        assert_eq!(
            (out.result, out.why),
            (SendResult::Uncertain, Some(Why::ReceiptTimeout)),
            "{out:?}"
        );
        assert!(out.event_id.is_some());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn an_ack_for_another_event_is_uncertain() {
        let tmp = run_dir();
        let ack = "{\"receipt\":\"ack\",\"event_id\":\"another\",\"peer_verified\":true}\n";
        fake_session(
            tmp.path(),
            "mix",
            tmp.path().join("mix.sock"),
            1,
            Fake::Reply(ack),
        );
        let out = send_strict_in(tmp.path(), "mix", &msg("hi"), quick()).await;
        assert_eq!(
            (out.result, out.why),
            (SendResult::Uncertain, Some(Why::ReceiptMismatch)),
            "{out:?}"
        );
        assert_eq!(out.peer_verified, None);
    }

    #[tokio::test]
    async fn an_unreadable_receipt_is_uncertain() {
        let tmp = run_dir();
        fake_session(
            tmp.path(),
            "odd",
            tmp.path().join("odd.sock"),
            1,
            Fake::Reply("ok, thanks\n"),
        );
        let out = send_strict_in(tmp.path(), "odd", &msg("hi"), quick()).await;
        assert_eq!(
            (out.result, out.why),
            (SendResult::Uncertain, Some(Why::NoReceipt)),
            "{out:?}"
        );
        assert!(out.detail.unwrap().starts_with("unreadable receipt"));
    }

    #[tokio::test]
    async fn a_nack_for_a_reason_this_build_does_not_know_is_not_sent() {
        let tmp = run_dir();
        let nack = "{\"receipt\":\"nack\",\"reason\":\"a_newer_reason\",\"detail\":\"later\"}\n";
        fake_session(
            tmp.path(),
            "new",
            tmp.path().join("new.sock"),
            1,
            Fake::Reply(nack),
        );
        let out = send_strict_in(tmp.path(), "new", &msg("hi"), quick()).await;
        assert_eq!(
            (out.result, out.why),
            (SendResult::NotSent, Some(Why::NackUnknown)),
            "{out:?}"
        );
        assert_eq!((out.event_id, out.detail.as_deref()), (None, Some("later")));
    }

    // A write cut short can't have been queued: not sent, with the
    // session's nack when it sent one.
    #[tokio::test]
    async fn a_write_cut_short_is_not_sent() {
        let body = "x".repeat(MAX_PAYLOAD - 4096);
        let nack = "{\"receipt\":\"nack\",\"reason\":\"invalid\",\"detail\":\"cut\"}\n";
        for (reply, why) in [(Some(nack), Why::NackInvalid), (None, Why::WriteFailed)] {
            let tmp = run_dir();
            fake_session(
                tmp.path(),
                "cut",
                tmp.path().join("cut.sock"),
                1,
                Fake::CloseEarly(reply),
            );
            let out = send_strict_in(tmp.path(), "cut", &msg(&body), quick()).await;
            assert_eq!(
                (out.result, out.why),
                (SendResult::NotSent, Some(why)),
                "{out:?}"
            );
            assert_eq!(out.event_id, None);
        }
    }

    #[test]
    fn the_outcome_line_has_stable_keys() {
        let out = Outcome {
            result: SendResult::Uncertain,
            target: "alpha".into(),
            session_id: Some("s-1".into()),
            event_id: Some("e-1".into()),
            why: Some(Why::NoReceipt),
            detail: Some("d".into()),
            peer_verified: None,
        };
        assert_eq!(
            serde_json::to_string(&out).unwrap(),
            r#"{"result":"uncertain","target":"alpha","session_id":"s-1","event_id":"e-1","why":"no_receipt","detail":"d","peer_verified":null}"#
        );
        let code = |result| {
            Outcome {
                result,
                ..out.clone()
            }
            .exit_code()
        };
        assert_eq!(
            [
                code(SendResult::Sent),
                code(SendResult::Usage),
                code(SendResult::NotSent),
                code(SendResult::Uncertain)
            ],
            [0, 2, 3, 5]
        );
    }

    #[test]
    fn severity_is_one_of_four_in_any_case() {
        assert_eq!(parse_severity("HIGH"), Ok(Severity::High));
        assert_eq!(parse_severity("Low"), Ok(Severity::Low));
        assert_eq!(parse_severity("medium"), Ok(Severity::Medium));
        assert_eq!(parse_severity("critical"), Ok(Severity::Critical));
        for bad in ["urgent", "", " high", "med"] {
            assert!(parse_severity(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_source_is_a_short_plain_token() {
        for good in ["cli", "sender-a", "a.b_c:d@e-1", &"s".repeat(SOURCE_MAX)] {
            assert_eq!(validate_source(good), Ok(()), "{good:?}");
        }
        for bad in [
            "",
            "has space",
            "quote\"",
            "angle<",
            "ünï",
            &"s".repeat(SOURCE_MAX + 1),
        ] {
            assert!(validate_source(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn an_empty_body_is_a_usage_error() {
        for body in ["", "  \n\t "] {
            assert!(
                StrictMessage::new("cli", "low", body.into()).is_err(),
                "{body:?}"
            );
        }
        assert!(StrictMessage::new("cli", "low", " x ".into()).is_ok());
        assert!(StrictMessage::new("bad source", "low", "x".into()).is_err());
        assert!(StrictMessage::new("cli", "urgent", "x".into()).is_err());
    }
}
