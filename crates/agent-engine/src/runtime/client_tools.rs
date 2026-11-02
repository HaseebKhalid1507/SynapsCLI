//! Client tools v1 (IK1 seam B', docs/client-tools.md): tools the session's
//! input owner registers over the wire and answers itself.
//!
//! RAM only. One [`Overlay`] belongs to one session actor, which hands it to
//! its own root runtime explicitly; `Runtime::clone` (a worker, a keep-warm
//! ping, a driver copy) starts with an empty one. Nothing here enters the
//! shared tool registry, `tool_register_tx`, `SessionConfig`, a fork seed, a
//! reload or crash record, or a worker. A call runs only on the root stream
//! that offered it (no `delegation_parent`, a conversation id, a model call
//! id) while the offering registration is current; anything else is refused
//! before a frame is sent. Arguments and returned content are never logged
//! and never Debug-printed.
//!
//! Each registered tool is an extension-origin tool
//! ([`ExtensionTool`](crate::tools::extension::ExtensionTool)) whose handler
//! is a [`ClientToolHandler`]: a call mints an invocation id, parks a pending
//! entry, and sends one [`Outbound::Invoke`] to the session actor, which
//! addresses `tool_invoke` to the installer only. Each entry takes one
//! terminal decision under the overlay lock (contract §7, R3): Pending ->
//! Accepted -> taken by the handler, or Ended by a turn cancel (the dropped
//! call sends `tool_cancel`), its timeout or a lifecycle drop by the actor.
//! The handler re-checks the entry when it takes it, so an accepted result
//! whose authority ended first never reaches the stream. Cancel wins.
//!
//! Every exit of a call leaves a [`ClientToolStamp`] under its tool_use id
//! (contract §8), built from the call's registration and its validated
//! outcome code; the root stream takes it with the result
//! ([`Offer::take_stamp`]). A stamp holds no arguments and no content.
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use agent_core::ClientToolStamp;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::extensions::hooks::events::{HookEvent, HookResult};
use crate::extensions::runtime::process::RegisteredExtensionToolSpec;
use crate::extensions::runtime::ExtensionHandler;
use crate::tools::activation::{AuthorizedToolCall, ResolvedToolCall};
use crate::tools::catalog::ToolId;
use crate::tools::extension::ExtensionTool;
use crate::tools::{Tool, ToolCapabilities};

/// Registered client tools per session.
pub const MAX_TOOLS: usize = 32;
/// Client tool calls in flight per session; one more is refused (`synaps_busy`).
pub const MAX_PENDING: usize = 8;
/// How long a call waits for its `tool_return`.
pub const TIMEOUT: Duration = Duration::from_secs(120);
/// Largest `tool_return` content, in bytes.
pub const MAX_CONTENT_BYTES: usize = 262_144;
/// Largest `tool_return` `_meta`, serialized.
pub const MAX_META_BYTES: usize = 4_096;
/// Largest call arguments, as compact JSON; larger ones end the call with
/// no frame (`synaps_args_too_large`).
pub const MAX_ARGS_BYTES: usize = 1_048_576;
/// Test-only: a shorter per-call timeout in milliseconds (never longer than
/// [`TIMEOUT`]), read when the session creates its overlay.
pub const TEST_TIMEOUT_ENV: &str = "SYNAPS_TEST_CLIENT_TOOL_TIMEOUT_MS";
/// Test-only: how long a woken call waits before it takes its entry, in
/// milliseconds (at most [`TIMEOUT`]), read when the session creates its
/// overlay. It opens the accepted-but-not-taken window on purpose.
pub const TEST_TAKE_DELAY_ENV: &str = "SYNAPS_TEST_CLIENT_TOOL_TAKE_DELAY_MS";

/// Stamps an overlay holds for its stream to take; the oldest drops first
/// (a cancelled call's stamp is never taken).
const MAX_STAMPS: usize = 64;

const REGISTRATION_PREFIX: &str = "cr_";
const INVOCATION_PREFIX: &str = "ci_";

/// `<prefix>` + 32 lowercase hex digits from the thread CSPRNG (128 bits).
fn mint(prefix: &str) -> String {
    format!("{prefix}{:032x}", rand::random::<u128>())
}

/// A new `registration_id` (`cr_` + 32 hex).
pub fn mint_registration_id() -> String {
    mint(REGISTRATION_PREFIX)
}

/// The runtime name of a client tool: `<extension_id>:<name>`.
pub fn runtime_name(extension_id: &str, name: &str) -> String {
    format!("{extension_id}:{name}")
}

/// The wire name the model sees: `<extension_id>_<name>`.
pub fn wire_name(extension_id: &str, name: &str) -> String {
    format!("{extension_id}_{name}")
}

/// One registered client tool, as validated by the session actor.
#[derive(Clone, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub result_family: String,
    pub outcomes: Vec<String>,
}

impl fmt::Debug for ToolDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolDef")
            .field("name", &self.name)
            .field("result_family", &self.result_family)
            .field("outcomes", &self.outcomes)
            .finish_non_exhaustive()
    }
}

/// The registration in force: everything the installer registered.
#[derive(Clone, PartialEq)]
pub struct Registration {
    pub registration_id: String,
    pub extension_id: String,
    pub require_memory_only: bool,
    pub tools: Vec<ToolDef>,
}

impl fmt::Debug for Registration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registration")
            .field("registration_id", &self.registration_id)
            .field("extension_id", &self.extension_id)
            .field("require_memory_only", &self.require_memory_only)
            .field("tools", &self.tools)
            .finish()
    }
}

