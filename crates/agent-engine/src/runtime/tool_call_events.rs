//! Native tool-call events: one content-free record per phase of every tool
//! call, for the daemon's `ToolCalls` subscribers.
//!
//! - **Phases.** `called` (before any extension sees the call), `decided`
//!   (after the `before_tool_call` hook chain) and `finished` (after the
//!   tool ran, was blocked or was cancelled).
//! - **No content.** A record names the tool, the call's ids, the owning
//!   session, a fingerprint of the input ([`call_fingerprint`]) and the
//!   call's base directory (`called.cwd`), never the input, the output or
//!   command text. Its `Debug` is the same metadata.
//! - **Opt-in.** The hub is off until the daemon turns it on from
//!   `daemon.tool_call_events`. Off, or on with nobody subscribed, a call
//!   builds nothing ([`ToolCallEvents::is_active`] is false).
//! - **Fail-open.** A record goes into one `tokio::sync::broadcast` channel
//!   (capacity [`TOOL_CALL_HUB_CAPACITY`]) with a synchronous send that never
//!   awaits a client. A subscriber that falls behind skips ahead and learns
//!   the gap from `dropped` on its next delivery.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::extensions::hooks::events::ToolCallHookContext;

/// `v` of every record this build writes.
pub const TOOL_CALL_RECORD_VERSION: u32 = 1;
/// Records the broadcast channel holds for a subscriber that is behind.
pub const TOOL_CALL_HUB_CAPACITY: usize = 4096;
/// The config key that turns the events on (default off).
pub const TOOL_CALL_EVENTS_KEY: &str = "daemon.tool_call_events";

/// One phase of one tool call. Metadata only: never the call's input,
/// output or command text (the one path is `called.cwd`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallRecord {
    /// Record version ([`TOOL_CALL_RECORD_VERSION`]).
    pub v: u32,
    #[serde(flatten)]
    pub phase: ToolCallPhase,
    /// The owning conversation. A subagent's call carries its parent's
    /// session id (with `handle_id`); `null` when there is none (an
    /// extension's own `tool.call`, an in-process worker of no session).
    pub session_id: Option<String>,
    /// The subagent handle (`sa_N`) when a worker made the call; absent for
    /// a session's own calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle_id: Option<String>,
    /// The tool's API name (what the model called).
    pub tool: String,
    /// The tool's runtime name (e.g. an extension or MCP tool's full name);
    /// equal to `tool` for builtins.
    pub tool_runtime: String,
    /// The provider's call id (`tool_use.id`), the same id the children
    /// ledger and the attach stream's `tool_use` report.
    pub tool_id: String,
    /// When the phase happened: RFC 3339, milliseconds, UTC (`...Z`).
    pub ts: String,
}

/// What happened, per phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum ToolCallPhase {
    /// The model asked for the call; no extension has seen it yet. `fp` is
    /// over the model's input. `cwd` is the base directory the tool resolves
    /// relative paths against: the same value the call's hook event carries
    /// (`effective_tool_cwd`). Omitted when unknown or not UTF-8. A folder
    /// named in the tool's own input is not here; a client resolves it from
    /// the `tool_use` it already has.
    Called {
        fp: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    /// The `before_tool_call` chain decided. `by` names the extensions that
    /// decided it (host-stamped): the blocker, the confirmers, or the
    /// modifiers; empty for `allow`. `fp` is over the input the chain
    /// settled on: it differs from `called`'s iff the input was modified.
    Decided {
        decision: ToolCallDecision,
        by: Vec<String>,
        fp: String,
    },
    /// The call ended: it ran (`ok`/`error`), was blocked (by an extension or
    /// a denied confirmation) or was cancelled.
    Finished {
        status: ToolCallStatus,
        duration_ms: u64,
    },
}

/// The hook chain's decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallDecision {
    Allow,
    Confirm,
    Block,
    Modify,
}

/// How a call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Ok,
    Error,
    Blocked,
    Cancelled,
}

/// Who a worker's calls belong to, as the children ledger reports them: the
/// parent's session id and the worker's handle. Set on a worker's runtime by
/// the subagent spawn paths and read ONLY by these records; the worker's own
/// `session_id` (hooks, injection, everything else) stays `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallOwner {
    /// The parent's conversation id (`None` when the parent has none).
    pub parent_session_id: Option<String>,
    /// The worker's handle id (`sa_N`).
    pub handle_id: String,
}

