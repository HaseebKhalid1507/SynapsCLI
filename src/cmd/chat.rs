//! `synaps chat` — fully-featured headless mode, hosted by the session daemon.
//!
//! Same engine as the TUI (MCP, extensions, skills, session persistence,
//! compaction), rendered to stdin/stdout. Built for scripting, piping, SSH,
//! CI, and agent benchmark frameworks like Harbor.
//!
//! The session runs in the profile's daemon (started on demand, exactly as
//! `synaps --attach` does) and is OWNED by this process: it is created with
//! this process's cwd and full environment (secret env vars forwarded to the
//! daemon's memory only, for the session's tools), and it ends — any running
//! turn cancelled, the journal kept — when chat exits, is killed or loses its
//! connection. stdin EOF and `/quit` end it cleanly first (`End` → `Ended`),
//! so an immediate `--continue` never races the dying session.
//! `--no-extensions` uses a private daemon booted without extensions.
//! Output goes through an ordered in-process queue ([`pump`]): a blocked
//! terminal never stops chat reading the session, so the daemon never ends
//! it for owner lag.
//!
//! C4a: chat continues turns when pending runtime events are injected at
//! turn end (AutoTriggerEvents, the actor's wake path), bounded by
//! AUTO_TURN_CAP.
//!
//! C4b: the event stream and stdin are selected together, so a runtime event
//! wakes the idle prompt at once. stdin is only polled while the session is
//! idle, so a piped line is always a `Submit`. Piped stdin, EOF, and CRLF
//! behaviour are preserved.

use agent_engine::daemon::{self, DaemonOpts, EnsureError, EXIT_DAEMON_LOST, EXIT_REFUSED, EXIT_VERSION};
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::{Attach, Hello, FEATURE_NO_EXTENSIONS, MAX_FRAME_BYTES};
use agent_engine::session::{
    AttachMode, ClientKind, ClientTransport, EndReason, Envelope, SessionCommand, SessionConfig,
    SessionEventWire, SessionQuery, SessionSetting, TransportError,
};
use synaps_cli::engine::commands;
use synaps_cli::{AgentEvent, LlmEvent, SessionEvent, StreamEvent};
use tokio::io::{AsyncBufReadExt, BufReader as TokioBufReader};

/// chat's `print!`/`eprint!`: through [`pump`], never straight to the fds.
macro_rules! out {
    ($($t:tt)*) => { pump::out(format!($($t)*)) };
}
macro_rules! outln {
    () => { pump::out(String::from("\n")) };
    ($($t:tt)*) => { pump::out(format!("{}\n", format_args!($($t)*))) };
}
macro_rules! err {
    ($($t:tt)*) => { pump::err(format!($($t)*)) };
}
macro_rules! errln {
    () => { pump::err(String::from("\n")) };
    ($($t:tt)*) => { pump::err(format!("{}\n", format_args!($($t)*))) };
}

/// Query id for the post-compaction token readout (user queries start at 1
/// and count up). Below `wire::RESERVED_QUERY_ID_BASE`: the socket
/// transport swallows the results of ids at or above it.
const COMPACT_TOKENS_QUERY: u64 = agent_engine::session::wire::RESERVED_QUERY_ID_BASE - 1;

/// Headroom for the frame envelope around a command (session id, tags).
const FRAME_OVERHEAD: usize = 4096;

struct Render {
    is_tty: bool,
    in_thinking: bool,
    streaming: bool,
    idle: bool,
    fatal: Option<synaps_cli::TurnError>,
    cost: f64,
    ended: bool,
    /// A `Confirm` prompt waiting for a y/n line from a TTY user (the
    /// next stdin line answers it). Secret prompts are never held here:
    /// the line client has no masked input and cancels them.
    pending_confirm: Option<agent_engine::session::PromptRequest>,
}

impl Render {
    fn end_thinking(&mut self) {
        if self.in_thinking {
            errln!("\x1b[0m");
            self.in_thinking = false;
        }
    }