/// Why a client tool call ended without an accepted `tool_return`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// The turn was cancelled (the call's future was dropped).
    Cancelled,
    /// The daemon is reloading.
    Reload,
    /// No return within the timeout.
    Timeout,
    /// The installer disconnected.
    Detached,
    /// The session's input owner changed.
    OwnerChanged,
    /// `/new` or `/resume` started another conversation.
    NewConversation,
    /// [`MAX_PENDING`] calls were already in flight.
    Busy,
    /// The arguments are over [`MAX_ARGS_BYTES`] as compact JSON.
    ArgsTooLarge,
    /// The offering registration is no longer in force.
    Unregistered,
    /// Not the session's own root call (a worker, no conversation id).
    NotRoot,
}

impl Ended {
    /// The stamp outcome code (contract §7).
    pub fn outcome(self) -> &'static str {
        match self {
            Self::Cancelled => "synaps_cancelled",
            Self::Reload => "synaps_reload",
            Self::Timeout => "synaps_timeout",
            Self::Detached => "synaps_detached",
            Self::OwnerChanged => "synaps_owner_changed",
            Self::NewConversation => "synaps_new_conversation",
            Self::Busy => "synaps_busy",
            Self::ArgsTooLarge => "synaps_args_too_large",
            Self::Unregistered => "synaps_unregistered",
            Self::NotRoot => "synaps_not_root",
        }
    }

    /// Ends the turn too: the turn's own cancel phrase is the result, so the
    /// call waits for the turn's cancel instead of answering.
    fn ends_turn(self) -> bool {
        matches!(self, Self::Cancelled | Self::Reload | Self::NewConversation)
    }

    /// The model text, without the `Tool execution failed: ` prefix that
    /// [`crate::RuntimeError::Tool`] adds. `wire` is the wire name, `call` the
    /// model's tool_use id, `timeout` the call's timeout.
    pub fn text(self, wire: &str, call: &str, timeout: Duration) -> String {
        match self {
            Self::Timeout => format!(
                "client tool {wire} timed out after {} (call {call}); nothing came back. Call it again if you still need it.",
                span(timeout)
            ),
            Self::Detached => format!(
                "client tool {wire} ended: its client disconnected (call {call}). Call it again if you still need it."
            ),
            Self::OwnerChanged => {
                format!("client tool {wire} ended: the session's input owner changed (call {call}).")
            }
            Self::Busy => format!(
                "client tool {wire} refused: {MAX_PENDING} client tool calls are already in flight in this session. Wait for one to finish, then call again."
            ),
            Self::ArgsTooLarge => format!(
                "client tool {wire} refused: its arguments are over {MAX_ARGS_BYTES} bytes (call {call}). Send smaller arguments."
            ),
            Self::Unregistered => {
                format!("client tool {wire} is no longer registered in this session (call {call}).")
            }
            Self::NotRoot => format!(
                "client tool {wire} runs only in the session's own conversation, not in a worker or without a conversation id."
            ),
            // Normally replaced by the turn's cancel phrase.
            Self::Cancelled => format!("client tool {wire} ended: the turn was cancelled (call {call})."),
            Self::Reload => format!("client tool {wire} ended: the daemon is reloading (call {call})."),
            Self::NewConversation => {
                format!("client tool {wire} ended: a new conversation started (call {call}).")
            }
        }
    }
}

fn span(d: Duration) -> String {
    if d.subsec_millis() == 0 {
        format!("{} s", d.as_secs())
    } else {
        format!("{} ms", d.as_millis())
    }
}

/// Why a `tool_cancel` is sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelReason {
    TurnCancel,
    Timeout,
    Reload,
    OwnerChange,
    NewConversation,
}

impl CancelReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TurnCancel => "turn_cancel",
            Self::Timeout => "timeout",
            Self::Reload => "reload",
            Self::OwnerChange => "owner_change",
            Self::NewConversation => "new_conversation",
        }
    }
}

/// Why the session actor drops the registration (contract §10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropReason {
    /// The installer detached, hit EOF or said Bye: no frame is sent.
    Detached,
    OwnerChange,
    Reload,
    NewConversation,
}

impl DropReason {
    fn ended(self) -> Ended {
        match self {
            Self::Detached => Ended::Detached,
            Self::OwnerChange => Ended::OwnerChanged,
            Self::Reload => Ended::Reload,
            Self::NewConversation => Ended::NewConversation,
        }
    }

    fn cancel(self) -> Option<CancelReason> {
        match self {
            Self::Detached => None,
            Self::OwnerChange => Some(CancelReason::OwnerChange),
            Self::Reload => Some(CancelReason::Reload),
            Self::NewConversation => Some(CancelReason::NewConversation),
        }
    }
}

/// One call for the installer (`tool_invoke`).
pub struct Invoke {
    pub invocation_id: String,
    pub registration_id: String,
    /// The calling conversation (the stream's `session_id`).
    pub conversation_id: String,
    /// The calling turn (never empty: the §4 gate refuses a call without).
    pub turn_correlation_id: String,
    pub tool_use_id: String,
    pub tool: String,
    pub wire_name: String,
    pub args: Value,
    pub deadline_ms: u64,
}

impl fmt::Debug for Invoke {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Invoke")
            .field("invocation_id", &self.invocation_id)
            .field("registration_id", &self.registration_id)
            .field("tool_use_id", &self.tool_use_id)
            .field("wire_name", &self.wire_name)
            .finish_non_exhaustive()
    }
}

/// One `tool_cancel` for the installer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cancel {
    pub invocation_id: String,
    pub registration_id: String,
    pub reason: CancelReason,
}

/// What a call sends the session actor, in order.
#[derive(Debug)]
pub enum Outbound {
    Invoke(Invoke),
    Cancel(Cancel),
}

/// Why a `tool_return` was refused after the sender check. None of these
/// but `NotPending` touches the call: it stays pending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReturnRefused {
    NotPending,
    UnknownOutcome,
    TooLarge,
}

impl ReturnRefused {
    pub fn code(self) -> &'static str {
        match self {
            Self::NotPending => "not_pending",
            Self::UnknownOutcome => "unknown_outcome",
            Self::TooLarge => "too_large",
        }
    }
}

