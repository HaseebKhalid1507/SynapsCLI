//! `synaps attach [ID] [--create] [--continue X]` — thin line client over
//! `SocketTransport` (PLAN-phase2 §2.11, B5).
//!
//! stdin lines → `Submit` (or `Steer` while streaming); `/abort` → `Cancel`;
//! `/detach` or Ctrl-C → `Detach` (the turn keeps running); `/sessions`;
//! prompts render as `[prompt #id] title: prompt > ` (echo off for Secret).

use std::io::Write;
use std::path::PathBuf;

use agent_engine::daemon::{registry, EXIT_REFUSED, EXIT_VERSION};
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{LlmEvent, SessionEvent, StreamEvent, TurnOutcome};
use clap::Args;

#[derive(Args, Debug, Clone, Default)]
pub(crate) struct AttachArgs {
    /// Session id to attach to (omit with --create).
    pub id: Option<String>,
    /// Create a new session in the daemon (cwd = this process's cwd).
    #[arg(long, alias = "new")]
    pub create: bool,
    /// Create by continuing a saved session (name or id).
    #[arg(long = "continue", value_name = "NAME_OR_ID")]
    pub continue_session: Option<String>,
    /// System prompt for a created session.
    #[arg(long = "system", short = 's')]
    pub system: Option<String>,
    /// Read-only mirror: never owns input (B1).
    #[arg(long, conflicts_with = "takeover")]
    pub observe: bool,
    /// Steal input ownership from the current owner (B1).
    #[arg(long)]
    pub takeover: bool,
    /// Never park this session (B3); also sent to an existing session.
    #[arg(long = "keep-warm")]
    pub keep_warm: bool,
}

impl AttachArgs {
    /// `--observe` → `Observe`, `--takeover` → `Takeover`, else `Mirror`
    /// (owner iff nobody owns input yet).
    pub(crate) fn attach_mode(&self) -> AttachMode {
        if self.observe {
            AttachMode::Observe
        } else if self.takeover {
            AttachMode::Takeover
        } else {
            AttachMode::Mirror
        }
    }
}

struct Client {
    t: SocketTransport,
    streaming: bool,
    pending: Vec<PromptRequest>,
    stdout: std::io::Stdout,
}

impl Client {
    fn out(&mut self, s: &str) {
        let _ = self.stdout.write_all(s.as_bytes());
        let _ = self.stdout.flush();
    }