impl ToolCallOwner {
    /// The owner of a worker started from a tool call in `ctx` as `handle_id`.
    pub fn for_worker(ctx: &crate::ToolContext, handle_id: &str) -> Self {
        Self {
            parent_session_id: ctx.capabilities.session_id.clone(),
            handle_id: handle_id.to_string(),
        }
    }
}

// ── fingerprint ──────────────────────────────────────────────────────────────
//
// Rewritten from pria-gate's `crates/pria-gate-ext/src/fingerprint.rs`
// (pria-apps): the same bytes and the same pinned vectors, so a client's
// pairing code carries over unchanged.

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Fingerprint of one call: `FNV-1a-64(tool_name ‖ 0x00 ‖ canonical JSON of
/// input)`, as 16 lowercase hex digits. `tool_name` is the API name. Same
/// algorithm and vectors as pria-gate's `call_fingerprint`. It links
/// records; it is not a secret and not a security boundary.
pub fn call_fingerprint(tool_name: &str, input: &Value) -> String {
    let mut canon = String::with_capacity(128);
    canonical_json(input, &mut canon);
    let mut h = FNV_OFFSET;
    for b in tool_name
        .as_bytes()
        .iter()
        .chain(std::iter::once(&0u8))
        .chain(canon.as_bytes())
    {
        h ^= u64::from(*b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    format!("{h:016x}")
}

/// Appends the canonical rendering of `v` to `out`: object keys sorted by
/// UTF-8 bytes, no whitespace, serde_json's string and number rendering
/// (independent of `serde_json/preserve_order`).
pub fn canonical_json(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => push_str_lit(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical_json(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_str_lit(k, out);
                out.push(':');
                if let Some(val) = map.get(k) {
                    canonical_json(val, out);
                }
            }
            out.push('}');
        }
    }
}

fn push_str_lit(s: &str, out: &mut String) {
    // serde_json's escaping; serializing a &str cannot fail.
    out.push_str(&serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string()));
}

// ── hub ──────────────────────────────────────────────────────────────────────

/// One broadcast item: the daemon-wide sequence number and the record.
pub type ToolCallItem = (u64, Arc<ToolCallRecord>);

/// The per-host broadcast point every dispatch site of that host (and its
/// workers) publishes to. Off until [`Self::set_enabled`]; the channel is
/// created on the first subscription.
pub struct ToolCallHub {
    enabled: AtomicBool,
    sender: OnceLock<broadcast::Sender<ToolCallItem>>,
    /// Last sequence number given out. Held across the (synchronous) send
    /// so the channel order is the `seq` order.
    seq: Mutex<u64>,
}

impl Default for ToolCallHub {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ToolCallHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCallHub")
            .field("enabled", &self.is_enabled())
            .field("subscribers", &self.subscribers())
            .finish()
    }
}

impl ToolCallHub {
    /// A hub that is off (nothing is recorded, a subscribe is refused).
    pub fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            sender: OnceLock::new(),
            seq: Mutex::new(0),
        }
    }

    /// Turn the events on or off (the daemon, from `daemon.tool_call_events`).
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::SeqCst);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// Live subscriptions.
    pub fn subscribers(&self) -> usize {
        self.sender.get().map_or(0, broadcast::Sender::receiver_count)
    }

    /// Whether a call made now would be recorded: on, and subscribed.
    pub fn is_active(&self) -> bool {
        self.is_enabled() && self.subscribers() > 0
    }

    /// A new subscription to every record published from now on; `None`
    /// while the hub is off.
    pub fn subscribe(&self) -> Option<ToolCallSubscription> {
        if !self.is_enabled() {
            return None;
        }
        let rx = self
            .sender
            .get_or_init(|| broadcast::channel(TOOL_CALL_HUB_CAPACITY).0)
            .subscribe();
        Some(ToolCallSubscription { rx, dropped: 0 })
    }

    /// Give `record` the next sequence number and send it. Synchronous:
    /// never waits for a subscriber (a full channel overwrites the oldest
    /// record, which its laggards then count as dropped).
    fn publish(&self, record: ToolCallRecord) {
        let Some(tx) = self.sender.get() else { return };
        if tx.receiver_count() == 0 {
            return;
        }
        let mut seq = self.seq.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *seq += 1;
        let _ = tx.send((*seq, Arc::new(record)));
    }
}

/// One subscriber's view of the hub.
#[derive(Debug)]
pub struct ToolCallSubscription {
    rx: broadcast::Receiver<ToolCallItem>,
    /// Records skipped since the last delivery.
    dropped: u64,
}