/// What a lifecycle drop took away.
#[derive(Debug, Default)]
pub struct Dropped {
    /// The registration that was in force, if any.
    pub registration: Option<Registration>,
    /// The `tool_cancel` frames to send its installer (none for a detach).
    pub cancels: Vec<Cancel>,
}

/// Where one call stands (contract §7, R3). Only `Pending` takes a return;
/// a lifecycle drop moves `Pending` or `Accepted` to `Ended`; the handler
/// takes (removes) the entry in whatever state it finds.
enum Call {
    Pending,
    Accepted(Answer),
    Ended(Ended),
}

/// An accepted return: its validated outcome code and its whole content.
struct Answer {
    outcome: String,
    content: String,
    memory_only: bool,
}

struct Entry {
    seq: u64,
    registration_id: String,
    outcomes: Vec<String>,
    call: Call,
    /// Wakes the waiting handler once the call leaves `Pending`.
    wake: Option<oneshot::Sender<()>>,
}

impl Entry {
    /// Not ended yet: it counts against [`MAX_PENDING`].
    fn live(&self) -> bool {
        !matches!(self.call, Call::Ended(_))
    }

    fn settle(&mut self, call: Call) {
        self.call = call;
        if let Some(wake) = self.wake.take() {
            let _ = wake.send(());
        }
    }
}

struct State {
    registration: Option<Registration>,
    offer: Arc<Offer>,
    pending: HashMap<String, Entry>,
    next_seq: u64,
    /// `None` on an overlay no session owns (a runtime clone): it never
    /// sends a frame.
    outbound: Option<mpsc::UnboundedSender<Outbound>>,
    timeout: Duration,
    /// Test-only ([`TEST_TAKE_DELAY_ENV`]); zero otherwise.
    take_delay: Duration,
    /// Each ended call's stamp by tool_use id, oldest first, at most
    /// [`MAX_STAMPS`].
    stamps: VecDeque<(String, ClientToolStamp)>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            registration: None,
            offer: Arc::default(),
            pending: HashMap::new(),
            next_seq: 0,
            outbound: None,
            timeout: TIMEOUT,
            take_delay: Duration::ZERO,
            stamps: VecDeque::new(),
        }
    }
}

impl State {
    fn send(&self, frame: Outbound) -> bool {
        self.outbound.as_ref().is_some_and(|tx| tx.send(frame).is_ok())
    }

    fn live(&self) -> usize {
        self.pending.values().filter(|entry| entry.live()).count()
    }

    /// Leave `stamp` for the stream that made call `tool_use_id`.
    fn stamp(&mut self, tool_use_id: &str, stamp: ClientToolStamp) {
        self.stamps.retain(|(id, _)| id != tool_use_id);
        self.stamps.push_back((tool_use_id.to_owned(), stamp));
        while self.stamps.len() > MAX_STAMPS {
            self.stamps.pop_front();
        }
    }
}

/// A test-only duration from `name` in milliseconds, at most [`TIMEOUT`].
fn test_ms(name: &str) -> Option<Duration> {
    std::env::var(name)
        .ok()
        .and_then(|ms| ms.parse::<u64>().ok())
        .map(Duration::from_millis)
        .filter(|d| *d <= TIMEOUT)
}

/// The compact JSON length of `value`, counted without building the text.
fn compact_len(value: &Value) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(buf.len());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    match serde_json::to_writer(&mut count, value) {
        Ok(()) => count.0,
        Err(_) => usize::MAX,
    }
}

fn lock(shared: &Mutex<State>) -> MutexGuard<'_, State> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One session's client tools (RAM only). Cloning shares it; `default()`
/// is an empty overlay no session owns.
#[derive(Clone, Default)]
pub struct Overlay {
    shared: Arc<Mutex<State>>,
}

impl fmt::Debug for Overlay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock(&self.shared);
        f.debug_struct("Overlay")
            .field(
                "registration_id",
                &state.registration.as_ref().map(|r| r.registration_id.as_str()),
            )
            .field("pending", &state.live())
            .finish()
    }
}

