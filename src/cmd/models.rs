//! `synaps models --favorites [--json]`: the model picker's Favorites view
//! (the favorites of every usable provider, in the picker's order).
//!
//! It asks the running daemon (`model_favorites`), which also knows its
//! plugin providers and the live catalogs it already holds. With no daemon,
//! a daemon that predates the query, an error, or no answer within
//! [`DAEMON_WAIT`], it answers from this process instead: no extensions, no
//! network request, no token refresh. It never starts a daemon.
//!
//! [`DAEMON_WAIT`] is the whole query's deadline (connect, handshake and
//! reply). It is not the daemon's own wait for its extension manager, which
//! bounds only the plugin rows: past that wait the daemon answers without
//! them (`plugins_loaded: false`).

use std::path::Path;
use std::time::Duration;

use agent_engine::model_picker::{FavoriteModel, FavoritesView};
use agent_engine::session::socket_transport::{ModelFavoritesReply, QueryError, SocketTransport};
use serde::Serialize;

/// How long the daemon gets to answer (one deadline for connect, handshake
/// and reply) before the local answer is used. It bounds the query, not the
/// command: saying goodbye adds at most 200 ms after any outcome but a
/// timeout, and the local answer, when used, is built after it.
pub(crate) const DAEMON_WAIT: Duration = Duration::from_secs(3);

/// What `synaps models --favorites --json` prints.
#[derive(Debug, Serialize)]
pub(crate) struct FavoritesOutput {
    /// "daemon" or "local".
    pub source: &'static str,
    /// Why the answer is local: "no_daemon", "daemon_predates",
    /// "daemon_error" or "timeout". Absent on a daemon answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    /// True when the plugin rows are final (a daemon answer only; a local
    /// answer has none).
    pub plugins_loaded: bool,
    /// Providers whose rows came from a live catalog the daemon held.
    pub live_providers: Vec<String>,
    pub models: Vec<FavoriteModel>,
    /// A line for stderr (text and JSON output alike).
    #[serde(skip)]
    pub notice: Option<String>,
}

/// `synaps models`. Only `--favorites` is offered today.
pub async fn run(profile: Option<String>, favorites: bool, json: bool) -> anyhow::Result<()> {
    if !favorites {
        anyhow::bail!("`synaps models` lists the favorites only: use `synaps models --favorites [--json]`");
    }
    let paths = agent_engine::daemon::registry::daemon_paths(profile.as_deref());
    let sock = agent_engine::daemon::registry::is_alive(&paths).then(|| paths.sock.clone());
    let out = favorites_with(sock.as_deref(), DAEMON_WAIT, agent_engine::model_picker::local_favorites_view).await;
    if let Some(notice) = &out.notice {
        eprintln!("{notice}");
    }
    if json {
        println!("{}", serde_json::to_string(&out)?);
    } else {
        print!("{}", text(&out));
    }
    Ok(())
}

/// [`run`] for the daemon socket `sock` (`None`: no daemon is running).
/// `local` builds the view in this process when the daemon does not answer.
pub(crate) async fn favorites_with(
    sock: Option<&Path>,
    wait: Duration,
    local: impl FnOnce() -> FavoritesView,
) -> FavoritesOutput {
    let (reason, notice) = match sock {
        None => (
            "no_daemon",
            Some("no daemon is running: answering locally, without plugin providers or live catalogs".to_string()),
        ),
        Some(sock) => match SocketTransport::model_favorites(sock, tokio::time::Instant::now() + wait).await {
            Ok(ModelFavoritesReply::Answer { models, plugins_loaded, live_providers }) => {
                return FavoritesOutput {
                    source: "daemon",
                    reason: None,
                    plugins_loaded,
                    live_providers,
                    models,
                    notice: (!plugins_loaded)
                        .then(|| "the daemon is still loading its extensions: plugin providers may be missing".to_string()),
                };
            }
            Ok(ModelFavoritesReply::DaemonPredates { daemon_version, daemon_build }) => {
                let build = if daemon_build.is_empty() { String::new() } else { format!(", build {daemon_build}") };
                (
                    "daemon_predates",
                    Some(format!(
                        "the daemon predates `models --favorites` (daemon {daemon_version}{build}): answering locally, without plugin providers or live catalogs"
                    )),
                )
            }
            Err(QueryError::Transport(e)) => (
                "daemon_error",
                Some(format!("daemon at {}: {e}; answering locally, without plugin providers or live catalogs", sock.display())),
            ),
            Err(QueryError::TimedOut) => (
                "timeout",
                Some(format!(
                    "the daemon did not answer within {} s: answering locally, without plugin providers or live catalogs",
                    wait.as_secs_f32()
                )),
            ),
        },
    };
    let view = local();
    FavoritesOutput {
        source: "local",
        reason: Some(reason),
        plugins_loaded: false,
        live_providers: view.live_providers,
        models: view.models,
        notice,
    }
}

