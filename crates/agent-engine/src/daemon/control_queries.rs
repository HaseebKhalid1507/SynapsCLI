//! Control queries answered without a session (and on an attached
//! connection): they read daemon-held state only and never wait on a
//! session actor or a turn.

use crate::session::children;
use crate::session::wire::{ChildListReply, DaemonFrame};
use crate::session::SessionId;

/// Feature name for the `children` query (and the `tool_child` events).
pub const FEATURE_CHILDREN: &str = "children";

/// What this daemon lists in `Welcome.features`.
pub fn daemon_features() -> Vec<String> {
    let mut features = Vec::new();
    if children::supported() {
        features.push(FEATURE_CHILDREN.to_string());
    }
    features
}

/// Answer `Children`. With `members`, the `/proc` walk runs on a blocking
/// thread. Any query may also start a lazy sweep (at most once per minute).
pub async fn children_reply(
    id: Option<u64>,
    session_id: Option<SessionId>,
    sid: Option<u32>,
    members: bool,
) -> DaemonFrame {
    if !children::supported() {
        return DaemonFrame::ChildList(ChildListReply {
            id,
            supported: false,
            boot_id: None,
            clk_tck: children::clk_tck(),
            sessions: Vec::new(),
            host: Vec::new(),
        });
    }
    children::maybe_sweep();
    let mut listing = children::list(session_id.as_ref().map(SessionId::as_str), sid);
    if members && !listing.sessions.is_empty() {
        if let Ok(scan) = tokio::task::spawn_blocking(children::scan_proc).await {
            children::attach_members(&mut listing, &scan);
        }
    }
    DaemonFrame::ChildList(ChildListReply {
        id,
        supported: true,
        boot_id: children::boot_id().map(str::to_string),
        clk_tck: children::clk_tck(),
        sessions: listing.sessions,
        host: listing.host,
    })
}