    async fn on_event(&mut self, transport: &SocketTransport, env: Envelope) {
        match env.event {
            SessionEventWire::Stream(ev) => self.on_stream(ev),
            SessionEventWire::TurnStarted { .. } => {
                self.streaming = true;
                self.idle = false;
            }
            SessionEventWire::Conversation(c) => self.cost = c.cost,
            SessionEventWire::Idle => self.idle = true,
            SessionEventWire::Prompt(pr) => {
                if pr.kind == agent_engine::session::PromptKind::Confirm
                    && self.is_tty
                    && self.pending_confirm.is_none()
                {
                    // Confirm (e.g. `activate_tools`): show the FULL body
                    // and take one y/n line. Anything but y/yes denies.
                    self.end_thinking();
                    // Sanitized (prA I-R4-3): no cursor moves or bidi tricks.
                    err!(
                        "\n[confirm] {}\n{}\n(y/n): ",
                        agent_engine::session::sanitize_prompt_text(&pr.title),
                        agent_engine::session::sanitize_prompt_text(&pr.prompt)
                    );
                    self.pending_confirm = Some(pr);
                    return;
                }
                // Headless chat has no masked prompt UI: cancel it.
                let _ = transport
                    .send(SessionCommand::Answer {
                        prompt_id: pr.id,
                        value: None,
                    })
                    .await;
            }
            SessionEventWire::PromptResolved { prompt_id } => {
                if self.pending_confirm.as_ref().is_some_and(|p| p.id == prompt_id) {
                    self.pending_confirm = None;
                }
            }
            SessionEventWire::External(event) => {
                errln!(
                    "\x1b[36m⚡ [event] {}\x1b[0m",
                    synaps_cli::events::format_event_for_agent(&event)
                );
            }
            SessionEventWire::AutoTurnCapReached { cap } => {
                errln!(
                    "\x1b[2m[auto-turn cap ({}) reached — waiting for user input]\x1b[0m",
                    cap
                );
            }
            SessionEventWire::SystemNotice(text) => errln!("\x1b[2m{}\x1b[0m", text),
            SessionEventWire::Aborted { context_saved } => errln!(
                "\x1b[2m{}\x1b[0m",
                if context_saved {
                    "aborted — partial work kept"
                } else {
                    "aborted"
                }
            ),
            SessionEventWire::Cleared { session_id } => errln!(
                "session cleared → {}",
                &session_id[..8.min(session_id.len())]
            ),
            // B2: spawned compaction reports through typed events — each
            // rendered exactly once.
            SessionEventWire::CompactionStarted { disclosure, .. } => {
                errln!("compacting...");
                errln!("{}", disclosure);
            }
            SessionEventWire::CompactionApplied { .. } => {
                // `compacted → ~N tokens` needs the post-apply assessment.
                let _ = transport
                    .send(SessionCommand::Query {
                        id: COMPACT_TOKENS_QUERY,
                        query: SessionQuery::ContextAssessment,
                    })
                    .await;
            }
            SessionEventWire::QueryResult { id, value } if id == COMPACT_TOKENS_QUERY => {
                errln!(
                    "\x1b[2m[compacted → ~{} tokens]\x1b[0m",
                    value["used_tokens"].as_u64().unwrap_or(0)
                )
            }
            SessionEventWire::CompactionFailed { message, .. } => {
                errln!("\x1b[2m[compaction failed: {}]\x1b[0m", message)
            }
            SessionEventWire::CompactionCancelled => {
                errln!("\x1b[2m[compaction cancelled]\x1b[0m")
            }
            SessionEventWire::Ended { .. } => self.ended = true,
            _ => {}
        }
    }