impl Overlay {
    /// A session's overlay, and the receiver of every frame its calls send.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<Outbound>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let state = State {
            outbound: Some(tx),
            timeout: test_ms(TEST_TIMEOUT_ENV).unwrap_or(TIMEOUT),
            take_delay: test_ms(TEST_TAKE_DELAY_ENV).unwrap_or_default(),
            ..State::default()
        };
        (Self { shared: Arc::new(Mutex::new(state)) }, rx)
    }

    /// Put `registration` in force (validated by the caller). Calls pending
    /// under the same registration id keep running.
    pub fn install(&self, registration: Registration) {
        let offer = Offer::build(&registration, Arc::downgrade(&self.shared));
        let mut state = lock(&self.shared);
        state.offer = Arc::new(offer);
        state.registration = Some(registration);
    }

    /// The registration in force.
    pub fn registration(&self) -> Option<Registration> {
        lock(&self.shared).registration.clone()
    }

    /// The id of the registration in force.
    pub fn registration_id(&self) -> Option<String> {
        lock(&self.shared).registration.as_ref().map(|r| r.registration_id.clone())
    }

    /// Calls in flight (pending, or accepted and not yet taken).
    pub fn pending(&self) -> usize {
        lock(&self.shared).live()
    }

    /// Whether `invocation_id` still waits for its return.
    pub fn is_pending(&self, invocation_id: &str) -> bool {
        lock(&self.shared)
            .pending
            .get(invocation_id)
            .is_some_and(|entry| matches!(entry.call, Call::Pending))
    }

    /// The per-call timeout.
    pub fn timeout(&self) -> Duration {
        lock(&self.shared).timeout
    }

    /// The tools a root stream offers now (a snapshot; empty without a
    /// registration).
    pub fn offer(&self) -> Arc<Offer> {
        Arc::clone(&lock(&self.shared).offer)
    }

    /// Accept the one return of a pending call (the caller checked that it
    /// came from the installer). Order: not pending, unknown outcome, too
    /// large (`content`'s decoded bytes, or its `meta_bytes`). Only an
    /// accepted return leaves `Pending`; the content is kept whole.
    pub fn resolve(
        &self,
        invocation_id: &str,
        outcome: &str,
        content: String,
        meta_bytes: usize,
        memory_only: bool,
    ) -> Result<(), ReturnRefused> {
        let mut state = lock(&self.shared);
        let Some(entry) = state
            .pending
            .get_mut(invocation_id)
            .filter(|entry| matches!(entry.call, Call::Pending))
        else {
            return Err(ReturnRefused::NotPending);
        };
        if !entry.outcomes.iter().any(|known| known == outcome) {
            return Err(ReturnRefused::UnknownOutcome);
        }
        if content.len() > MAX_CONTENT_BYTES || meta_bytes > MAX_META_BYTES {
            return Err(ReturnRefused::TooLarge);
        }
        entry.settle(Call::Accepted(Answer { outcome: outcome.to_owned(), content, memory_only }));
        Ok(())
    }

    /// Drop the registration and end every live call at once, an accepted
    /// one not yet taken included (contract §7 R3, §10). Returns what was in
    /// force and the `tool_cancel` frames due, in call order.
    pub fn drop_registration(&self, reason: DropReason) -> Dropped {
        let mut state = lock(&self.shared);
        let registration = state.registration.take();
        state.offer = Arc::default();
        let mut ended: Vec<(&String, &mut Entry)> =
            state.pending.iter_mut().filter(|(_, entry)| entry.live()).collect();
        ended.sort_by_key(|(_, entry)| entry.seq);
        let cancels = ended
            .into_iter()
            .filter_map(|(invocation_id, entry)| {
                entry.settle(Call::Ended(reason.ended()));
                reason.cancel().map(|reason| Cancel {
                    invocation_id: invocation_id.clone(),
                    registration_id: entry.registration_id.clone(),
                    reason,
                })
            })
            .collect();
        Dropped { registration, cancels }
    }
}

struct Offered {
    wire_name: String,
    runtime_name: String,
    tool_id: ToolId,
    schema: Value,
    tool: Arc<dyn Tool>,
}

/// The client tools of one registration, as a root stream offers them.
#[derive(Default)]
pub struct Offer {
    tools: Vec<Offered>,
    /// The registration's `require_memory_only` (fixed for its life, R1).
    memory_only: bool,
    /// The overlay its calls leave their stamps in (none for the empty offer).
    shared: Weak<Mutex<State>>,
}

impl fmt::Debug for Offer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.tools.iter().map(|t| &t.wire_name)).finish()
    }
}

impl Offer {
    fn build(registration: &Registration, shared: Weak<Mutex<State>>) -> Self {
        let handler: Arc<dyn ExtensionHandler> =
            Arc::new(ClientToolHandler::new(registration, Weak::clone(&shared)));
        let extension_id = registration.extension_id.as_str();
        let tools = registration
            .tools
            .iter()
            .map(|def| Offered {
                wire_name: wire_name(extension_id, &def.name),
                runtime_name: runtime_name(extension_id, &def.name),
                tool_id: ToolId::extension(extension_id, &def.name),
                schema: serde_json::json!({
                    "name": wire_name(extension_id, &def.name),
                    "description": def.description,
                    "input_schema": def.input_schema,
                }),
                tool: Arc::new(ExtensionTool::new(
                    extension_id,
                    RegisteredExtensionToolSpec {
                        name: def.name.clone(),
                        description: def.description.clone(),
                        input_schema: def.input_schema.clone(),
                    },
                    Arc::clone(&handler),
                )),
            })
            .collect();
        Self { tools, memory_only: registration.require_memory_only, shared }
    }

    /// Whether a call of `wire_name` is a memory-only client tool's (its
    /// registration requires memory-only results): its hooks are shown the
    /// name only (decision 18(a)). A registry tool the wire name resolves
    /// to first is withheld too, never the other way round.
    pub fn memory_only(&self, wire_name: &str) -> bool {
        self.memory_only && self.tools.iter().any(|t| t.wire_name == wire_name)
    }

    /// The stamp a client tool call `tool_use_id` left, taken once. `None`
    /// for any other call: the result is not a client tool's.
    pub fn take_stamp(&self, tool_use_id: &str) -> Option<ClientToolStamp> {
        let shared = self.shared.upgrade()?;
        let mut state = lock(&shared);
        let at = state.stamps.iter().position(|(id, _)| id == tool_use_id)?;
        state.stamps.remove(at).map(|(_, stamp)| stamp)
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// The wire names offered, in registration order.
    pub fn wire_names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.wire_name.clone()).collect()
    }

    /// The tool offered under `wire_name`.
    pub fn tool(&self, wire_name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.wire_name == wire_name).map(|t| Arc::clone(&t.tool))
    }

    /// The schema entries of every offered tool whose wire name none of
    /// `held` holds (a client tool never shadows another), in registration
    /// order.
    pub(crate) fn unheld_schema(&self, held: &[&[Value]]) -> Vec<Value> {
        self.tools
            .iter()
            .filter(|t| {
                !held
                    .iter()
                    .any(|entries| entries.iter().any(|entry| entry["name"] == t.wire_name.as_str()))
            })
            .map(|t| t.schema.clone())
            .collect()
    }

    /// A root stream's round options with the offered tools appended after
    /// every other tool of the request (and of the loaded half of a deferred
    /// split, whose last entry then carries the cache breakpoint). `base` is
    /// the registry schema used when `options` carries none. `None` when no
    /// tool is added: the request stays byte-identical.
    pub(crate) fn round_options(
        &self,
        options: &super::api::ApiOptions,
        base: &[Value],
    ) -> Option<super::api::ApiOptions> {
        let schema = options.request_tools_schema.as_deref().map_or(base, Vec::as_slice);
        let split = options.anthropic_tool_split.as_deref();
        let mut held = vec![schema];
        if let Some(split) = split {
            held.push(&split.loaded);
            held.push(&split.deferred);
        }
        let added = self.unheld_schema(&held);
        if added.is_empty() {
            return None;
        }
        let anthropic_tool_split = split.map(|split| {
            let mut split = split.clone();
            split.loaded.extend(added.iter().cloned());
            Arc::new(split)
        });
        let mut schema = schema.to_vec();
        schema.extend(added);
        Some(super::api::ApiOptions {
            request_tools_schema: Some(Arc::new(schema)),
            anthropic_tool_split,
            ..options.clone()
        })
    }

    /// A call of an offered tool, for a wire name the shared registry does
    /// not resolve. Core: the root stream offered it in this round.
    pub(crate) fn authorize(&self, wire_name: &str) -> Option<AuthorizedToolCall> {
        self.tools.iter().find(|t| t.wire_name == wire_name).map(|t| {
            AuthorizedToolCall::client_tool(
                ResolvedToolCall::new(&t.wire_name, &t.runtime_name, t.tool_id.clone()),
                Arc::clone(&t.tool),
            )
        })
    }
}

