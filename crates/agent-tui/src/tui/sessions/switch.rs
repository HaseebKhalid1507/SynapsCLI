//! Switching this TUI to another daemon session from the `/sessions` modal.
//!
//! A tab's Enter leaves a [`SwitchTarget`] on `SessionsState::pending_switch`;
//! the run loop drains it into [`perform`]. The new attach happens FIRST on a
//! fresh connection ([`Connector`]); only once it succeeded is the old link
//! swapped out and detached — the old session keeps running on the daemon
//! (its turn too, mid-stream). A failed attach leaves everything as it was,
//! with the reason inline in the modal.
//!
//! After the swap: per-session UI state is reset
//! ([`App::reset_for_session_switch`]), the prompt bridge is replaced, the
//! screen is rebuilt from the attach snapshot (`run_setup::remirror`, the
//! reconnect path), a mid-turn replay + pending prompts are fed like
//! `--attach` does, the modal closes and one line says what happened.

use super::{Notice, SwitchTarget};
use crate::tui::app::{App, ChatMessage};
use crate::tui::session_link::{PromptBridge, SessionLink};
use agent_engine::session::wire::{Attach, Hello};
use agent_engine::session::{
    AttachMode, AttachSnapshot, ClientKind, ClientTransport, CompactionPolicyWire, HistoryMode,
    SessionCommand, SessionConfig, SessionId,
};
use std::path::PathBuf;

/// A successful attach for a switch.
pub(crate) struct Attached {
    pub(crate) transport: Box<dyn ClientTransport>,
    pub(crate) snapshot: AttachSnapshot,
    /// `Continue` found its journal already live and attached to that.
    pub(crate) reused_live: bool,
}

/// Opens the new session. Production: [`DaemonConnector`]; the headless
/// harness plugs a scripted one.
#[async_trait::async_trait]
pub(crate) trait Connector: Send + Sync {
    async fn connect(&self, target: &SwitchTarget, mode: AttachMode) -> Result<Attached, String>;
}

/// Attaches over the daemon socket this TUI already talks to.
pub(crate) struct DaemonConnector {
    pub(crate) path: PathBuf,
}

#[async_trait::async_trait]
impl Connector for DaemonConnector {
    async fn connect(&self, target: &SwitchTarget, mode: AttachMode) -> Result<Attached, String> {
        use agent_engine::session::socket_transport::SocketTransport;
        let hello = Hello::new(ClientKind::Tui)
            .with_history(HistoryMode::from_env_or(HistoryMode::attach_client_default()));
        let conn = SocketTransport::connect(&self.path, hello)
            .await
            .map_err(|e| format!("daemon unreachable: {e}"))?;
        let (attach, reused_live) = match target {
            SwitchTarget::Attach { id } => {
                (Attach::Existing { session_id: SessionId(id.clone()), mode }, false)
            }
            SwitchTarget::Continue { journal_id } => {
                // Already live (someone continued it meanwhile)? Attach to
                // that one rather than forking a second session on one journal.
                match conn
                    .welcome
                    .sessions
                    .iter()
                    .find(|m| m.journal_id == *journal_id || m.id.0 == *journal_id)
                {
                    Some(m) => (Attach::Existing { session_id: m.id.clone(), mode }, true),
                    None => (continue_attach(journal_id, mode), false),
                }
            }
        };
        let (transport, snapshot) = SocketTransport::attach(conn, attach)
            .await
            .map_err(|e| match e {
                agent_engine::session::transport::TransportError::Refused(why) => why,
                other => other.to_string(),
            })?;
        Ok(Attached { transport: Box::new(transport), snapshot, reused_live })
    }
}

/// `synaps --attach --continue <id>`'s Create, from this TUI's cwd.
fn continue_attach(journal_id: &str, mode: AttachMode) -> Attach {
    Attach::Create {
        config: SessionConfig {
            continue_session: Some(Some(journal_id.to_string())),
            cwd: std::env::current_dir().ok(),
            compaction_policy: CompactionPolicyWire::LinkedSuccessor,
            await_extensions: !synaps_cli::load_config().startup.quick_start,
            ..Default::default()
        },
        mode,
    }
}

/// `kite (08e3)`, or `…08e3` for an unnamed session.
pub(crate) fn display_name(name: Option<&str>, id: &str) -> String {
    let short = super::short_id(id);
    match name.filter(|n| !n.is_empty()) {
        Some(n) => format!("{n} ({short})"),
        None => format!("\u{2026}{short}"),
    }
}