    fn on_stream(&mut self, ev: StreamEvent) {
        match ev {
            StreamEvent::Llm(LlmEvent::Thinking(text)) => {
                if !self.in_thinking {
                    err!("\x1b[2m");
                    self.in_thinking = true;
                }
                err!("{}", text);
            }
            StreamEvent::Llm(LlmEvent::Text(text)) => {
                self.end_thinking();
                out!("{}", text);
            }
            StreamEvent::Llm(LlmEvent::ToolUseStart { tool_name, .. }) => {
                self.end_thinking();
                err!("\x1b[33m⚙ {}\x1b[0m", tool_name);
            }
            StreamEvent::Llm(LlmEvent::ToolUse {
                tool_name, input, ..
            }) => {
                let input_preview = serde_json::to_string(&input).unwrap_or_default();
                let preview: String = input_preview.chars().take(60).collect();
                errln!("\x1b[33m ⚙ {} ({})\x1b[0m", tool_name, preview);
            }
            StreamEvent::Llm(LlmEvent::ToolResult { result, .. }) => {
                let preview: String = result.chars().take(80).collect();
                errln!("\x1b[32m  → {}\x1b[0m", preview);
            }
            StreamEvent::Agent(AgentEvent::SubagentStart {
                agent_name,
                task_preview,
                ..
            }) => errln!("\x1b[35m🎭 [{}] {}\x1b[0m", agent_name, task_preview),
            StreamEvent::Agent(AgentEvent::SubagentDone {
                result_preview,
                duration_secs,
                ..
            }) => {
                let status = if result_preview.starts_with("[TIMED OUT") {
                    "\u{26a0} timed out".to_string()
                } else if result_preview.starts_with("ERROR") {
                    let preview: String = result_preview.chars().take(40).collect();
                    format!("\u{2718} {}", preview)
                } else {
                    let preview: String = result_preview.chars().take(40).collect();
                    format!("\u{2714} {}", preview)
                };
                errln!("\x1b[32m✔ {} ({:.1}s)\x1b[0m", status, duration_secs);
            }
            StreamEvent::Agent(AgentEvent::SteeringDelivered { message }) => {
                errln!("\x1b[33m→ [steering] {}\x1b[0m", message);
            }
            StreamEvent::Session(SessionEvent::Notice(text) | SessionEvent::Retry(text)) => {
                errln!("\x1b[2m{}\x1b[0m", text);
            }
            StreamEvent::Session(SessionEvent::Done) => {
                self.end_thinking();
                outln!();
                self.streaming = false;
            }
            StreamEvent::Session(SessionEvent::Error(err)) => {
                errln!("\x1b[31m❌ {}\x1b[0m", err.message);
                self.end_thinking();
                outln!();
                errln!("\x1b[31m❌ turn failed [{}]\x1b[0m", err.category_label());
                self.fatal = Some(err);
                self.streaming = false;
            }
            _ => {}
        }
    }

    /// Pump events until the reply for query `id` arrives.
    async fn reply(&mut self, t: &mut SocketTransport, id: u64) -> serde_json::Value {
        loop {
            let Some(env) = next(t, self).await else { break };
            if let SessionEventWire::QueryResult { id: rid, value } = &env.event {
                if *rid == id {
                    return value.clone();
                }
            }
            self.on_event(t, env).await;
            if self.ended {
                break;
            }
        }
        serde_json::Value::Null
    }
}

/// The next session event. A daemon reload (`Reloading`) re-binds this
/// owned session within the owner grace (`reconnect` caps its budget at
/// it); any other loss of the connection — the daemon gone, or this owner
/// disconnected for lagging (`OwnerLagged`) — is fatal: the owned session
/// is gone with it, so chat says why and exits [`EXIT_DAEMON_LOST`].
/// `None` = the session ended (`Ended` was seen).
async fn next(t: &mut SocketTransport, r: &mut Render) -> Option<Envelope> {
    loop {
        if let Some(env) = t.next_event().await {
            return Some(env);
        }
        if r.ended {
            return None;
        }
        match t.last_error() {
            Some(TransportError::Reloading { .. }) => match t.reconnect(AttachMode::Mirror).await {
                Ok(snap) => {
                    r.streaming = snap.streaming;
                    r.idle = !snap.streaming;
                    continue;
                }
                Err(e) => lost(&format!("the daemon reloaded and the session could not be re-bound: {e}")),
            },
            Some(TransportError::OwnerLagged { dropped }) => lost(&format!(
                "chat fell behind the session's event stream ({dropped} event(s) dropped; is stdout blocked?) \
                 and the daemon disconnected it rather than drop output"
            )),
            Some(e) => lost(&e.to_string()),
            None => lost("the daemon closed the connection"),
        }
    }
}