/// The extension handler behind every tool of one registration. It holds
/// the overlay weakly: a dropped session ends its calls.
pub struct ClientToolHandler {
    shared: Weak<Mutex<State>>,
    registration_id: String,
    extension_id: String,
    /// Each tool's `result_family`, by its own name.
    families: HashMap<String, String>,
}

/// Owns a call's entry. Dropped before the handler took it (the turn was
/// cancelled), it removes the entry and, if the call was still live, sends
/// `tool_cancel{turn_cancel}`.
struct PendingGuard {
    shared: Arc<Mutex<State>>,
    invocation_id: String,
}

impl PendingGuard {
    /// The call's one terminal decision: take the entry as it stands now.
    /// `expired`: the timer fired first, so a live entry (accepted but not
    /// taken included) ends `Timeout` and its client gets `tool_cancel`.
    fn take(&self, expired: bool) -> Result<Answer, Ended> {
        let mut state = lock(&self.shared);
        let Some(entry) = state.pending.remove(&self.invocation_id) else {
            return Err(Ended::Unregistered);
        };
        match entry.call {
            Call::Ended(ended) => Err(ended),
            Call::Accepted(answer) if !expired => Ok(answer),
            Call::Pending | Call::Accepted(_) => {
                state.send(Outbound::Cancel(Cancel {
                    invocation_id: self.invocation_id.clone(),
                    registration_id: entry.registration_id,
                    reason: CancelReason::Timeout,
                }));
                Err(Ended::Timeout)
            }
        }
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let mut state = lock(&self.shared);
        if let Some(entry) = state.pending.remove(&self.invocation_id) {
            if entry.live() {
                state.send(Outbound::Cancel(Cancel {
                    invocation_id: self.invocation_id.clone(),
                    registration_id: entry.registration_id,
                    reason: CancelReason::TurnCancel,
                }));
            }
        }
    }
}

impl ClientToolHandler {
    fn new(registration: &Registration, shared: Weak<Mutex<State>>) -> Self {
        Self {
            shared,
            registration_id: registration.registration_id.clone(),
            extension_id: registration.extension_id.clone(),
            families: registration
                .tools
                .iter()
                .map(|def| (def.name.clone(), def.result_family.clone()))
                .collect(),
        }
    }

    /// The stamp of one call of `name` (contract §8): this registration, the
    /// invocation when one was sent, and the outcome code.
    fn stamp(&self, name: &str, invocation_id: Option<&str>, outcome: &str, memory_only: bool) -> ClientToolStamp {
        ClientToolStamp {
            registration_id: self.registration_id.clone(),
            invocation_id: invocation_id.map(str::to_owned),
            extension_id: self.extension_id.clone(),
            tool: name.to_owned(),
            family: self.families.get(name).cloned().unwrap_or_default(),
            outcome: outcome.to_owned(),
            memory_only,
        }
    }