/// One row per line: id, label, provider (tab-separated).
fn text(out: &FavoritesOutput) -> String {
    out.models
        .iter()
        .map(|m| format!("{}\t{}\t{}\n", m.id, m.label.as_deref().unwrap_or(""), m.provider_name))
        .collect()
}

// Unix only: the fixture daemons listen on Unix sockets.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use agent_engine::session::wire::{decode_line, encode_line, ClientFrame, DaemonFrame, Hello, Welcome, PROTOCOL_VERSION};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    /// How the fixture daemon answers the `model_favorites` frame.
    #[derive(Clone)]
    enum Answer {
        Rows(Vec<FavoriteModel>, bool),
        /// As an older daemon whose `ClientFrame` has no such variant.
        Old,
        Error(&'static str),
        /// Read the frame and say nothing.
        Silent,
        /// A result for another id, its rows carrying [`SENTINEL`].
        WrongId,
        /// A session's error carrying [`SENTINEL`].
        SessionError,
    }

    /// Stands in for a secret in a frame that is not the answer.
    const SENTINEL: &str = "sk-sentinel-0f1e2d";

    /// An older daemon's client frames (no `model_favorites`).
    #[derive(serde::Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    #[allow(dead_code, clippy::large_enum_variant)]
    enum OldClientFrame {
        Hello(Hello),
        Ping,
        Bye,
    }

    fn row(id: &str) -> FavoriteModel {
        FavoriteModel {
            id: id.into(),
            label: Some(id.to_uppercase()),
            provider_key: "p".into(),
            provider_name: "P".into(),
            badges: vec![],
        }
    }

    fn local() -> FavoritesView {
        FavoritesView { models: vec![row("local/x")], live_providers: vec![] }
    }

    /// The fixture daemon's `Welcome`, listing `features`.
    fn welcome(features: Vec<String>) -> DaemonFrame {
        DaemonFrame::Welcome(Welcome {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: "0.10.1".into(),
            pid: 1,
            profile: None,
            sessions: vec![],
            progressive_tool_disclosure: false,
            generation: 1,
            daemon_build: "oldbld1".into(),
            features,
            accepted_events: vec![],
            config_path: None,
            memory_backend: None,
        })
    }

    /// A fixture daemon for one connection: `Welcome` listing `features`,
    /// then `answer`. Returns the socket and how many `model_favorites`
    /// frames reached it.
    async fn fixture(features: &[&str], answer: Answer) -> (tempfile::TempDir, std::path::PathBuf, Arc<AtomicUsize>) {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let asked = Arc::new(AtomicUsize::new(0));
        let seen = asked.clone();
        let features: Vec<String> = features.iter().map(|f| f.to_string()).collect();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut lines = BufReader::new(r).lines();
            let hello = lines.next_line().await.unwrap().expect("hello");
            assert!(matches!(decode_line::<ClientFrame>(&hello), Ok(ClientFrame::Hello(_))), "{hello}");
            w.write_all(encode_line(&welcome(features)).unwrap().as_bytes()).await.unwrap();
            while let Ok(Some(line)) = lines.next_line().await {
                let reply = match &answer {
                    Answer::Old => match decode_line::<OldClientFrame>(&line) {
                        Ok(OldClientFrame::Bye) => break,
                        Ok(_) => panic!("unexpected frame {line}"),
                        Err(e) => {
                            seen.fetch_add(1, Ordering::SeqCst);
                            DaemonFrame::Error { session_id: None, message: format!("malformed frame: {e}") }
                        }
                    },
                    answer => match decode_line::<ClientFrame>(&line).unwrap() {
                        ClientFrame::Bye => break,
                        ClientFrame::ModelFavorites { id } => {
                            seen.fetch_add(1, Ordering::SeqCst);
                            match answer {
                                Answer::Rows(models, plugins_loaded) => DaemonFrame::ModelFavoritesResult {
                                    id,
                                    models: models.clone(),
                                    plugins_loaded: *plugins_loaded,
                                    live_providers: vec!["xai-auth".into()],
                                },
                                Answer::Error(message) => DaemonFrame::Error { session_id: None, message: message.to_string() },
                                Answer::Silent => continue,
                                Answer::WrongId => DaemonFrame::ModelFavoritesResult {
                                    id: id.map(|id| id + 1),
                                    models: vec![row(SENTINEL)],
                                    plugins_loaded: true,
                                    live_providers: vec![SENTINEL.into()],
                                },
                                Answer::SessionError => DaemonFrame::Error {
                                    session_id: Some("s1".into()),
                                    message: format!("refused: {SENTINEL}"),
                                },
                                Answer::Old => unreachable!(),
                            }
                        }
                        other => panic!("unexpected frame {other:?}"),
                    },
                };
                w.write_all(encode_line(&reply).unwrap().as_bytes()).await.unwrap();
            }
        });
        (dir, sock, asked)
    }

    /// A daemon that accepts the connection and never says a word (no
    /// `Welcome`). The connection stays open until the test ends.
    async fn mute_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        (dir, sock)
    }

    /// A fixture daemon for one connection that writes `welcome_line` as
    /// its `Welcome` and `reply_line` as its answer to `model_favorites`,
    /// both as given. Returns the socket and how many `model_favorites`
    /// frames reached it.
    async fn raw_fixture(welcome_line: String, reply_line: String) -> (tempfile::TempDir, std::path::PathBuf, Arc<AtomicUsize>) {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let asked = Arc::new(AtomicUsize::new(0));
        let seen = asked.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut lines = BufReader::new(r).lines();
            let _hello = lines.next_line().await;
            let _ = w.write_all(format!("{welcome_line}\n").as_bytes()).await;
            while let Ok(Some(line)) = lines.next_line().await {
                if !matches!(decode_line::<ClientFrame>(&line), Ok(ClientFrame::ModelFavorites { .. })) {
                    break;
                }
                seen.fetch_add(1, Ordering::SeqCst);
                let _ = w.write_all(format!("{reply_line}\n").as_bytes()).await;
            }
        });
        (dir, sock, asked)
    }

    /// `frame` as a line, its field `field` holding [`SENTINEL`]: a value
    /// of the wrong type, so the frame does not decode.
    fn with_sentinel(frame: &DaemonFrame, field: &str) -> String {
        let mut v = serde_json::to_value(frame).unwrap();
        assert!(v.get(field).is_some(), "{field} in {v}");
        v[field] = serde_json::json!(SENTINEL);
        v.to_string()
    }

    #[tokio::test]
    async fn no_daemon_answers_locally() {
        let out = favorites_with(None, DAEMON_WAIT, local).await;
        assert_eq!((out.source, out.reason, out.plugins_loaded), ("local", Some("no_daemon"), false));
        assert_eq!(out.models, vec![row("local/x")]);
        assert!(out.notice.as_deref().is_some_and(|n| n.starts_with("no daemon is running: answering locally")), "{:?}", out.notice);
        assert_eq!(
            serde_json::to_value(&out).unwrap(),
            serde_json::json!({"source": "local", "reason": "no_daemon", "plugins_loaded": false, "live_providers": [],
                "models": [{"id": "local/x", "label": "LOCAL/X", "provider_key": "p", "provider_name": "P"}]})
        );
    }

    #[tokio::test]
    async fn a_daemon_answer_is_passed_through_and_the_local_view_is_not_built() {
        let (_d, sock, asked) = fixture(&["model_favorites"], Answer::Rows(vec![row("a/1"), row("plug:p:m")], true)).await;
        let out = favorites_with(Some(&sock), DAEMON_WAIT, || panic!("no local view on a daemon answer")).await;
        assert_eq!((out.source, out.reason, out.plugins_loaded), ("daemon", None, true));
        assert_eq!(out.models, vec![row("a/1"), row("plug:p:m")]);
        assert_eq!(out.live_providers, ["xai-auth"]);
        assert!(out.notice.is_none());
        assert_eq!(asked.load(Ordering::SeqCst), 1);
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v["source"], "daemon");
        assert!(v.get("reason").is_none(), "{v}");
        // Still loading its extensions: said so.
        let (_d, sock, _) = fixture(&["model_favorites"], Answer::Rows(vec![], false)).await;
        let out = favorites_with(Some(&sock), DAEMON_WAIT, local).await;
        assert_eq!((out.source, out.plugins_loaded), ("daemon", false));
        assert!(out.notice.unwrap().contains("still loading"));
    }

    #[tokio::test]
    async fn an_older_daemon_is_not_asked_or_answers_unknown_variant() {
        // Its Welcome lacks the feature: the frame is never sent.
        let (_d, sock, asked) = fixture(&["children", "thinking_options"], Answer::Old).await;
        let out = favorites_with(Some(&sock), DAEMON_WAIT, local).await;
        assert_eq!((out.source, out.reason), ("local", Some("daemon_predates")));
        assert_eq!(out.models, vec![row("local/x")]);
        assert!(out.notice.unwrap().contains("daemon 0.10.1, build oldbld1"));
        assert_eq!(asked.load(Ordering::SeqCst), 0);
        // It lists the feature but cannot decode the frame (an old decoder).
        let (_d, sock, asked) = fixture(&["model_favorites"], Answer::Old).await;
        let out = favorites_with(Some(&sock), DAEMON_WAIT, local).await;
        assert_eq!((out.source, out.reason), ("local", Some("daemon_predates")));
        assert_eq!(asked.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_error_or_no_answer_in_time_falls_back_locally() {
        let (_d, sock, _) = fixture(&["model_favorites"], Answer::Error("model favorites: task failed")).await;
        let out = favorites_with(Some(&sock), DAEMON_WAIT, local).await;
        assert_eq!((out.source, out.reason), ("local", Some("daemon_error")));
        assert!(out.notice.unwrap().contains("model favorites: task failed"));

        // A silent daemon, at the production deadline: one after `Welcome`
        // (no reply), one before it (no `Welcome`). Each waits the whole
        // DAEMON_WAIT, past the transport's own 2 s step budgets, and ends
        // as a timeout, never as a daemon error.
        let (_d, silent, asked) = fixture(&["model_favorites"], Answer::Silent).await;
        let (_m, mute) = mute_fixture().await;
        let timed = |sock: std::path::PathBuf| async move {
            let started = std::time::Instant::now();
            let out = favorites_with(Some(&sock), DAEMON_WAIT, local).await;
            (out, started.elapsed())
        };
        let ((out, took), (mute_out, mute_took)) = tokio::join!(timed(silent), timed(mute));
        for (out, took) in [(&out, took), (&mute_out, mute_took)] {
            assert_eq!((out.source, out.reason), ("local", Some("timeout")), "{:?}", out.notice);
            assert!(took >= DAEMON_WAIT - Duration::from_millis(50), "the whole deadline: {took:?}");
            assert!(took < DAEMON_WAIT + Duration::from_secs(1), "bounded: {took:?}");
            assert!(out.notice.as_deref().is_some_and(|n| n.contains("did not answer within 3 s")), "{:?}", out.notice);
            assert_eq!(out.models, vec![row("local/x")]);
        }
        assert_eq!(asked.load(Ordering::SeqCst), 1);

        // A socket path with nothing listening: an error, never a spawn.
        let dir = tempfile::tempdir().unwrap();
        let out = favorites_with(Some(&dir.path().join("none.sock")), DAEMON_WAIT, local).await;
        assert_eq!((out.source, out.reason), ("local", Some("daemon_error")));
    }

    /// A frame that is not this query's answer (a result for another id, a
    /// session's error) fails the query by its kind only: nothing it
    /// carries reaches the notice (stderr) or the output.
    #[tokio::test]
    async fn a_frame_that_is_not_the_answer_never_shows_its_payload() {
        for (answer, kind) in [(Answer::WrongId, "got model_favorites_result 2"), (Answer::SessionError, "got a session's error")] {
            let (_d, sock, asked) = fixture(&["model_favorites"], answer).await;
            let out = favorites_with(Some(&sock), DAEMON_WAIT, local).await;
            assert_eq!((out.source, out.reason), ("local", Some("daemon_error")));
            assert_eq!(asked.load(Ordering::SeqCst), 1);
            let notice = out.notice.clone().unwrap_or_default();
            assert!(notice.contains(kind), "{notice}");
            assert!(!notice.contains(SENTINEL), "{notice}");
            assert!(!serde_json::to_string(&out).unwrap().contains(SENTINEL));
            assert!(!text(&out).contains(SENTINEL));
        }
    }

    /// A `Welcome` or a reply that does not decode, its bad field value
    /// carrying [`SENTINEL`]: the query fails as a daemon error that names
    /// the frame's size only. Nothing from it reaches the notice (stderr),
    /// the JSON or the text output.
    #[tokio::test]
    async fn a_frame_that_does_not_decode_never_shows_its_values() {
        let features = vec!["model_favorites".to_string()];
        let good_welcome = encode_line(&welcome(features.clone())).unwrap().trim_end().to_string();
        let result =
            DaemonFrame::ModelFavoritesResult { id: Some(1), models: vec![row("a/1")], plugins_loaded: true, live_providers: vec![] };
        let cases = [
            ("welcome", with_sentinel(&welcome(features), "protocol_version"), encode_line(&result).unwrap().trim_end().to_string(), 0),
            ("reply", good_welcome, with_sentinel(&result, "plugins_loaded"), 1),
        ];
        for (what, welcome_line, reply_line, asks) in cases {
            let (_d, sock, asked) = raw_fixture(welcome_line, reply_line).await;
            let out = favorites_with(Some(&sock), DAEMON_WAIT, local).await;
            assert_eq!((out.source, out.reason), ("local", Some("daemon_error")), "{what}: {:?}", out.notice);
            assert_eq!(asked.load(Ordering::SeqCst), asks, "{what}");
            assert_eq!(out.models, vec![row("local/x")], "{what}");
            let notice = out.notice.clone().unwrap_or_default();
            assert!(notice.contains("a frame that does not decode ("), "{what}: {notice}");
            assert!(!notice.contains(SENTINEL), "{what}: {notice}");
            assert!(!serde_json::to_string(&out).unwrap().contains(SENTINEL), "{what}");
            assert!(!text(&out).contains(SENTINEL), "{what}");
        }
    }

    #[test]
    fn text_output_is_one_row_per_line() {
        let mut unlabelled = row("plug:p:m");
        unlabelled.label = None;
        let out = FavoritesOutput {
            source: "daemon",
            reason: None,
            plugins_loaded: true,
            live_providers: vec![],
            models: vec![row("a/1"), unlabelled],
            notice: None,
        };
        assert_eq!(text(&out), "a/1\tA/1\tP\nplug:p:m\t\tP\n");
    }
}