fn lost(why: &str) -> ! {
    r_end_thinking_raw();
    errln!(
        "synaps chat: lost the session: {why}\n  the session ended with this connection (its journal is kept); \
         resume it with `synaps chat --continue <id>`"
    );
    pump::exit(EXIT_DAEMON_LOST);
}

/// Reset any open dim (thinking) attribute before a fatal line.
fn r_end_thinking_raw() {
    err!("\x1b[0m");
}

/// `cmd` fits in one daemon frame (the daemon drops a connection that sends
/// an oversize frame, which would end this owned session).
fn fits_frame(cmd: &SessionCommand) -> bool {
    serde_json::to_vec(cmd).map(|v| v.len() + FRAME_OVERHEAD <= MAX_FRAME_BYTES).unwrap_or(false)
}

/// What chat prints when no daemon can be had, and how to fix it.
fn no_daemon(profile: Option<&str>, no_extensions: bool, error: Option<&str>) -> ! {
    errln!("synaps chat: {}", super::attach::daemon_unavailable_message(profile, no_extensions, error));
    pump::exit(EXIT_REFUSED);
}

/// Entry point: connect to (or start) the profile's daemon and run an owned
/// session over its socket. Everything printed is written out before this
/// returns.
pub async fn run(
    continue_session: Option<String>,
    system: Option<String>,
    agent: Option<String>,
    profile: Option<String>,
    no_extensions: bool,
) -> synaps_cli::Result<()> {
    let result = run_session(continue_session, system, agent, profile, no_extensions).await;
    pump::drain();
    result
}