    async fn invoke(&self, name: &str, args: Value, caps: &ToolCapabilities) -> Result<Value, String> {
        let wire = wire_name(&self.extension_id, name);
        let call = caps.tool_use_id.as_deref().unwrap_or_default();
        // §4 gate, before any frame: the session's own root call only, in
        // a turn the installer can correlate (a non-empty turn id).
        let (Some(conversation_id), Some(_), Some(turn_correlation_id), None) = (
            caps.session_id.as_deref(),
            caps.tool_use_id.as_deref(),
            caps.turn_correlation_id.as_deref().filter(|turn| !turn.is_empty()),
            caps.delegation_parent.as_deref(),
        ) else {
            if let (Some(tool_use_id), Some(shared)) = (caps.tool_use_id.as_deref(), self.shared.upgrade()) {
                lock(&shared).stamp(tool_use_id, self.stamp(name, None, Ended::NotRoot.outcome(), false));
            }
            return Err(Ended::NotRoot.text(&wire, call, TIMEOUT));
        };
        let Some(shared) = self.shared.upgrade() else {
            return Err(Ended::Unregistered.text(&wire, call, TIMEOUT));
        };
        // R2: measured before the lock; the args are never logged or kept
        // past the frame.
        let args_bytes = compact_len(&args);
        let (invocation_id, woken, timeout, take_delay) = {
            let mut state = lock(&shared);
            let timeout = state.timeout;
            let outcomes = state
                .registration
                .as_ref()
                .filter(|r| r.registration_id == self.registration_id)
                .and_then(|r| r.tools.iter().find(|t| t.name == name))
                .map(|def| def.outcomes.clone());
            let refused = match outcomes.filter(|_| state.outbound.is_some()) {
                None => Err(Ended::Unregistered),
                Some(_) if args_bytes > MAX_ARGS_BYTES => Err(Ended::ArgsTooLarge),
                Some(_) if state.live() >= MAX_PENDING => Err(Ended::Busy),
                Some(outcomes) => Ok(outcomes),
            };
            let outcomes = match refused {
                Ok(outcomes) => outcomes,
                Err(ended) => {
                    state.stamp(call, self.stamp(name, None, ended.outcome(), false));
                    return Err(ended.text(&wire, call, timeout));
                }
            };
            let mut invocation_id = mint(INVOCATION_PREFIX);
            while state.pending.contains_key(&invocation_id) {
                invocation_id = mint(INVOCATION_PREFIX);
            }
            let sent = state.send(Outbound::Invoke(Invoke {
                invocation_id: invocation_id.clone(),
                registration_id: self.registration_id.clone(),
                conversation_id: conversation_id.to_owned(),
                turn_correlation_id: turn_correlation_id.to_owned(),
                tool_use_id: call.to_owned(),
                tool: name.to_owned(),
                wire_name: wire.clone(),
                args,
                deadline_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            }));
            if !sent {
                state.stamp(call, self.stamp(name, None, Ended::Unregistered.outcome(), false));
                return Err(Ended::Unregistered.text(&wire, call, timeout));
            }
            let (wake, woken) = oneshot::channel();
            let seq = state.next_seq;
            state.next_seq += 1;
            state.pending.insert(
                invocation_id.clone(),
                Entry {
                    seq,
                    registration_id: self.registration_id.clone(),
                    outcomes,
                    call: Call::Pending,
                    wake: Some(wake),
                },
            );
            (invocation_id, woken, timeout, state.take_delay)
        };
        let started = Instant::now();
        let guard = PendingGuard { shared, invocation_id };
        // A wake already there wins over a timer that fired with it.
        let expired = tokio::time::timeout(timeout, woken).await.is_err();
        if !expired && !take_delay.is_zero() {
            tokio::time::sleep(take_delay).await;
        }
        let taken = guard.take(expired);
        let stamp = match &taken {
            Ok(answer) => self.stamp(name, Some(&guard.invocation_id), &answer.outcome, answer.memory_only),
            Err(ended) => self.stamp(name, Some(&guard.invocation_id), ended.outcome(), false),
        };
        // Left before any wait below: a cancelled stream discards it.
        lock(&guard.shared).stamp(call, stamp);
        match taken {
            Ok(Answer { content, memory_only: false, .. }) => Ok(Value::String(content)),
            Ok(Answer { content, memory_only: true, .. }) => Ok(serde_json::json!({
                "content": content,
                "_meta": { crate::runtime::memory_only::META_KEY: true },
            })),
            Err(ended) => {
                if ended.ends_turn() {
                    // The turn is being cancelled: its cancel phrase wins.
                    if let Some(cancel) = &caps.launch_cancel {
                        let left = timeout.saturating_sub(started.elapsed());
                        let _ = tokio::time::timeout(left, cancel.cancelled()).await;
                    }
                }
                Err(ended.text(&wire, call, timeout))
            }
        }
    }
}

#[async_trait::async_trait]
impl ExtensionHandler for ClientToolHandler {
    fn id(&self) -> &str {
        &self.extension_id
    }

    async fn handle(&self, _event: &HookEvent) -> HookResult {
        HookResult::Continue
    }

    async fn shutdown(&self) {}

    /// Without the calling context there is no root call to serve.
    async fn call_tool(&self, name: &str, _input: Value) -> Result<Value, String> {
        Err(Ended::NotRoot.text(&wire_name(&self.extension_id, name), "", TIMEOUT))
    }