    fn render(&mut self, env: &Envelope) {
        match &env.event {
            SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::Text(t))) => self.out(t),
            SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::Thinking(_))) => {}
            SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::ToolUse { tool_name, .. })) => {
                self.out(&format!("\n[tool: {tool_name}]\n"))
            }
            SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::ToolResult { result, .. })) => {
                let short: String = result.chars().take(400).collect();
                self.out(&format!("[result] {short}\n"))
            }
            SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Done)) => {
                self.streaming = false;
                self.out("\n");
            }
            SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Error(e))) => {
                self.streaming = false;
                let label = if matches!(e.outcome, TurnOutcome::Canceled) { "canceled" } else { "error" };
                self.out(&format!("\n[{label}] {} ({})\n", e.message, e.category_label()));
            }
            SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Notice(n))) => self.out(&format!("[notice] {n}\n")),
            SessionEventWire::Stream(StreamEvent::Agent(a)) => self.out(&format!("[agent] {a:?}\n")),
            SessionEventWire::Stream(_) => {}
            SessionEventWire::TurnStarted { .. } => self.streaming = true,
            SessionEventWire::Prompt(p) => {
                self.pending.push(p.clone());
                self.out(&format!("[prompt #{}] {}: {} > ", p.id, p.title, p.prompt));
            }
            SessionEventWire::PromptResolved { prompt_id } => self.pending.retain(|p| p.id != *prompt_id),
            SessionEventWire::SystemNotice(s) => self.out(&format!("[system] {s}\n")),
            SessionEventWire::Steered { text, delivered } => {
                self.out(&format!("{} {text}\n", if *delivered { "→ steering:" } else { "queued:" }))
            }
            SessionEventWire::Dequeued { text } => self.out(&format!("[dequeued] {text}\n")),
            SessionEventWire::AutoTurnCapReached { cap } => self.out(&format!("[auto-turn cap {cap} reached]\n")),
            SessionEventWire::Idle => {}
            SessionEventWire::External(e) => self.out(&format!("[event {}] {}\n", e.source.name, e.content.text)),
            SessionEventWire::ClientJoined { client, kind } => self.out(&format!("[client #{} joined ({kind:?})]\n", client.0)),
            SessionEventWire::ClientLeft { client } => self.out(&format!("[client #{} left]\n", client.0)),
            SessionEventWire::Ended { reason } => self.out(&format!("[session ended: {reason:?}]\n")),
            SessionEventWire::SettingChanged(a) => {
                self.out(&format!("[{}: {}]\n", a.setting, if a.ok { "ok" } else { a.message.as_deref().unwrap_or("failed") }))
            }
            SessionEventWire::QueryResult { value, .. } => self.out(&format!("{value}\n")),
            SessionEventWire::Conversation(_)
            | SessionEventWire::LoaderProgress(_)
            | SessionEventWire::ExtensionNotification { .. }
            | SessionEventWire::Attached { .. } => {}
            SessionEventWire::Aborted { context_saved } => {
                self.streaming = false;
                self.out(if *context_saved { "[aborted — context saved for next message]\n" } else { "[aborted]\n" })
            }
            SessionEventWire::Cleared { session_id } => {
                self.out(&format!("[session cleared → {}]\n", &session_id[..8.min(session_id.len())]))
            }
            SessionEventWire::CompactionStarted { disclosure, .. } => self.out(&format!("[compacting: {disclosure}]\n")),
            SessionEventWire::CompactionApplied { msg_count, .. } => self.out(&format!("[compacted {msg_count} messages]\n")),
            SessionEventWire::CompactionFailed { message, .. } => self.out(&format!("[compaction failed: {message}]\n")),
            SessionEventWire::CompactionCancelled => self.out("[compaction cancelled]\n"),
            SessionEventWire::InputOwnerChanged { to, reason, .. } => {
                self.out(&format!("[input owner → {} ({reason:?})]\n", to.map(|c| format!("client #{}", c.0)).unwrap_or_else(|| "nobody".into())))
            }
            SessionEventWire::Refused { command, reason, .. } => self.out(&format!("[refused {command}: {reason}]\n")),
            SessionEventWire::AttachRefused { message } => self.out(&format!("[attach refused: {message}]\n")),
            SessionEventWire::Lifecycle(l) => self.out(&format!("[session {l:?}]\n")),
            SessionEventWire::Reloading { generation, .. } => self.out(&format!("[daemon reloading → generation {generation}]\n")),
            SessionEventWire::SubagentRows(_) | SessionEventWire::Resumed { .. } => {}
        }
    }

    /// stdin line → command. Returns `false` to detach.
    async fn line(&mut self, line: &str) -> bool {
        let line = line.trim_end_matches(['\n', '\r']);
        if let Some(p) = self.pending.first().cloned() {
            let value = if line.is_empty() { None } else { Some(line.to_string()) };
            let _ = self.t.send(SessionCommand::Answer { prompt_id: p.id, value }).await;
            self.pending.remove(0);
            if p.kind == PromptKind::Secret {
                set_echo(true);
                self.out("\n");
            }
            return true;
        }
        match line {
            "" => return true,
            "/detach" | "/quit" | "/exit" => return false,
            "/abort" => {
                let _ = self.t.send(SessionCommand::Cancel).await;
            }
            "/sessions" => {
                let _ = self.t.send(SessionCommand::Query { id: 1, query: SessionQuery::Status }).await;
            }
            "/save" => {
                let _ = self.t.send(SessionCommand::Save).await;
            }
            "/new" => {
                let _ = self.t.send(SessionCommand::NewSession).await;
            }
            "/help" => self.out("/detach /abort /save /new /sessions /model NAME /cmd NAME [ARG] /keep-warm on|off\n"),
            "/keep-warm on" | "/keep-warm off" => {
                let _ = self.t.send(SessionCommand::KeepWarm { on: line.ends_with("on") }).await;
            }
            _ if line.starts_with("/cmd ") => {
                let rest = line["/cmd ".len()..].trim();
                let (name, arg) = rest.split_once(' ').unwrap_or((rest, ""));
                let _ = self
                    .t
                    .send(SessionCommand::EngineCommand { id: 2, name: name.to_string(), arg: arg.trim().to_string() })
                    .await;
            }
            _ if line.starts_with("/model ") => {
                let model = line["/model ".len()..].trim().to_string();
                let _ = self.t.send(SessionCommand::Set { id: 0, setting: SessionSetting::Model { model } }).await;
            }
            text => {
                let cmd = if self.streaming {
                    SessionCommand::Steer { text: text.to_string() }
                } else {
                    SessionCommand::Submit { text: text.to_string(), attachments: vec![] }
                };
                match self.t.send(cmd).await {
                    Ok(()) => {}
                    Err(TransportError::Backpressure) => self.out("[backpressure: try again]\n"),
                    Err(e) => self.out(&format!("[send failed: {e}]\n")),
                }
            }
        }
        true
    }
}

fn set_echo(on: bool) {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("stty")
            .arg(if on { "echo" } else { "-echo" })
            .stdin(std::process::Stdio::inherit())
            .status();
    }
}