async fn run_session(
    continue_session: Option<String>,
    system: Option<String>,
    agent: Option<String>,
    profile: Option<String>,
    no_extensions: bool,
) -> synaps_cli::Result<()> {
    // Agent prompt resolves before any session work (exit 1).
    let agent_prompt = match agent {
        Some(ref agent_name) => match synaps_cli::tools::resolve_agent_prompt(agent_name) {
            Ok(p) => {
                errln!("🎭 Agent: {}", agent_name);
                Some(p)
            }
            Err(e) => {
                errln!("❌ {}", e);
                pump::exit(1);
            }
        },
        None => None,
    };
    if let Some(w) = daemon::legacy_env_warning() {
        errln!("synaps chat: {w}");
    }
    let config = synaps_cli::load_config();
    pump::drain(); // the warnings below write to stderr directly
    agent_engine::host::report_config_warnings(&config);

    // ── the daemon: running, or started now (`--attach`'s autospawn) ──
    let opts = DaemonOpts { profile: profile.clone(), no_extensions, ..Default::default() };
    let paths = opts.paths();
    let spawn_opts = opts.clone();
    let ensured = tokio::task::spawn_blocking(move || daemon::ensure_running(&spawn_opts))
        .await
        .map_err(|e| synaps_cli::RuntimeError::Session(format!("daemon start: {e}")))?;
    match ensured {
        Ok(_) => {}
        Err(EnsureError::AutospawnDisabled) => {
            no_daemon(profile.as_deref(), no_extensions, None)
        }
        Err(EnsureError::Spawn(reason)) => {
            no_daemon(profile.as_deref(), no_extensions, Some(&format!("could not start it: {reason}")))
        }
    }
    let conn = match SocketTransport::connect(&paths.sock, Hello::new(ClientKind::Chat)).await {
        Ok(c) => c,
        Err(TransportError::Version { client, daemon }) => {
            errln!(
                "synaps chat: protocol version mismatch (client {client}, daemon {daemon}); \
                 run `synaps daemon reload` to restart the daemon on this binary"
            );
            pump::exit(EXIT_VERSION);
        }
        Err(e) => no_daemon(profile.as_deref(), no_extensions, Some(&format!("{}: {e}", paths.sock.display()))),
    };
    if let Some(msg) = daemon::profile_mismatch(conn.welcome.profile.as_deref(), profile.as_deref()) {
        errln!("synaps chat: {msg}");
        pump::exit(EXIT_REFUSED);
    }
    if conn.welcome.supports(FEATURE_NO_EXTENSIONS) != no_extensions {
        errln!(
            "synaps chat: the daemon at {} {} extensions; refusing to run this session there",
            paths.sock.display(),
            if no_extensions { "runs" } else { "does not run" }
        );
        pump::exit(EXIT_REFUSED);
    }

    // The session runs where chat runs. `--system` keeps its in-process
    // meaning: an existing file (relative to this cwd) is read here, any
    // other value is the prompt text itself.
    let cwd = std::env::current_dir()
        .map_err(|e| synaps_cli::RuntimeError::Session(format!("current directory: {e}")))?;
    let system = system.map(|v| match std::path::Path::new(&v).is_file() {
        true => std::fs::read_to_string(&v).unwrap_or(v),
        false => v,
    });
    let session_cfg = SessionConfig {
        continue_session: continue_session.map(Some),
        system,
        auto_compact: true,
        cwd: Some(cwd),
        ..SessionConfig::default()
    };
    let (mut t, snap) = match SocketTransport::attach(
        conn,
        Attach::create_owned(session_cfg, AttachMode::Mirror, true),
    )
    .await
    {
        Ok(x) => x,
        Err(e) => {
            errln!("synaps chat: could not create the session in the daemon: {e}");
            pump::exit(EXIT_REFUSED);
        }
    };
    if let Some(p) = agent_prompt {
        let _ = t
            .send(SessionCommand::Set { id: 0, setting: SessionSetting::SystemPrompt { text: p } })
            .await;
    }

    let session_id = snap.meta.id.as_str().to_string();
    errln!(
        "synaps {} | {} | session {}",
        env!("CARGO_PKG_VERSION"),
        snap.view.model,
        &session_id[..8.min(session_id.len())]
    );
    if snap.meta.continued {
        errln!(
            "↳ resumed session ({} messages)",
            snap.conversation.api_messages.len()
        );
    }
    let mcp_tools = agent_engine::mcp::cached_tool_count();
    if mcp_tools > 0 {
        errln!("↳ {} MCP servers available", mcp_tools);
    }
    errln!();

    let mut r = Render {
        is_tty: std::io::IsTerminal::is_terminal(&std::io::stdin()),
        in_thinking: false,
        streaming: snap.streaming,
        idle: !snap.streaming,
        fatal: None,
        cost: snap.conversation.cost,
        ended: false,
        pending_confirm: None,
    };
    let mut pending_attachments = agent_engine::attachments::PendingAttachments::default();
    let mut stdin_lines = TokioBufReader::new(tokio::io::stdin()).lines();
    let mut next_query: u64 = 1;
    let mut prompt_shown = false;

    loop {
        if r.idle && r.is_tty && !prompt_shown {
            err!("❯ ");
            prompt_shown = true;
        }
        // stdin only while idle: a piped line is always a Submit.
        let line = tokio::select! {
            biased;
            ev = next(&mut t, &mut r) => match ev {
                Some(env) => {
                    r.on_event(&t, env).await;
                    if r.ended { break; }
                    // Headless (piped): an unrecovered failure terminates
                    // after the turn settles; TTY users keep their session.
                    if r.idle && r.fatal.is_some() && !r.is_tty { break; }
                    if r.idle && r.fatal.is_some() { r.fatal = None; }
                    continue;
                }
                None => break,
            },
            line = stdin_lines.next_line(), if r.idle || r.pending_confirm.is_some() => match line {
                Ok(Some(l)) => l,
                Ok(None) => break,
                Err(e) => { errln!("input error: {}", e); break; }
            },
        };
        prompt_shown = false;

        // A pending confirm owns the next line: `y`/`yes` allows, anything
        // else (including empty) denies — fail-closed, never a Submit.
        if let Some(pr) = r.pending_confirm.take() {
            let answer = line.trim();
            let value = if answer.is_empty() { None } else { Some(answer.to_string()) };
            let _ = t
                .send(SessionCommand::Answer { prompt_id: pr.id, value })
                .await;
            continue;
        }

        let trimmed = line.trim_end_matches('\r').trim();
        // Blank-line attachment-only submit.
        if trimmed.is_empty() {
            if !pending_attachments.is_empty() {
                let summaries = pending_attachments.summaries();
                for s in &summaries {
                    errln!("📎 {s}");
                }
                let cmd = SessionCommand::Submit {
                    text: String::new(),
                    attachments: pending_attachments.take_blocks(),
                };
                if !fits_frame(&cmd) {
                    oversize();
                    continue;
                }
                r.idle = false;
                if t.send(cmd).await.is_err() {
                    break;
                }
            }
            continue;
        }

        if let Some((cmd, arg)) = commands::parse_command(trimmed) {
            // Client-side attachment commands — never reach the session.
            match cmd {
                "attach" => {
                    if arg.is_empty() {
                        errln!("usage: /attach <path>");
                        continue;
                    }
                    let path_str = arg.trim_matches(|c| c == '"' || c == '\'');
                    let path = std::path::PathBuf::from(path_str);
                    match agent_engine::attachments::load_attachment_sync(&path) {
                        Ok(loaded) => match pending_attachments.add(loaded) {
                            Ok(()) => {
                                let s = pending_attachments.summaries();
                                errln!("📎 staged: {}", s.last().unwrap_or(&String::new()));
                            }
                            Err(e) => errln!("error: {e}"),
                        },
                        Err(e) => {
                            errln!("error: {path_str}: {e}");
                            if !r.is_tty {
                                // Piped: fail-stop (the session still ends cleanly).
                                end_session(&mut t, &mut r).await;
                                return Err(synaps_cli::RuntimeError::Session(
                                    format!("attachment failed: {path_str}: {e}"),
                                ));
                            }
                        }
                    }
                    continue;
                }
                "attachments" => {
                    let summaries = pending_attachments.summaries();
                    if summaries.is_empty() {
                        errln!("no pending attachments");
                    } else {
                        for (i, s) in summaries.iter().enumerate() {
                            errln!("  [{i}] {s}");
                        }
                    }
                    continue;
                }
                "detach" => {
                    let n = pending_attachments.len();
                    if arg.is_empty() || arg == "all" || arg == "clear" {
                        pending_attachments.clear();
                        if n > 0 {
                            errln!("detached {n} file(s)");
                        } else {
                            errln!("no pending attachments");
                        }
                    } else if let Ok(idx) = arg.parse::<usize>() {
                        if idx < n {
                            pending_attachments.remove(idx);
                            errln!("detached [{idx}]");
                        } else {
                            errln!("no attachment at index {idx}");
                        }
                    } else {
                        errln!("usage: /detach [n|all]");
                    }
                    continue;
                }
                _ => {} // fall through to EngineCommand
            }

            let id = next_query;
            next_query += 1;
            let _ = t
                .send(SessionCommand::EngineCommand {
                    id,
                    name: cmd.to_string(),
                    arg: arg.to_string(),
                })
                .await;
            let reply = r.reply(&mut t, id).await;
            if r.ended {
                break;
            }
            match reply["kind"].as_str().unwrap_or("none") {
                "quit" => break,
                "notice" => errln!("{}", reply["text"].as_str().unwrap_or("")),
                "error" => errln!("error: {}", reply["text"].as_str().unwrap_or("")),
                "output" => outln!("{}", reply["text"].as_str().unwrap_or("")),
                "unhandled" => match cmd {
                    "clear" => {
                        let _ = t.send(SessionCommand::NewSession).await;
                    }
                    "sessions" => {
                        let id = next_query;
                        next_query += 1;
                        let _ = t
                            .send(SessionCommand::Query {
                                id,
                                query: SessionQuery::Status,
                            })
                            .await;
                        let status = r.reply(&mut t, id).await;
                        let current = status["session"].as_str().unwrap_or("");
                        match synaps_cli::list_recent_sessions(20) {
                            Ok(sessions) => {
                                for s in sessions.iter().take(20) {
                                    let marker = if s.id == current { "→ " } else { "  " };
                                    errln!(
                                        "{}{} {} ({}, ${:.4})",
                                        marker,
                                        &s.id[..8],
                                        s.title,
                                        s.model,
                                        s.session_cost
                                    );
                                }
                            }
                            Err(e) => errln!("error: {}", e),
                        }
                    }
                    "status" => {
                        let id = next_query;
                        next_query += 1;
                        let _ = t
                            .send(SessionCommand::Query {
                                id,
                                query: SessionQuery::Status,
                            })
                            .await;
                        let st = r.reply(&mut t, id).await;
                        let id2 = next_query;
                        next_query += 1;
                        let _ = t
                            .send(SessionCommand::Query {
                                id: id2,
                                query: SessionQuery::ContextAssessment,
                            })
                            .await;
                        let ctx = r.reply(&mut t, id2).await;
                        let sid = st["session"].as_str().unwrap_or("");
                        errln!("session: {}", &sid[..8.min(sid.len())]);
                        errln!("model: {}", st["model"].as_str().unwrap_or(""));
                        errln!(
                            "tokens: {}↑ {}↓",
                            st["tokens"]["input"].as_u64().unwrap_or(0),
                            st["tokens"]["output"].as_u64().unwrap_or(0)
                        );
                        errln!("cost: ${:.4}", st["cost"].as_f64().unwrap_or(0.0));
                        errln!("messages: {}", st["messages"].as_u64().unwrap_or(0));
                        errln!(
                            "context: ~{} of {} budget tokens ({} window)",
                            ctx["used_tokens"].as_u64().unwrap_or(0),
                            ctx["budget_tokens"].as_u64().unwrap_or(0),
                            ctx["provider_window"].as_u64().unwrap_or(0)
                        );
                    }
                    "help" => {
                        errln!("commands: /model /thinking /compact /clear /sessions /status /attach /attachments /detach /quit");
                        errln!("quitting (or EOF) mid-turn cancels the turn; its partial work is kept in history for the next --continue");
                    }
                    _ => errln!("unknown command: /{} (try /help)", cmd),
                },
                _ => {}
            }
            continue;
        }

        // Regular user message.
        let attachment_blocks = if pending_attachments.is_empty() {
            Vec::new()
        } else {
            let summaries = pending_attachments.summaries();
            for s in &summaries {
                errln!("📎 {s}");
            }
            pending_attachments.take_blocks()
        };
        let cmd = SessionCommand::Submit {
            text: trimmed.to_string(),
            attachments: attachment_blocks,
        };
        if !fits_frame(&cmd) {
            oversize();
            if !r.is_tty {
                end_session(&mut t, &mut r).await;
                return Err(synaps_cli::RuntimeError::Session(
                    "message too large for one daemon frame".into(),
                ));
            }
            continue;
        }
        r.idle = false;
        if t.send(cmd).await.is_err() {
            break;
        }
    }

    // ── Shutdown: End → the session saves, fires on_session_end, ends ──
    end_session(&mut t, &mut r).await;

    if !pending_attachments.is_empty() {
        errln!(
            "warning: {} unsent attachment(s) discarded",
            pending_attachments.len()
        );
    }

    errln!(
        "session saved: {} (${:.4})",
        &session_id[..8.min(session_id.len())],
        r.cost
    );

    if let Some(err) = r.fatal {
        return Err(synaps_cli::RuntimeError::Session(format!(
            "turn failed: {} [{}]",
            err.message,
            err.category_label()
        )));
    }
    Ok(())
}