    async fn call_tool_in(
        &self,
        name: &str,
        input: Value,
        caps: &ToolCapabilities,
    ) -> Result<Value, String> {
        self.invoke(name, input, caps).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_prefixed_128_bit_hex() {
        let id = mint_registration_id();
        assert_eq!(id.len(), 35);
        assert!(id.starts_with("cr_"));
        assert!(id[3..].chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
        assert_ne!(id, mint_registration_id());
        assert!(mint(INVOCATION_PREFIX).starts_with("ci_"));
    }

    #[test]
    fn names_and_texts_follow_the_contract() {
        assert_eq!(runtime_name("pria-desktop", "pria_vault_get"), "pria-desktop:pria_vault_get");
        assert_eq!(wire_name("pria-desktop", "pria_vault_get"), "pria-desktop_pria_vault_get");
        assert_eq!(
            Ended::Timeout.text("w", "toolu_1", TIMEOUT),
            "client tool w timed out after 120 s (call toolu_1); nothing came back. Call it again if you still need it."
        );
        assert_eq!(
            Ended::Busy.text("w", "toolu_1", TIMEOUT),
            "client tool w refused: 8 client tool calls are already in flight in this session. Wait for one to finish, then call again."
        );
        assert_eq!(Ended::Detached.outcome(), "synaps_detached");
    }

    #[test]
    fn the_schema_never_shadows_a_tool_the_request_holds() {
        let (overlay, _rx) = Overlay::new();
        assert!(overlay.offer().is_empty());
        overlay.install(Registration {
            registration_id: mint_registration_id(),
            extension_id: "pria-desktop".into(),
            require_memory_only: false,
            tools: ["a", "b"]
                .map(|name| ToolDef {
                    name: name.into(),
                    description: "d".into(),
                    input_schema: serde_json::json!({"type": "object"}),
                    result_family: "f".into(),
                    outcomes: vec!["ok".into()],
                })
                .to_vec(),
        });
        let base = vec![serde_json::json!({"name": "pria-desktop_a"})];
        let offer = overlay.offer();
        let names = |schema: &[Value]| -> Vec<String> {
            schema.iter().map(|e| e["name"].as_str().unwrap().to_owned()).collect()
        };
        assert_eq!(names(&offer.unheld_schema(&[&base])), ["pria-desktop_b"]);
        let plain = super::super::api::ApiOptions::default();
        let merged = offer.round_options(&plain, &base).unwrap();
        let schema = merged.request_tools_schema.as_deref().unwrap();
        assert_eq!(names(schema), ["pria-desktop_a", "pria-desktop_b"]);
        assert!(merged.anthropic_tool_split.is_none());
        assert!(offer.round_options(&merged, &[]).is_none(), "nothing left to add");
        let split = super::super::api::ApiOptions {
            anthropic_tool_split: Some(Arc::new(crate::tools::DeferredToolSplit {
                loaded: vec![serde_json::json!({"name": "read"})],
                deferred: vec![serde_json::json!({"name": "pria-desktop_a"})],
                ..Default::default()
            })),
            ..Default::default()
        };
        let merged = offer.round_options(&split, &[]).unwrap();
        let split = merged.anthropic_tool_split.as_deref().unwrap();
        assert_eq!(names(&split.loaded), ["read", "pria-desktop_b"]);
        assert_eq!(names(merged.request_tools_schema.as_deref().unwrap()), ["pria-desktop_b"]);
    }

    const WIRE: &str = "pria-desktop_get";

    fn one_tool() -> (Overlay, mpsc::UnboundedReceiver<Outbound>, Arc<ClientToolHandler>) {
        let (overlay, rx) = Overlay::new();
        let registration = Registration {
            registration_id: mint_registration_id(),
            extension_id: "pria-desktop".into(),
            require_memory_only: false,
            tools: vec![ToolDef {
                name: "get".into(),
                description: "d".into(),
                input_schema: serde_json::json!({"type": "object"}),
                result_family: "pria.vault".into(),
                outcomes: vec!["ok".into()],
            }],
        };
        let handler = Arc::new(ClientToolHandler::new(&registration, Arc::downgrade(&overlay.shared)));
        overlay.install(registration);
        (overlay, rx, handler)
    }

    fn call(handler: &Arc<ClientToolHandler>, args: Value) -> tokio::task::JoinHandle<Result<Value, String>> {
        let handler = Arc::clone(handler);
        tokio::spawn(async move {
            let mut caps = crate::tools::test_helpers::create_tool_context().capabilities;
            caps.session_id = Some("conv-1".into());
            caps.tool_use_id = Some("toolu_1".into());
            caps.turn_correlation_id = Some("turn-1".into());
            handler.invoke("get", args, &caps).await
        })
    }

    async fn invoked(rx: &mut mpsc::UnboundedReceiver<Outbound>) -> Invoke {
        match rx.recv().await {
            Some(Outbound::Invoke(invoke)) => invoke,
            other => panic!("expected an invoke, got {other:?}"),
        }
    }

    /// §4: a call with a tool_use_id but no turn id (absent or empty) is
    /// refused not-root before any frame: the invoke's turn id is required.
    #[tokio::test]
    async fn a_call_without_a_turn_id_is_refused_before_any_frame() {
        let (overlay, mut rx, handler) = one_tool();
        for turn in [None, Some(String::new())] {
            let mut caps = crate::tools::test_helpers::create_tool_context().capabilities;
            caps.session_id = Some("conv-1".into());
            caps.tool_use_id = Some("toolu_1".into());
            caps.turn_correlation_id = turn.clone();
            let text = tokio::time::timeout(Duration::from_secs(5), handler.invoke("get", serde_json::json!({}), &caps))
                .await
                .unwrap_or_else(|_| panic!("turn id {turn:?}: refused at once, not sent"))
                .expect_err("refused");
            assert_eq!(text, Ended::NotRoot.text(WIRE, "toolu_1", TIMEOUT), "turn id {turn:?}");
            assert!(rx.try_recv().is_err(), "turn id {turn:?}: no frame was sent");
            assert_eq!(overlay.pending(), 0, "turn id {turn:?}: nothing is pending");
            let stamp = overlay.offer().take_stamp("toolu_1").expect("stamped");
            assert_eq!((stamp.invocation_id, stamp.outcome.as_str()), (None, "synaps_not_root"));
        }
        assert!(overlay.registration().is_some(), "a refusal leaves the registration in force");
    }

    #[tokio::test]
    async fn an_accepted_result_not_yet_taken_ends_with_its_authority() {
        let (overlay, mut rx, handler) = one_tool();
        lock(&overlay.shared).take_delay = Duration::from_millis(300);
        let running = call(&handler, serde_json::json!({}));
        let invoke = invoked(&mut rx).await;
        assert!(overlay.is_pending(&invoke.invocation_id));
        overlay.resolve(&invoke.invocation_id, "ok", "kept".into(), 0, false).expect("accepted");
        assert!(!overlay.is_pending(&invoke.invocation_id));
        assert_eq!(overlay.pending(), 1, "accepted, not taken: still in flight");
        let dropped = overlay.drop_registration(DropReason::OwnerChange);
        assert_eq!(
            dropped.cancels,
            [Cancel {
                invocation_id: invoke.invocation_id.clone(),
                registration_id: invoke.registration_id.clone(),
                reason: CancelReason::OwnerChange,
            }]
        );
        assert_eq!(overlay.pending(), 0);
        let text = running.await.expect("joined").expect_err("the owner change wins");
        assert_eq!(text, Ended::OwnerChanged.text(WIRE, "toolu_1", TIMEOUT));
        assert_eq!(
            overlay.resolve(&invoke.invocation_id, "ok", "late".into(), 0, false),
            Err(ReturnRefused::NotPending)
        );
        assert!(rx.try_recv().is_err(), "nothing more was sent");
    }

    #[tokio::test]
    async fn a_return_is_checked_in_order_and_kept_whole() {
        let (overlay, mut rx, handler) = one_tool();
        let running = call(&handler, serde_json::json!({"q": 1}));
        let invoke = invoked(&mut rx).await;
        let id = invoke.invocation_id.as_str();
        let refused = |outcome: &str, content: String, meta: usize| {
            overlay.resolve(id, outcome, content, meta, false).expect_err("refused")
        };
        assert_eq!(
            overlay.resolve("ci_0", "ok", String::new(), 0, false),
            Err(ReturnRefused::NotPending)
        );
        assert_eq!(refused("nope", String::new(), 0), ReturnRefused::UnknownOutcome);
        assert_eq!(refused("ok", "x".repeat(MAX_CONTENT_BYTES + 1), 0), ReturnRefused::TooLarge);
        assert_eq!(refused("ok", String::new(), MAX_META_BYTES + 1), ReturnRefused::TooLarge);
        // Decoded bytes: 131,072 two-byte characters are exactly the limit.
        let edge = "é".repeat(MAX_CONTENT_BYTES / 2);
        assert!(overlay.is_pending(id), "refusals consume nothing");
        overlay.resolve(id, "ok", edge.clone(), MAX_META_BYTES, true).expect("accepted");
        let result = running.await.expect("joined").expect("taken");
        assert_eq!(result["content"], Value::String(edge), "whole, never cut");
        assert_eq!(result["_meta"][crate::runtime::memory_only::META_KEY], true);
        assert_eq!(overlay.resolve(id, "ok", String::new(), 0, false), Err(ReturnRefused::NotPending));
        assert_eq!(overlay.pending(), 0);
        // §8: the stamp is the registration and the validated code, once.
        let offer = overlay.offer();
        assert_eq!(
            offer.take_stamp("toolu_1"),
            Some(ClientToolStamp {
                registration_id: invoke.registration_id.clone(),
                invocation_id: Some(invoke.invocation_id.clone()),
                extension_id: "pria-desktop".into(),
                tool: "get".into(),
                family: "pria.vault".into(),
                outcome: "ok".into(),
                memory_only: true,
            })
        );
        assert_eq!(offer.take_stamp("toolu_1"), None, "taken once");
        assert_eq!(Offer::default().take_stamp("toolu_1"), None);
    }

    #[tokio::test]
    async fn args_timeout_and_busy_end_a_call_without_a_return() {
        let (overlay, mut rx, handler) = one_tool();
        lock(&overlay.shared).timeout = Duration::from_millis(50);
        // R2: one byte over the limit as compact JSON: no frame at all.
        let over = Value::String("x".repeat(MAX_ARGS_BYTES - 1));
        assert_eq!(compact_len(&over), MAX_ARGS_BYTES + 1);
        let text = call(&handler, over).await.expect("joined").expect_err("refused");
        assert_eq!(text, Ended::ArgsTooLarge.text(WIRE, "toolu_1", TIMEOUT));
        assert_eq!(
            text,
            "client tool pria-desktop_get refused: its arguments are over 1048576 bytes (call toolu_1). Send smaller arguments."
        );
        assert!(rx.try_recv().is_err(), "no frame");
        let stamp = overlay.offer().take_stamp("toolu_1").expect("stamped");
        assert_eq!((stamp.invocation_id, stamp.outcome.as_str()), (None, "synaps_args_too_large"));
        // At the limit it is sent; with no return it times out.
        let edge = Value::String("x".repeat(MAX_ARGS_BYTES - 2));
        let text = call(&handler, edge).await.expect("joined").expect_err("timed out");
        assert_eq!(text, Ended::Timeout.text(WIRE, "toolu_1", Duration::from_millis(50)));
        let invoke = invoked(&mut rx).await;
        let stamp = overlay.offer().take_stamp("toolu_1").expect("stamped");
        assert_eq!(stamp.invocation_id.as_deref(), Some(invoke.invocation_id.as_str()));
        assert_eq!(stamp.outcome, "synaps_timeout");
        match rx.try_recv() {
            Ok(Outbound::Cancel(cancel)) => assert_eq!(
                cancel,
                Cancel {
                    invocation_id: invoke.invocation_id.clone(),
                    registration_id: invoke.registration_id.clone(),
                    reason: CancelReason::Timeout,
                }
            ),
            other => panic!("expected tool_cancel, got {other:?}"),
        }
        assert_eq!(
            overlay.resolve(&invoke.invocation_id, "ok", String::new(), 0, false),
            Err(ReturnRefused::NotPending)
        );
        // Eight in flight: the ninth is refused before a frame.
        lock(&overlay.shared).timeout = TIMEOUT;
        let running: Vec<_> = (0..MAX_PENDING).map(|_| call(&handler, Value::Null)).collect();
        for _ in 0..MAX_PENDING {
            invoked(&mut rx).await;
        }
        let text = call(&handler, Value::Null).await.expect("joined").expect_err("busy");
        assert_eq!(text, Ended::Busy.text(WIRE, "toolu_1", TIMEOUT));
        assert!(rx.try_recv().is_err(), "no frame for the ninth");
        let stamp = overlay.offer().take_stamp("toolu_1").expect("stamped");
        assert_eq!((stamp.invocation_id, stamp.outcome.as_str()), (None, "synaps_busy"));
        // A detach ends them all at once and sends nothing.
        let dropped = overlay.drop_registration(DropReason::Detached);
        assert!(dropped.registration.is_some() && dropped.cancels.is_empty());
        for running in running {
            let text = running.await.expect("joined").expect_err("detached");
            assert_eq!(text, Ended::Detached.text(WIRE, "toolu_1", TIMEOUT));
        }
        assert_eq!(overlay.pending(), 0);
        assert!(rx.try_recv().is_err(), "a detach sends nothing");
    }
}