pub(crate) async fn run(profile: Option<String>, args: AttachArgs) -> anyhow::Result<()> {
    if let Err(code) = super::daemon::require_enabled("synaps attach") {
        std::process::exit(code);
    }
    let paths = registry::daemon_paths(profile.as_deref());
    // `main` already auto-spawned (or exited 3); this is the safety net.
    if !registry::is_alive(&paths) {
        eprintln!("{}", no_daemon_message(profile.as_deref()));
        std::process::exit(EXIT_REFUSED);
    }
    let conn = match SocketTransport::connect(&paths.sock, Hello::new(ClientKind::Attach)).await {
        Ok(c) => c,
        Err(TransportError::Version { client, daemon }) => {
            eprintln!("synaps attach: protocol version mismatch (client {client}, daemon {daemon}); restart the daemon with this binary");
            std::process::exit(EXIT_VERSION);
        }
        Err(e) => anyhow::bail!("connect: {e}"),
    };
    if conn.welcome.daemon_version != binary_version() {
        eprintln!("[notice] daemon binary {} differs from client {} (same protocol)", conn.welcome.daemon_version, binary_version());
    }

    let mode = args.attach_mode();
    let attach = if args.create || args.continue_session.is_some() {
        Attach::Create {
            config: SessionConfig {
                continue_session: args.continue_session.clone().map(Some),
                system: args.system.clone(),
                cwd: Some(std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))),
                keep_warm: args.keep_warm,
                ..Default::default()
            },
            mode,
        }
    } else if let Some(id) = &args.id {
        let id = resolve_id(&conn.welcome.sessions, id).unwrap_or_else(|| SessionId::from(id.as_str()));
        Attach::Existing { session_id: id, mode }
    } else if conn.welcome.sessions.len() == 1 {
        Attach::Existing { session_id: conn.welcome.sessions[0].id.clone(), mode }
    } else if conn.welcome.sessions.is_empty() {
        Attach::Create {
            config: SessionConfig { cwd: std::env::current_dir().ok(), keep_warm: args.keep_warm, ..Default::default() },
            mode,
        }
    } else {
        eprintln!("several sessions; pick one:");
        for m in &conn.welcome.sessions {
            eprintln!("  {}  model={}", m.id, m.model);
        }
        std::process::exit(1);
    };

    let existing = matches!(attach, Attach::Existing { .. });
    let (t, snap) = SocketTransport::attach(conn, attach).await.map_err(|e| anyhow::anyhow!("attach: {e}"))?;
    let mut c = Client { t, streaming: snap.streaming, pending: snap.pending_prompts.clone(), stdout: std::io::stdout() };
    if args.keep_warm && existing {
        let _ = c.t.send(SessionCommand::KeepWarm { on: true }).await;
    }
    // "○ ready" is the marker scripts/memprof/launch.sh polls for.
    c.out(&format!(
        "[attached {} as client #{}  model={}  messages={}{}] ○ ready\n",
        snap.meta.id,
        c.t.client_id().0,
        snap.view.model,
        snap.conversation.api_messages.len(),
        if snap.streaming { "  streaming" } else { "" }
    ));
    for env in &snap.replay {
        c.render(env);
    }
    for p in &snap.pending_prompts {
        c.out(&format!("[prompt #{}] {}: {} > ", p.id, p.title, p.prompt));
    }

    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    loop {
        line.clear();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                c.out("\n[detaching; the session keeps running]\n");
                break;
            }
            r = tokio::io::AsyncBufReadExt::read_line(&mut stdin, &mut line) => {
                match r {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if !c.line(&line).await { break; }
                        if let Some(p) = c.pending.first() { if p.kind == PromptKind::Secret { set_echo(false); } }
                    }
                }
            }
            ev = c.t.next_event() => {
                match ev {
                    Some(env) => {
                        let ended = matches!(env.event, SessionEventWire::Ended { .. });
                        c.render(&env);
                        if let Some(p) = c.pending.first() { if p.kind == PromptKind::Secret { set_echo(false); } }
                        if ended { set_echo(true); return Ok(()); }
                    }
                    None => { c.out("[connection closed]\n"); set_echo(true); return Ok(()); }
                }
            }
        }
    }
    set_echo(true);
    c.t.detach().await;
    Ok(())
}

/// `SYNAPS_DAEMON_AUTOSPAWN=0` and nobody running: say how to start one.
pub(crate) fn no_daemon_message(profile: Option<&str>) -> String {
    let start = match profile {
        Some(p) => format!("synaps --profile {p} daemon --detach"),
        None => "synaps daemon --detach".to_string(),
    };
    format!("no daemon running — start it with `{start}` (or unset SYNAPS_DAEMON_AUTOSPAWN=0 to auto-start)")
}

fn resolve_id(sessions: &[SessionMeta], q: &str) -> Option<SessionId> {
    sessions
        .iter()
        .find(|m| m.id.as_str() == q || m.name.as_deref() == Some(q))
        .or_else(|| sessions.iter().find(|m| m.id.as_str().starts_with(q)))
        .map(|m| m.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: AttachArgs,
    }

    #[test]
    fn flags_plumb_to_attach_mode() {
        let a = Cli::parse_from(["attach"]).args;
        assert_eq!(a.attach_mode(), AttachMode::Mirror);
        assert!(!a.keep_warm);
        let a = Cli::parse_from(["attach", "--observe"]).args;
        assert_eq!(a.attach_mode(), AttachMode::Observe);
        let a = Cli::parse_from(["attach", "abc", "--takeover", "--keep-warm"]).args;
        assert_eq!(a.attach_mode(), AttachMode::Takeover);
        assert!(a.keep_warm);
        assert_eq!(a.id.as_deref(), Some("abc"));
        assert!(Cli::try_parse_from(["attach", "--observe", "--takeover"]).is_err());
    }
}