fn oversize() {
    errln!(
        "error: message too large for one daemon frame ({} MiB limit); not sent",
        MAX_FRAME_BYTES / (1024 * 1024)
    );
}

/// `End{ClientQuit}` and wait for `Ended`: the session is saved (and its
/// `on_session_end` ran) before chat prints "session saved" and exits, so
/// an immediate `--continue` never races the dying session.
async fn end_session(t: &mut SocketTransport, r: &mut Render) {
    if r.ended {
        return;
    }
    let _ = t
        .send(SessionCommand::End {
            reason: EndReason::ClientQuit,
        })
        .await;
    while !r.ended {
        match t.next_event().await {
            Some(env) => r.on_event(t, env).await,
            None => break,
        }
    }
}

/// chat's output: ordered, and never blocking the session reader.
///
/// chat's session is owned, and the daemon ends an owned session whose
/// owner stops reading its events (PR 3: lag → `Bye{owner_lagged}`). A
/// terminal that stops taking output (a paused terminal, `| less`, a slow
/// pipe) must not do that, so nothing chat prints is written on the reader's
/// path: stdout and stderr chunks go, in order, through one queue that a
/// writer thread drains (flushing each, as the old `flush_stdout` calls did).
/// A blocked stdout only grows the queue; past [`QUEUE_CAP`] bytes output is
/// dropped and counted instead, and the count is reported on stderr once the
/// queue drains. A stdout write error (the reader went away) ends chat
/// (exit 1), as a failed `print!` did; the owned session ends with it.
mod pump {
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender};
    use std::sync::OnceLock;

    /// Bytes queued (not yet written) before output is dropped.
    const QUEUE_CAP: usize = 64 << 20;

    enum Chunk {
        Out(String),
        Err(String),
        Barrier(SyncSender<()>),
    }

    struct Pump {
        tx: Sender<Chunk>,
        queued: AtomicUsize,
        dropped: AtomicUsize,
    }

    static PUMP: OnceLock<Pump> = OnceLock::new();

    fn pump() -> &'static Pump {
        PUMP.get_or_init(|| {
            let (tx, rx) = channel();
            std::thread::Builder::new()
                .name("chat-output".into())
                .spawn(move || writer(rx))
                .expect("spawn the chat output thread");
            Pump { tx, queued: AtomicUsize::new(0), dropped: AtomicUsize::new(0) }
        })
    }

    fn push(chunk: Chunk, len: usize) {
        let p = pump();
        if p.queued.load(Ordering::SeqCst) + len > QUEUE_CAP {
            p.dropped.fetch_add(len, Ordering::SeqCst);
            return;
        }
        p.queued.fetch_add(len, Ordering::SeqCst);
        let _ = p.tx.send(chunk);
    }

    pub(super) fn out(s: String) {
        if !s.is_empty() {
            let n = s.len();
            push(Chunk::Out(s), n);
        }
    }

    pub(super) fn err(s: String) {
        if !s.is_empty() {
            let n = s.len();
            push(Chunk::Err(s), n);
        }
    }

    /// Wait until everything queued so far is written.
    pub(super) fn drain() {
        let Some(p) = PUMP.get() else { return };
        let (done_tx, done_rx) = sync_channel(1);
        if p.tx.send(Chunk::Barrier(done_tx)).is_ok() {
            let _ = done_rx.recv();
        }
    }

    /// `std::process::exit` after the queued output is written.
    pub(super) fn exit(code: i32) -> ! {
        drain();
        std::process::exit(code)
    }

    fn writer(rx: Receiver<Chunk>) {
        for chunk in rx {
            let p = PUMP.get().expect("the pump is set before its first chunk");
            match chunk {
                Chunk::Out(s) => {
                    let mut o = std::io::stdout().lock();
                    let r = o.write_all(s.as_bytes()).and_then(|()| o.flush());
                    drop(o);
                    p.queued.fetch_sub(s.len(), Ordering::SeqCst);
                    if let Err(e) = r {
                        let _ = writeln!(std::io::stderr(), "synaps chat: stdout: {e}");
                        std::process::exit(1);
                    }
                }
                Chunk::Err(s) => {
                    let mut e = std::io::stderr().lock();
                    let _ = e.write_all(s.as_bytes()).and_then(|()| e.flush());
                    drop(e);
                    p.queued.fetch_sub(s.len(), Ordering::SeqCst);
                }
                Chunk::Barrier(done) => {
                    let _ = done.send(());
                }
            }
            if p.queued.load(Ordering::SeqCst) == 0 {
                let dropped = p.dropped.swap(0, Ordering::SeqCst);
                if dropped > 0 {
                    let _ = writeln!(
                        std::io::stderr(),
                        "\x1b[2m[synaps chat: {dropped} bytes of output dropped while the output was blocked]\x1b[0m"
                    );
                }
            }
        }
    }
}