fn refuse(app: &mut App, text: impl Into<String>) {
    if let Some(st) = app.sessions.as_mut() {
        st.notice = Some(Notice { text: text.into(), error: true });
    } else {
        app.push_msg(ChatMessage::Error(text.into()));
    }
    app.request_redraw();
}

/// Run one switch. `connector = None` means an in-process TUI (no daemon).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn perform(
    app: &mut App,
    link: &mut SessionLink,
    bridge: &mut PromptBridge,
    target: SwitchTarget,
    connector: Option<&dyn Connector>,
    registry: &std::sync::Arc<synaps_cli::skills::registry::CommandRegistry>,
    render_handle: &crate::tui::render_thread::RenderHandle,
) {
    let own_id = link.transport().session_id().0.clone();
    if let SwitchTarget::Attach { id } = &target {
        if *id == own_id {
            refuse(app, "already on this session");
            return;
        }
    }
    let Some(connector) = connector else {
        return in_process(app, link, target).await;
    };

    let mode = if link.mode() == AttachMode::Observe { AttachMode::Observe } else { AttachMode::Mirror };
    if let Some(st) = app.sessions.as_mut() {
        st.notice = Some(Notice { text: "switching\u{2026}".into(), error: false });
    }
    let attached = match connector.connect(&target, mode).await {
        Ok(a) => a,
        Err(e) => {
            refuse(app, format!("switch failed: {e}"));
            return;
        }
    };
    if attached.snapshot.meta.id.0 == own_id {
        // Continue raced onto the session we are already on.
        refuse(app, "already on this session");
        return;
    }

    // ── the new attach worked: swap, then let the old one go ──
    let old_name = display_name(app.session.name.as_deref(), &own_id);
    let new_link = SessionLink::new(attached.transport);
    let old = std::mem::replace(link, new_link);
    let _ = old.send(SessionCommand::Detach { client: old.client_id() }).await;
    drop(old);

    let snapshot = attached.snapshot;
    app.reset_for_session_switch();
    *bridge = bridge.fresh();
    crate::tui::run_setup::remirror(app, &snapshot);
    app.observing = link.mode() == AttachMode::Observe;
    if let Some(cwd) = snapshot.meta.cwd.as_deref() {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        app.header_folder = crate::tui::header::folder_label(cwd, home.as_deref());
    }
    if snapshot.meta.name.is_some() {
        app.session.name = snapshot.meta.name.clone();
    }
    super::close(app);
    crate::tui::input::reconcile_secret_prompt(app);
    let new_name = display_name(snapshot.meta.name.as_deref(), &snapshot.meta.id.0);
    let via = if attached.reused_live { " (already live)" } else { "" };
    app.push_msg(ChatMessage::System(format!(
        "switched to {new_name}{via} \u{2014} {old_name} keeps running"
    )));
    if let Some(n) = snapshot.input_owned_elsewhere(link.client_id()) {
        app.push_msg(ChatMessage::System(n));
    }

    // Mid-turn attach: rebuild the partial turn, then the pending prompts
    // (same order as `--attach`).
    bridge.set_owns_input(snapshot.input_owner == Some(link.client_id()));
    for env in snapshot.replay.clone() {
        let _ = crate::tui::stream_handler::handle_session_event_arm(
            env, app, link, registry, render_handle, bridge, None,
        )
        .await;
    }
    for pr in snapshot.pending_prompts.clone() {
        bridge.on_prompt(pr);
    }
    crate::tui::stream_handler::sync_waiting_confirms(app, bridge);
    #[cfg(debug_assertions)]
    crate::tui::focus::debug_assert_stack_sync(app);
    app.request_immediate_redraw();
}

/// No daemon: nothing to attach to. Past → Continue falls back to today's
/// `/resume` (an actor swap in this process) — only when idle, since the
/// swap would cancel a running turn.
async fn in_process(app: &mut App, link: &mut SessionLink, target: SwitchTarget) {
    match target {
        SwitchTarget::Attach { .. } => {
            refuse(app, "no daemon \u{2014} switching sessions needs `synaps --attach`");
        }
        SwitchTarget::Continue { .. } if app.streaming || app.compacting => {
            refuse(app, "switching would cancel this turn \u{2014} wait for it to end (or Esc)");
        }
        SwitchTarget::Continue { journal_id } => match link.resume(&journal_id).await {
            Ok(reply) => {
                super::close(app);
                app.transcript.clear();
                app.invalidate();
                app.resume_pending = Some(crate::tui::app::ResumePending {
                    old_id: reply.old_id,
                    new_id: reply.new_id,
                    via: reply.via,
                    clamp_notice: reply.clamp_notice,
                });
            }
            Err(e) => refuse(app, format!("continue failed: {e}")),
        },
    }
}