/// What a subscriber receives: the record, its daemon-wide `seq`, and how
/// many records this subscriber missed since its previous delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallDelivery {
    pub seq: u64,
    pub dropped: u64,
    pub record: Arc<ToolCallRecord>,
}

impl ToolCallSubscription {
    /// The next record; `None` once the hub is gone. A lag is folded into
    /// the next delivery's `dropped`.
    pub async fn next(&mut self) -> Option<ToolCallDelivery> {
        loop {
            match self.rx.recv().await {
                Ok((seq, record)) => {
                    let dropped = std::mem::take(&mut self.dropped);
                    return Some(ToolCallDelivery { seq, dropped, record });
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    self.dropped = self.dropped.saturating_add(n);
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }

    /// [`Self::next`] without waiting: `None` when nothing is queued.
    pub fn try_next(&mut self) -> Option<ToolCallDelivery> {
        loop {
            match self.rx.try_recv() {
                Ok((seq, record)) => {
                    let dropped = std::mem::take(&mut self.dropped);
                    return Some(ToolCallDelivery { seq, dropped, record });
                }
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    self.dropped = self.dropped.saturating_add(n);
                }
                Err(_) => return None,
            }
        }
    }
}

// ── per call ─────────────────────────────────────────────────────────────────

/// The records of one call, from `called` to `finished`. Built by
/// [`crate::runtime::emit_before_tool_call_recorded`] at every dispatch
/// site; inactive (no allocation, no fingerprint) when the hub is off or
/// nobody is subscribed. Dropped before [`Self::finish`], it records
/// `finished(cancelled)` (`error` if dropped by a panic): a cancelled turn
/// drops the dispatch future mid-call.
#[derive(Debug)]
pub struct ToolCallEvents {
    active: Option<ActiveCall>,
}

#[derive(Debug)]
struct ActiveCall {
    hub: Arc<ToolCallHub>,
    session_id: Option<String>,
    handle_id: Option<String>,
    tool: String,
    tool_runtime: String,
    tool_id: String,
    started: Instant,
    finished: bool,
}

impl ToolCallEvents {
    /// A recorder that records nothing.
    pub fn inactive() -> Self {
        Self { active: None }
    }

    /// Start recording a call and publish its `called` record (fp over the
    /// model's `input`). `owner` marks a worker's call (parent session id +
    /// handle); otherwise the record's session is `call.session_id`.
    pub fn begin(
        hub: &Arc<ToolCallHub>,
        tool: &str,
        tool_runtime: Option<&str>,
        call: &ToolCallHookContext<'_>,
        owner: Option<&ToolCallOwner>,
        input: &Value,
    ) -> Self {
        // Off, or nobody subscribed: build nothing, hash nothing.
        if !hub.is_active() {
            return Self::inactive();
        }
        let (session_id, handle_id) = match owner {
            Some(owner) => (owner.parent_session_id.clone(), Some(owner.handle_id.clone())),
            None => (call.session_id.map(str::to_string), None),
        };
        let active = ActiveCall {
            hub: Arc::clone(hub),
            session_id,
            handle_id,
            tool: tool.to_string(),
            tool_runtime: tool_runtime.unwrap_or(tool).to_string(),
            tool_id: call.tool_call_id.unwrap_or_default().to_string(),
            started: Instant::now(),
            finished: false,
        };
        let fp = call_fingerprint(tool, input);
        active.publish(ToolCallPhase::Called { fp, cwd: call.cwd.clone() });
        Self { active: Some(active) }
    }

    /// Whether this call is being recorded.
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// Publish `decided`, with the fp over the input the chain settled on.
    pub fn decided(&mut self, decision: ToolCallDecision, by: Vec<String>, final_input: &Value) {
        if let Some(active) = &self.active {
            let fp = call_fingerprint(&active.tool, final_input);
            active.publish(ToolCallPhase::Decided { decision, by, fp });
        }
    }

    /// Publish `finished`. Later calls (and the drop) record nothing more.
    pub fn finish(&mut self, status: ToolCallStatus) {
        if let Some(active) = &mut self.active {
            active.finish(status);
        }
    }
}

impl ActiveCall {
    fn publish(&self, phase: ToolCallPhase) {
        self.hub.publish(ToolCallRecord {
            v: TOOL_CALL_RECORD_VERSION,
            phase,
            session_id: self.session_id.clone(),
            handle_id: self.handle_id.clone(),
            tool: self.tool.clone(),
            tool_runtime: self.tool_runtime.clone(),
            tool_id: self.tool_id.clone(),
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        });
    }

    fn finish(&mut self, status: ToolCallStatus) {
        if self.finished {
            return;
        }
        self.finished = true;
        let duration_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.publish(ToolCallPhase::Finished { status, duration_ms });
    }
}

impl Drop for ToolCallEvents {
    fn drop(&mut self) {
        if let Some(active) = &mut self.active {
            let status = if std::thread::panicking() {
                ToolCallStatus::Error
            } else {
                ToolCallStatus::Cancelled
            };
            active.finish(status);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::hooks::events::{HookEvent, HookKind, HookResult};
    use crate::extensions::hooks::HookBus;
    use crate::extensions::permissions::{Permission, PermissionSet};
    use serde_json::json;

    /// A secret-shaped string no record may ever carry.
    const SECRET: &str = "sk-ant-api03-TOOLCALLSECRET0123456789abcdefABCDEF";

    fn on_hub() -> Arc<ToolCallHub> {
        let hub = Arc::new(ToolCallHub::new());
        hub.set_enabled(true);
        hub
    }

    fn call<'a>(session: Option<&'a str>, id: &'a str) -> ToolCallHookContext<'a> {
        ToolCallHookContext { session_id: session, tool_call_id: Some(id), cwd: None }
    }

    fn drain(sub: &mut ToolCallSubscription) -> Vec<ToolCallDelivery> {
        std::iter::from_fn(|| sub.try_next()).collect()
    }

    fn phase_name(r: &ToolCallRecord) -> &'static str {
        match r.phase {
            ToolCallPhase::Called { .. } => "called",
            ToolCallPhase::Decided { .. } => "decided",
            ToolCallPhase::Finished { .. } => "finished",
        }
    }

    fn fp_of(r: &ToolCallRecord) -> &str {
        match &r.phase {
            ToolCallPhase::Called { fp, .. } | ToolCallPhase::Decided { fp, .. } => fp,
            ToolCallPhase::Finished { .. } => "",
        }
    }

    // ── group 8: the fingerprint is pria-gate's ──

    #[test]
    fn fp_pinned_vectors_are_pria_gates() {
        // Same vectors as pria-gate's `known_vector_is_stable`.
        assert_eq!(call_fingerprint("bash", &json!({"command": "ls"})), "664c8869d8c4b4dd");
        assert_eq!(call_fingerprint("", &Value::Null), "b4d24b59678288f2");
    }

    #[test]
    fn fp_ignores_key_order_and_whitespace() {
        let a: Value = serde_json::from_str(r#"{"b": 1, "a": {"y": [1, 2], "x": "é\n"}}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"a":{"x":"é\n","y":[1,2]},"b":1}"#).unwrap();
        assert_eq!(call_fingerprint("bash", &a), call_fingerprint("bash", &b));
        let mut s = String::new();
        canonical_json(&a, &mut s);
        assert_eq!(s, r#"{"a":{"x":"é\n","y":[1,2]},"b":1}"#);
    }

    #[test]
    fn fp_tool_name_and_values_matter() {
        let input = json!({"command": "ls"});
        let fp = call_fingerprint("bash", &input);
        assert_eq!(fp.len(), 16);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(fp, call_fingerprint("sh", &input));
        assert_ne!(fp, call_fingerprint("bash", &json!({"command": "ls "})));
        // The 0x00 separator keeps name/input boundaries apart.
        assert_ne!(call_fingerprint("a", &json!("bc")), call_fingerprint("ab", &json!("c")));
    }

    // ── group 1: off means nothing ──

    #[test]
    fn a_hub_that_is_off_records_nothing_and_refuses_a_subscribe() {
        let hub = Arc::new(ToolCallHub::new());
        assert!(!hub.is_enabled(), "off by default");
        assert!(hub.subscribe().is_none(), "a subscribe is refused while off");
        let mut events = ToolCallEvents::begin(&hub, "bash", None, &call(Some("s"), "t1"), None, &json!({}));
        assert!(!events.is_active(), "off: nothing is built");
        events.decided(ToolCallDecision::Allow, vec![], &json!({}));
        events.finish(ToolCallStatus::Ok);
        // Turning it on later starts from a clean sequence.
        hub.set_enabled(true);
        let mut sub = hub.subscribe().expect("on: subscribe works");
        drop(ToolCallEvents::begin(&hub, "bash", None, &call(Some("s"), "t2"), None, &json!({})));
        let got = drain(&mut sub);
        assert_eq!(got.first().map(|d| d.seq), Some(1), "{got:?}");
        assert!(got.iter().all(|d| d.record.tool_id == "t2"));
    }

    // ── groups 2 and 4 (unit): the three phases ──

    #[test]
    fn one_call_records_called_decided_finished_in_seq_order() {
        let hub = on_hub();
        let mut sub = hub.subscribe().unwrap();
        let input = json!({"command": "ls"});
        let hook_call = ToolCallHookContext { cwd: Some("/srv/project".into()), ..call(Some("conv-1"), "toolu_1") };
        let mut events = ToolCallEvents::begin(&hub, "bash", Some("bash"), &hook_call, None, &input);
        assert!(events.is_active());
        events.decided(ToolCallDecision::Allow, vec![], &input);
        events.finish(ToolCallStatus::Ok);
        events.finish(ToolCallStatus::Error);
        drop(events);
        let got = drain(&mut sub);
        let phases: Vec<_> = got.iter().map(|d| phase_name(&d.record)).collect();
        assert_eq!(phases, ["called", "decided", "finished"], "one finished only");
        assert!(got.windows(2).all(|w| w[1].seq == w[0].seq + 1), "{got:?}");
        assert!(got.iter().all(|d| d.dropped == 0));
        for d in &got {
            let r = &d.record;
            assert_eq!((r.v, r.tool.as_str(), r.tool_runtime.as_str(), r.tool_id.as_str()), (1, "bash", "bash", "toolu_1"));
            assert_eq!(r.session_id.as_deref(), Some("conv-1"));
            assert_eq!(r.handle_id, None);
            // RFC 3339, milliseconds, UTC: 2026-10-24T18:00:00.123Z
            assert_eq!(r.ts.len(), 24, "{}", r.ts);
            assert!(r.ts.ends_with('Z') && r.ts.as_bytes()[19] == b'.', "{}", r.ts);
        }
        assert_eq!(fp_of(&got[0].record), "664c8869d8c4b4dd");
        assert_eq!(fp_of(&got[0].record), fp_of(&got[1].record), "unmodified: same fp");
        assert!(matches!(
            got[1].record.phase,
            ToolCallPhase::Decided { decision: ToolCallDecision::Allow, ref by, .. } if by.is_empty()
        ));
        assert!(matches!(got[2].record.phase, ToolCallPhase::Finished { status: ToolCallStatus::Ok, .. }));
        // `cwd` is the hook event's, on `called` only.
        assert!(
            matches!(&got[0].record.phase, ToolCallPhase::Called { cwd: Some(c), .. } if c == "/srv/project"),
            "{:?}",
            got[0].record
        );
        for d in &got[1..] {
            let json = serde_json::to_value(&*d.record).unwrap();
            assert!(json.get("cwd").is_none(), "cwd is on called only: {json}");
        }
    }

    #[test]
    fn a_recorder_dropped_before_it_finished_records_cancelled() {
        let hub = on_hub();
        let mut sub = hub.subscribe().unwrap();
        let mut events = ToolCallEvents::begin(&hub, "bash", None, &call(None, "toolu_x"), None, &json!({}));
        events.decided(ToolCallDecision::Allow, vec![], &json!({}));
        drop(events);
        let got = drain(&mut sub);
        assert_eq!(got.len(), 3, "{got:?}");
        assert!(matches!(
            got[2].record.phase,
            ToolCallPhase::Finished { status: ToolCallStatus::Cancelled, .. }
        ));
        assert_eq!(got[2].record.session_id, None, "no conversation: null");
    }

    // ── group 7 (unit): a worker's call ──

    #[test]
    fn a_workers_call_carries_its_parents_session_and_its_handle() {
        let hub = on_hub();
        let mut sub = hub.subscribe().unwrap();
        let owner = ToolCallOwner { parent_session_id: Some("conv-parent".into()), handle_id: "sa_3".into() };
        // A worker's own session id is None: the owner supplies the parent's.
        let mut events = ToolCallEvents::begin(&hub, "read", None, &call(None, "toolu_w"), Some(&owner), &json!({}));
        events.finish(ToolCallStatus::Ok);
        let mut own = ToolCallEvents::begin(&hub, "read", None, &call(Some("conv-parent"), "toolu_p"), None, &json!({}));
        own.finish(ToolCallStatus::Ok);
        let got = drain(&mut sub);
        let worker: Vec<_> = got.iter().filter(|d| d.record.tool_id == "toolu_w").collect();
        assert_eq!(worker.len(), 2);
        for d in worker {
            assert_eq!(d.record.session_id.as_deref(), Some("conv-parent"));
            assert_eq!(d.record.handle_id.as_deref(), Some("sa_3"));
        }
        let parent = got.iter().find(|d| d.record.tool_id == "toolu_p").unwrap();
        assert_eq!(parent.record.handle_id, None);
        let json = serde_json::to_value(&*parent.record).unwrap();
        assert!(json.get("handle_id").is_none(), "a session's own call omits handle_id: {json}");
    }

    // ── group 9 (unit): the record's wire shape ──

    #[test]
    fn records_serialize_to_the_documented_json() {
        let base = |phase| ToolCallRecord {
            v: 1,
            phase,
            session_id: Some("s1".into()),
            handle_id: None,
            tool: "bash".into(),
            tool_runtime: "bash".into(),
            tool_id: "toolu_1".into(),
            ts: "2026-10-24T18:00:00.123Z".into(),
        };
        let cases = [
            (
                base(ToolCallPhase::Called { fp: "664c8869d8c4b4dd".into(), cwd: Some("/srv/project".into()) }),
                json!({"v":1,"phase":"called","fp":"664c8869d8c4b4dd","cwd":"/srv/project","session_id":"s1","tool":"bash","tool_runtime":"bash","tool_id":"toolu_1","ts":"2026-10-24T18:00:00.123Z"}),
            ),
            (
                base(ToolCallPhase::Called { fp: "664c8869d8c4b4dd".into(), cwd: None }),
                json!({"v":1,"phase":"called","fp":"664c8869d8c4b4dd","session_id":"s1","tool":"bash","tool_runtime":"bash","tool_id":"toolu_1","ts":"2026-10-24T18:00:00.123Z"}),
            ),
            (
                base(ToolCallPhase::Decided { decision: ToolCallDecision::Confirm, by: vec!["jev".into()], fp: "0f".into() }),
                json!({"v":1,"phase":"decided","decision":"confirm","by":["jev"],"fp":"0f","session_id":"s1","tool":"bash","tool_runtime":"bash","tool_id":"toolu_1","ts":"2026-10-24T18:00:00.123Z"}),
            ),
            (
                ToolCallRecord {
                    session_id: None,
                    handle_id: Some("sa_3".into()),
                    ..base(ToolCallPhase::Finished { status: ToolCallStatus::Cancelled, duration_ms: 7 })
                },
                json!({"v":1,"phase":"finished","status":"cancelled","duration_ms":7,"session_id":null,"handle_id":"sa_3","tool":"bash","tool_runtime":"bash","tool_id":"toolu_1","ts":"2026-10-24T18:00:00.123Z"}),
            ),
        ];
        for (record, want) in cases {
            assert_eq!(serde_json::to_value(&record).unwrap(), want);
            let back: ToolCallRecord = serde_json::from_value(want).unwrap();
            assert_eq!(back, record);
        }
        for (d, s) in [
            (ToolCallDecision::Allow, "allow"),
            (ToolCallDecision::Confirm, "confirm"),
            (ToolCallDecision::Block, "block"),
            (ToolCallDecision::Modify, "modify"),
        ] {
            assert_eq!(serde_json::to_value(d).unwrap(), json!(s));
        }
        for (st, s) in [
            (ToolCallStatus::Ok, "ok"),
            (ToolCallStatus::Error, "error"),
            (ToolCallStatus::Blocked, "blocked"),
            (ToolCallStatus::Cancelled, "cancelled"),
        ] {
            assert_eq!(serde_json::to_value(st).unwrap(), json!(s));
        }
    }

    // ── group 5 (unit): no content ──

    #[test]
    fn no_record_or_debug_line_carries_the_input() {
        let hub = on_hub();
        let mut sub = hub.subscribe().unwrap();
        let input = json!({"command": format!("curl -H 'x-api-key: {SECRET}' /home/u/secret.txt")});
        let rewritten = json!({"command": format!("echo {SECRET}")});
        // The base directory is a path, not content: it is let through.
        let hook_call = ToolCallHookContext { cwd: Some("/srv/project".into()), ..call(Some("s"), "toolu_s") };
        let mut events = ToolCallEvents::begin(&hub, "bash", None, &hook_call, None, &input);
        events.decided(ToolCallDecision::Modify, vec!["rewriter".into()], &rewritten);
        let debug = format!("{events:?} {hub:?}");
        events.finish(ToolCallStatus::Ok);
        let got = drain(&mut sub);
        assert_eq!(got.len(), 3);
        assert!(matches!(&got[0].record.phase, ToolCallPhase::Called { cwd: Some(_), .. }));
        for text in got
            .iter()
            .flat_map(|d| [serde_json::to_string(&*d.record).unwrap(), format!("{d:?}")])
            .chain([debug])
        {
            for needle in [SECRET, "secret.txt", "curl", "/home/u"] {
                assert!(!text.contains(needle), "{needle} leaked: {text}");
            }
        }
    }

    // ── group 6: fail-open ──

    #[test]
    fn with_no_subscriber_a_call_is_a_no_op() {
        let hub = on_hub();
        for i in 0..10 {
            let id = format!("t{i}");
            let mut events = ToolCallEvents::begin(&hub, "bash", None, &call(None, &id), None, &json!({}));
            assert!(!events.is_active(), "nobody subscribed: nothing is built");
            events.finish(ToolCallStatus::Ok);
        }
        let mut sub = hub.subscribe().unwrap();
        let mut events = ToolCallEvents::begin(&hub, "bash", None, &call(None, "after"), None, &json!({}));
        events.finish(ToolCallStatus::Ok);
        let got = drain(&mut sub);
        assert_eq!(got.first().map(|d| (d.seq, d.dropped)), Some((1, 0)), "no-op calls used no seq");
    }

    #[test]
    fn a_subscriber_that_never_reads_never_delays_a_call() {
        let hub = on_hub();
        let mut sub = hub.subscribe().unwrap();
        let calls = 3 * TOOL_CALL_HUB_CAPACITY;
        let started = Instant::now();
        for i in 0..calls {
            let id = format!("t{i}");
            let mut events =
                ToolCallEvents::begin(&hub, "bash", None, &call(Some("s"), &id), None, &json!({"i": i}));
            events.decided(ToolCallDecision::Allow, vec![], &json!({"i": i}));
            events.finish(ToolCallStatus::Ok);
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "{calls} calls against a stalled subscriber took {elapsed:?}"
        );
        let first = sub.try_next().expect("the newest records are kept");
        let sent = 3 * calls as u64;
        assert!(first.dropped > 0, "the gap is reported: {first:?}");
        assert_eq!(first.seq, first.dropped + 1, "dropped counts exactly the skipped records");
        assert_eq!(first.dropped, sent - TOOL_CALL_HUB_CAPACITY as u64);
        let second = sub.try_next().unwrap();
        assert_eq!((second.seq, second.dropped), (first.seq + 1, 0));
    }

    // ── groups 3 and 4: the hook chain's decision ──

    struct Fixed {
        id: &'static str,
        result: HookResult,
    }

    #[async_trait::async_trait]
    impl crate::extensions::runtime::ExtensionHandler for Fixed {
        fn id(&self) -> &str {
            self.id
        }
        async fn handle(&self, _event: &HookEvent) -> HookResult {
            self.result.clone()
        }
        async fn shutdown(&self) {}
    }

    async fn bus_with(handlers: Vec<(&'static str, HookResult)>) -> Arc<HookBus> {
        let bus = Arc::new(HookBus::new());
        bus.tool_call_hub().set_enabled(true);
        let mut perms = PermissionSet::new();
        perms.grant(Permission::ToolsIntercept);
        for (id, result) in handlers {
            bus.subscribe(HookKind::BeforeToolCall, Arc::new(Fixed { id, result }), None, None, perms.clone())
                .await
                .unwrap();
        }
        bus
    }

    #[tokio::test]
    async fn a_confirm_is_decided_by_its_confirmer_and_its_prompt_names_the_call() {
        let bus = bus_with(vec![("jev", HookResult::confirm("Run ls?"))]).await;
        let mut sub = bus.tool_call_hub().subscribe().unwrap();
        let input = json!({"command": "ls"});
        let hook_call = call(Some("conv-1"), "toolu_c");
        let (result, mut events) =
            crate::runtime::emit_before_tool_call_recorded(&bus, "bash", Some("bash"), input.clone(), &hook_call, None)
                .await;
        let latch = crate::tools::SessionConfirmLatch::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = crate::tools::SecretPromptHandle::new(tx);
        let answer = tokio::spawn(async move {
            let prompt = rx.recv().await.expect("a confirm prompt");
            let scope = prompt.confirm_scope.clone();
            let _ = prompt.response_tx.send(Some("no".into()));
            scope
        });
        let decision = crate::runtime::resolve_before_tool_call_decision_for(
            &hook_call,
            input,
            result,
            Some(&handle),
            false,
            Some(&latch),
        )
        .await;
        assert!(matches!(decision, crate::runtime::BeforeToolCallDecision::Block { .. }));
        events.finish(ToolCallStatus::Blocked);
        let scope = answer.await.unwrap().expect("a scope: this path has a latch");
        let got = drain(&mut sub);
        let phases: Vec<_> = got.iter().map(|d| phase_name(&d.record)).collect();
        assert_eq!(phases, ["called", "decided", "finished"]);
        assert!(matches!(
            &got[1].record.phase,
            ToolCallPhase::Decided { decision: ToolCallDecision::Confirm, by, .. } if by == &["jev".to_string()]
        ), "{:?}", got[1].record);
        assert_eq!(scope.tool_id.as_deref(), Some(got[1].record.tool_id.as_str()));
        assert_eq!(scope.tool_id.as_deref(), Some("toolu_c"));
        assert!(matches!(got[2].record.phase, ToolCallPhase::Finished { status: ToolCallStatus::Blocked, .. }));
    }

    #[tokio::test]
    async fn a_modify_changes_the_decided_fp_and_names_the_modifier() {
        let rewritten = json!({"command": "ls -la"});
        let bus = bus_with(vec![
            ("watcher", HookResult::Continue),
            ("rewriter", HookResult::Modify { input: rewritten.clone() }),
        ])
        .await;
        let mut sub = bus.tool_call_hub().subscribe().unwrap();
        let input = json!({"command": "ls"});
        let (result, mut events) = crate::runtime::emit_before_tool_call_recorded(
            &bus,
            "bash",
            None,
            input.clone(),
            &call(Some("s"), "toolu_m"),
            None,
        )
        .await;
        assert!(matches!(result, HookResult::Modify { .. }));
        events.finish(ToolCallStatus::Ok);
        let got = drain(&mut sub);
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(fp_of(&got[0].record), call_fingerprint("bash", &input));
        assert_eq!(fp_of(&got[1].record), call_fingerprint("bash", &rewritten));
        assert_ne!(fp_of(&got[0].record), fp_of(&got[1].record), "modified: the fp differs");
        assert!(matches!(
            &got[1].record.phase,
            ToolCallPhase::Decided { decision: ToolCallDecision::Modify, by, .. } if by == &["rewriter".to_string()]
        ), "{:?}", got[1].record);
    }

    #[tokio::test]
    async fn a_block_is_decided_by_its_blocker() {
        let bus = bus_with(vec![
            ("rewriter", HookResult::Modify { input: json!({"command": "x"}) }),
            ("guard", HookResult::Block { reason: "no".into() }),
        ])
        .await;
        let mut sub = bus.tool_call_hub().subscribe().unwrap();
        let (result, mut events) = crate::runtime::emit_before_tool_call_recorded(
            &bus,
            "bash",
            None,
            json!({"command": "ls"}),
            &call(Some("s"), "toolu_b"),
            None,
        )
        .await;
        assert!(matches!(result, HookResult::Block { .. }));
        events.finish(ToolCallStatus::Blocked);
        let got = drain(&mut sub);
        assert!(matches!(
            &got[1].record.phase,
            ToolCallPhase::Decided { decision: ToolCallDecision::Block, by, .. } if by == &["guard".to_string()]
        ), "{got:?}");
        assert!(matches!(got[2].record.phase, ToolCallPhase::Finished { status: ToolCallStatus::Blocked, .. }));
    }

    /// The no-extension fast path still records `decided(allow)`.
    #[tokio::test]
    async fn with_no_extensions_a_call_is_decided_allow() {
        let bus = bus_with(vec![]).await;
        let mut sub = bus.tool_call_hub().subscribe().unwrap();
        let (result, events) = crate::runtime::emit_before_tool_call_recorded(
            &bus,
            "read",
            None,
            json!({"path": "a"}),
            &call(None, "toolu_n"),
            None,
        )
        .await;
        assert!(matches!(result, HookResult::Continue));
        drop(events);
        let phases: Vec<_> = drain(&mut sub).iter().map(|d| phase_name(&d.record)).collect();
        assert_eq!(phases, ["called", "decided", "finished"]);
    }

    /// A worker's bus shares its host's hub.
    #[test]
    fn a_bus_built_with_a_hub_shares_it() {
        let host = HookBus::new();
        let worker = HookBus::new().with_tool_call_hub(Arc::clone(host.tool_call_hub()));
        assert!(Arc::ptr_eq(host.tool_call_hub(), worker.tool_call_hub()));
    }
}
