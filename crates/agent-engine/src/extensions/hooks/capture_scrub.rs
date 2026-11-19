//! The capture scrub: the screen a hook payload passes before the bus hands
//! it to an extension that may store it.
//!
//! Core redacts its own history archive, but a capture or memory extension
//! is sent chat text, tool text and memory metadata by hooks. This is the
//! one place that screens those payloads, in core, before delivery. An
//! extension cannot reach it, turn it off or thin it out.
//!
//! # What is screened
//!
//! Capture-class hooks ([`is_capture_class`]): `after_tool_call`,
//! `before_message`, `on_message_complete`, `on_compaction`,
//! `on_session_end`, `on_context_archived` and `on_memory_forgotten`.
//! `before_tool_call` is a decision hook (a guard must judge the real
//! call, and a payload that cannot be screened must not become a missing
//! verdict), and `on_session_start` carries no text. The top-level `path`
//! of the two store notices (`on_context_archived`, `on_memory_forgotten`)
//! is a file path the host built: it gets every rule of the floor, with the
//! opaque-token rule applied per path component, not to the whole path.
//! Names and ids the host sets (`tool_name`, `tool_runtime_name`,
//! `session_id`, `cwd`) are not screened.
//!
//! Every string is screened as it is and, when it holds a backslash, once
//! more with each escape (`\n`, `\t`, `\uXXXX`, …) turned into a space: in
//! JSON a tool prints, the escape's letter sits right before a key and would
//! otherwise hide the key's prefix. An `after_tool_call` output cut to the
//! hook's size cap is also screened across the cut (the event's unsent
//! `cut_context`), so a key the cut split is not half delivered.
//!
//! # The floor
//!
//! A string is withheld when it holds any of:
//!
//! - what the shared credential screen
//!   ([`crate::runtime::trace::export::redact_text`]) would change: known
//!   secret prefixes (`sk-`, `sk-ant`, `hf_`, `ghp_`, `github_pat_`,
//!   `glpat-`, `AKIA`, …), private-key blocks, JWTs, credential-named
//!   values and credentials in URLs;
//! - a high-entropy token (an opaque key with no known prefix): 24+ letters
//!   and digits, both cases, 3.6+ bits per byte, mostly not words, not a
//!   hex id or hash;
//! - one of the extra prefixes the operator listed in config
//!   (`capture_scrub_prefixes`, [`CaptureScrub::with_extra_prefixes`]);
//! - a serialized content block that never belongs in a capture: a
//!   `"signature"` field or a `thinking`, `redacted_thinking`, `tool_use` or
//!   `tool_result` block.
//!
//! Extra prefixes only add. There is no switch, list entry or hook result
//! that removes a rule of the floor.
//!
//! # Drop, never redact
//!
//! A snippet that needed anything removed is dropped whole, not delivered
//! with a marker: the unit is a message (or one text block of it) of a
//! transcript, or the whole payload of a single-text hook. Only visible
//! text is delivered. A transcript message keeps only its text blocks
//! (never `thinking`, `redacted_thinking`, `tool_use`, `tool_result`,
//! attachments or any other block, nor a block's other fields), inline
//! `<think>…</think>` reasoning is cut out of every message, reply, summary
//! and tool output (an opening tag that is never closed hides the rest of
//! the text, later blocks of the message included), and host-injected
//! context (recall segments, compaction summaries, agent-event payloads,
//! tool-in-flight notes, and the blocks a `before_message` hook added to a
//! user message, with their `_synaps_hooked` count) is not delivered as if
//! it were chat: none of it is in a transcript. Compaction summaries are not
//! delivered as a `before_message`; event messages are, screened, and are
//! kept out of capture transcripts. An event message arrives as an event:
//! its text as the host wrote it (the `<event …>` header, its `source`
//! included), its inline reasoning cut out, and `data` set to
//! [`event_mark`], so a handler cannot take it for the owner's words.
//!
//! An `on_compaction` summary is model text written from the conversation.
//! When the summarizer was also fed the model's thinking (the default), the
//! summary can restate it, so the event is sent without it; it is sent only
//! when the host marks the summary's input as free of thinking
//! ([`COMPACTION_THINKING_EXCLUDED`]).
//!
//! # Fail closed
//!
//! A payload that cannot be screened is not delivered: no scrub stage on the
//! bus, a stage over its byte, node or time budget, a payload nested deeper
//! than the walk allows, a malformed archive id. The bus logs the hook, the
//! extension and the reason (never the content) and sends nothing.

use std::borrow::Cow;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::events::{HookEvent, HookKind};
use crate::runtime::trace::export::{key_is_secret, redact_text};

/// The `before_message` `data` key the screen sets on an event message
/// ([`event_mark`]).
pub const MESSAGE_KIND_KEY: &str = "message_kind";
/// Most extra prefixes one operator list may add.
pub const MAX_EXTRA_PREFIXES: usize = 64;
/// The `on_compaction` data flag the host sets to `true` when the
/// summarizer's input held no model thinking: the summary is delivered only
/// then.
pub const COMPACTION_THINKING_EXCLUDED: &str = "thinking_excluded";
/// Shortest and longest extra prefix, in bytes.
const EXTRA_PREFIX_BYTES: std::ops::RangeInclusive<usize> = 3..=64;
/// Token bytes that must follow an extra prefix for it to count.
const EXTRA_BODY_MIN: usize = 8;
/// Text one payload may hold, in bytes, before it is not screened at all.
pub const MAX_SCREEN_BYTES: usize = 8 * 1024 * 1024;
/// Time one payload may take to screen.
pub const SCREEN_DEADLINE: Duration = Duration::from_secs(1);
/// Deepest JSON nesting and most JSON nodes one payload may have.
const MAX_DEPTH: usize = 32;
const MAX_NODES: usize = 65_536;

/// Whether the bus screens `kind` before delivery. No wildcard arm: a new
/// hook kind does not compile until it has been classified here.
pub fn is_capture_class(kind: HookKind) -> bool {
    match kind {
        HookKind::AfterToolCall
        | HookKind::BeforeMessage
        | HookKind::OnMessageComplete
        | HookKind::OnCompaction
        | HookKind::OnSessionEnd
        | HookKind::OnContextArchived
        | HookKind::OnMemoryForgotten => true,
        // Never delivered by the bus: the core asks the policy provider
        // directly (`hook_value`). Screened if it ever were.
        HookKind::SandboxPolicy => true,
        HookKind::BeforeToolCall | HookKind::OnSessionStart => false,
    }
}

/// Whether the scrub stage works: the floor withholds a credential-named
/// value and a serialized thinking block, and lets plain text through. What
/// a daemon's `Welcome.features` announces ([`crate::session::wire::FEATURE_CAPTURE_SCRUB`]),
/// so it never claims a screen that does not screen. Cheap: a handful of
/// short strings.
pub fn is_active() -> bool {
    stage_works(&CaptureScrub::floor())
}

/// [`is_active`] for one stage: it withholds, and it delivers.
fn stage_works(stage: &CaptureScrub) -> bool {
    let event =
        |output: &str, input: Value| HookEvent::after_tool_call("probe", input, output.to_string());
    let withheld = |event: HookEvent| {
        matches!(
            stage.screen(&event),
            Err(Dropped::Secret | Dropped::RawBlock)
        )
    };
    withheld(event("ok", json!({"password": "probe-value-not-a-secret"})))
        && withheld(event("{\"type\": \"thinking\"}", Value::Null))
        && matches!(
            stage.screen(&event("plain text", json!({"path": "src/main.rs"}))),
            Ok(None)
        )
}

/// Why a payload was not delivered. Logged; never carries content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dropped {
    /// A credential shape, a high-entropy token or an extra prefix.
    Secret,
    /// A serialized thinking, signature or raw tool block.
    RawBlock,
    /// Over the byte, node or depth budget.
    Oversize,
    /// Over the time budget.
    Timeout,
    /// No scrub stage on the bus.
    Absent,
    /// A payload that is not what the hook sends.
    Malformed,
    /// A compaction summary where the user's own message would be.
    Injected,
}

impl Dropped {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Secret => "secret",
            Self::RawBlock => "raw_block",
            Self::Oversize => "oversize",
            Self::Timeout => "timeout",
            Self::Absent => "scrub_stage_absent",
            Self::Malformed => "malformed",
            Self::Injected => "injected_context",
        }
    }
}

impl std::fmt::Display for Dropped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The scrub stage: the floor, the operator's extra prefixes and the
/// budgets. `Debug` shows counts, never a prefix.
#[derive(Clone)]
pub struct CaptureScrub {
    extra_prefixes: Vec<String>,
    /// Operator entries left out as unusable ([`Self::ignored_prefix_count`]).
    ignored_prefixes: usize,
    max_bytes: usize,
    deadline: Duration,
}

impl std::fmt::Debug for CaptureScrub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureScrub")
            .field("extra_prefixes", &self.extra_prefixes.len())
            .field("ignored_prefixes", &self.ignored_prefixes)
            .finish()
    }
}

/// What one screening run has spent.
struct Run {
    started: Instant,
    deadline: Duration,
    bytes_left: usize,
    nodes: usize,
    /// The payload is a store notice (`on_context_archived`,
    /// `on_memory_forgotten`): its top-level `path` is a file path the host
    /// built, screened component by component ([`CaptureScrub::clean_path`]).
    path_field: bool,
}

impl Run {
    fn time(&self) -> Result<(), Dropped> {
        if self.started.elapsed() >= self.deadline {
            return Err(Dropped::Timeout);
        }
        Ok(())
    }

    fn text(&mut self, bytes: usize) -> Result<(), Dropped> {
        self.time()?;
        self.bytes_left = self
            .bytes_left
            .checked_sub(bytes)
            .ok_or(Dropped::Oversize)?;
        Ok(())
    }

    fn node(&mut self, depth: usize) -> Result<(), Dropped> {
        self.nodes += 1;
        if depth > MAX_DEPTH || self.nodes > MAX_NODES {
            return Err(Dropped::Oversize);
        }
        Ok(())
    }
}

impl CaptureScrub {
    /// The floor alone.
    pub fn floor() -> Self {
        Self {
            extra_prefixes: Vec::new(),
            ignored_prefixes: 0,
            max_bytes: MAX_SCREEN_BYTES,
            deadline: SCREEN_DEADLINE,
        }
    }

    /// Add the operator's extra credential prefixes. Add-only: the floor is
    /// not a list and nothing here can touch it. A blank entry or a repeat
    /// is skipped; an entry that is not 3-64 printable non-space ASCII
    /// bytes, or one past [`MAX_EXTRA_PREFIXES`], is ignored and counted
    /// ([`Self::ignored_prefix_count`]).
    pub fn with_extra_prefixes<I, S>(mut self, prefixes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for prefix in prefixes {
            let prefix = prefix.as_ref().trim();
            if prefix.is_empty() || self.extra_prefixes.iter().any(|known| known == prefix) {
                continue;
            }
            let usable = EXTRA_PREFIX_BYTES.contains(&prefix.len())
                && prefix.bytes().all(|b| b.is_ascii_graphic());
            if usable && self.extra_prefixes.len() < MAX_EXTRA_PREFIXES {
                self.extra_prefixes.push(prefix.to_string());
            } else {
                self.ignored_prefixes += 1;
            }
        }
        self
    }

    /// Extra prefixes in force.
    pub fn extra_prefix_count(&self) -> usize {
        self.extra_prefixes.len()
    }

    /// Operator entries ignored: too short or long, not printable ASCII, or
    /// past the cap. A count only; the entries are never kept or shown.
    pub fn ignored_prefix_count(&self) -> usize {
        self.ignored_prefixes
    }

    /// Smaller budgets, for tests of the fail-closed paths.
    #[cfg(test)]
    pub(crate) fn with_limits(mut self, max_bytes: usize, deadline: Duration) -> Self {
        self.max_bytes = max_bytes;
        self.deadline = deadline;
        self
    }

    fn run(&self) -> Run {
        Run {
            started: Instant::now(),
            deadline: self.deadline,
            bytes_left: self.max_bytes,
            nodes: 0,
            path_field: false,
        }
    }

    /// `event` as a handler may receive it: `Ok(None)` when it is fine as
    /// it is (or not a capture-class hook), `Ok(Some(copy))` when it is to be
    /// sent with its reasoning cut out, its transcript reduced to visible,
    /// clean text, its compaction summary withheld or its event message
    /// marked ([`event_mark`]), `Err` when it must not be sent at all.
    pub fn screen(&self, event: &HookEvent) -> Result<Option<HookEvent>, Dropped> {
        if !is_capture_class(event.kind) {
            return Ok(None);
        }
        let mut run = self.run();
        run.path_field = matches!(
            event.kind,
            HookKind::OnContextArchived | HookKind::OnMemoryForgotten
        );
        let mut screened: Option<HookEvent> = None;
        if let Some(input) = &event.tool_input {
            self.clean_value(&mut run, input, 0)?;
        }
        if let Some(output) = &event.tool_output {
            // A tool can return model text (a subagent's last reply): it
            // loses its inline reasoning like a reply does.
            let shown = strip_inline_reasoning(output);
            self.clean_text(&mut run, &shown)?;
            if let Cow::Owned(stripped) = shown {
                screened.get_or_insert_with(|| event.clone()).tool_output = Some(stripped);
            }
        }
        if let Some(context) = &event.cut_context {
            // Both sides of the output's cut as one piece: a credential the
            // cut split is withheld, not half delivered.
            self.clean_text(&mut run, context)?;
        }
        if let Some(message) = &event.message {
            if event.kind == HookKind::BeforeMessage && compaction_summary(message) {
                // A compaction summary in the user's place is not the
                // user's words; a transcript drops it too.
                return Err(Dropped::Injected);
            }
            let event_message = event.kind == HookKind::BeforeMessage && event_payload(message);
            if event.kind == HookKind::OnCompaction && !summary_input_excluded_thinking(event) {
                // The summarizer read the model's thinking: the summary may
                // restate it, so it is not sent.
                screened.get_or_insert_with(|| event.clone()).message = None;
            } else {
                // An event message passes the same screen as the user's
                // words: a credential withholds it whole, reasoning is cut.
                let text = strip_inline_reasoning(message);
                self.clean_text(&mut run, &text)?;
                if let Cow::Owned(stripped) = text {
                    screened.get_or_insert_with(|| event.clone()).message = Some(stripped);
                }
            }
            if event_message {
                // An agent event (`synaps send`, a worker's or a timer's
                // notice) is delivered as one, marked; its header is the
                // host's. A transcript still leaves it out.
                screened.get_or_insert_with(|| event.clone()).data = event_mark();
            }
        }
        self.clean_value(&mut run, &event.data, 0)?;
        if event.kind == HookKind::OnContextArchived {
            let id = event.data.get("archive_id").and_then(Value::as_str);
            // An archive id is 32 lowercase hex characters; anything else is
            // not what the host sends, and is not passed on.
            let archive_id = |id: &str| {
                id.len() == 32
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            };
            if !id.is_some_and(archive_id) {
                return Err(Dropped::Malformed);
            }
        }
        if let Some(transcript) = &event.transcript {
            let mut kept = Vec::with_capacity(transcript.len());
            for message in transcript {
                run.node(0)?;
                if let Some(message) = self.capture_message(&mut run, message)? {
                    kept.push(std::sync::Arc::new(message));
                }
            }
            screened.get_or_insert_with(|| event.clone()).transcript = Some(kept);
        }
        Ok(screened)
    }

    /// One transcript message reduced to its visible, clean text (without
    /// the blocks a `before_message` hook added), in the
    /// shape it had (a string stays a string, blocks stay text blocks), or
    /// `None` when nothing deliverable is left. A secret or raw block drops
    /// the snippet (the message, or one text block of it); reasoning left
    /// open by a block drops the blocks after it; only a spent budget is an
    /// `Err`.
    fn capture_message(&self, run: &mut Run, message: &Value) -> Result<Option<Value>, Dropped> {
        if message.get("_synaps_context").is_some()
            || crate::engine::interrupt::is_tool_in_flight_note(message)
            || !visible(message)
        {
            return Ok(None);
        }
        let Some(role) = message.get("role").and_then(Value::as_str) else {
            return Ok(None);
        };
        if !matches!(role, "user" | "assistant") {
            return Ok(None);
        }
        let mut reasoning_open = false;
        let mut snippet = |text: &str| -> Result<Option<String>, Dropped> {
            if reasoning_open || text.trim().is_empty() || injected_context(text) {
                return Ok(None);
            }
            let (text, open) = cut_inline_reasoning(text);
            reasoning_open = open;
            if text.trim().is_empty() {
                return Ok(None);
            }
            match self.clean_text(run, &text) {
                Ok(()) => Ok(Some(text.into_owned())),
                Err(Dropped::Secret | Dropped::RawBlock) => Ok(None),
                Err(spent) => Err(spent),
            }
        };
        // The user's own words only: the blocks a `before_message` hook
        // added (saved extension context and markers, told apart by the
        // host's `_synaps_hooked` count) are extension-supplied context and
        // never reach a capture, nor does the count (the output is built
        // from the role and the kept content alone).
        let content = match &*agent_core::core::extension_context::user_content(message) {
            Value::String(text) => snippet(text)?.map(Value::String),
            Value::Array(blocks) => {
                let mut kept = Vec::new();
                for block in blocks {
                    let is_text = block.get("type").and_then(Value::as_str) == Some("text");
                    if !(is_text && visible(block)) {
                        continue;
                    }
                    let Some(text) = block.get("text").and_then(Value::as_str) else {
                        continue;
                    };
                    if let Some(text) = snippet(text)? {
                        kept.push(json!({"type": "text", "text": text}));
                    }
                }
                (!kept.is_empty()).then_some(Value::Array(kept))
            }
            _ => None,
        };
        Ok(content.map(|content| json!({"role": role, "content": content})))
    }

    /// `Ok` when `text` holds nothing the floor withholds.
    fn clean_text(&self, run: &mut Run, text: &str) -> Result<(), Dropped> {
        run.text(text.len())?;
        if raw_block(text) {
            return Err(Dropped::RawBlock);
        }
        let unescaped = escapes_as_spaces(text);
        if std::iter::once(text)
            .chain(unescaped.as_deref())
            .any(|view| self.credential_shape(view) || contains_high_entropy(view))
        {
            return Err(Dropped::Secret);
        }
        // The screen ran, but too slowly to be trusted to have been fed all
        // of a payload this size: not delivered.
        run.time()
    }

    /// `Ok` when no string, and no key-named value, of `value` is withheld.
    fn clean_value(&self, run: &mut Run, value: &Value, depth: usize) -> Result<(), Dropped> {
        run.node(depth)?;
        match value {
            Value::String(text) => self.clean_text(run, text),
            Value::Array(items) => items
                .iter()
                .try_for_each(|item| self.clean_value(run, item, depth + 1)),
            Value::Object(map) => {
                for (key, entry) in map {
                    self.clean_text(run, key)?;
                    if key_is_secret(key) {
                        // A credential-named field may only hold nothing or
                        // the redaction marker (numbers and flags carry no
                        // credential: `max_tokens: 4096`).
                        if !holds_only_placeholders(entry, 0) {
                            return Err(Dropped::Secret);
                        }
                    } else if let (true, "path", Value::String(path)) =
                        (run.path_field && depth == 0, key.as_str(), entry)
                    {
                        self.clean_path(run, path)?;
                    } else {
                        self.clean_value(run, entry, depth + 1)?;
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// A file path of a store notice. The credential shapes, the extra
    /// prefixes and the raw-block test apply to the whole path; the opaque-
    /// token test applies to each component, because the path is one run of
    /// token bytes (`/` joins them) whose id and folder names would otherwise
    /// read as a single opaque key.
    fn clean_path(&self, run: &mut Run, path: &str) -> Result<(), Dropped> {
        run.text(path.len())?;
        if raw_block(path) {
            return Err(Dropped::RawBlock);
        }
        let unescaped = escapes_as_spaces(path);
        if std::iter::once(path)
            .chain(unescaped.as_deref())
            .any(|view| self.credential_shape(view))
            || path
                .split(['/', '\\'])
                .any(contains_high_entropy)
        {
            return Err(Dropped::Secret);
        }
        run.time()
    }

    /// A credential shape of the shared screen, or an extra prefix.
    fn credential_shape(&self, text: &str) -> bool {
        self.extra_prefix_hit(text) || redact_text(text) != text
    }

    /// An operator-listed prefix, at a token start, followed by a body.
    fn extra_prefix_hit(&self, text: &str) -> bool {
        let bytes = text.as_bytes();
        self.extra_prefixes.iter().any(|prefix| {
            text.match_indices(prefix.as_str()).any(|(at, _)| {
                let starts_token = at == 0 || !bytes[at - 1].is_ascii_alphanumeric();
                let body = bytes[at + prefix.len()..]
                    .iter()
                    .take_while(|b| b.is_ascii_alphanumeric() || matches!(**b, b'.' | b'_' | b'-'))
                    .count();
                starts_token && body >= EXTRA_BODY_MIN
            })
        })
    }
}

/// Whether every string under `value` is empty or the redaction marker.
/// Past [`MAX_DEPTH`] nothing is assumed: the value is not a placeholder.
fn holds_only_placeholders(value: &Value, depth: usize) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    match value {
        Value::String(text) => text.is_empty() || text == "[REDACTED]",
        Value::Array(items) => items
            .iter()
            .all(|item| holds_only_placeholders(item, depth + 1)),
        Value::Object(fields) => fields
            .values()
            .all(|field| holds_only_placeholders(field, depth + 1)),
        _ => true,
    }
}

/// A capture body holding a serialized content block that never belongs in
/// a capture: a `"signature"` field, a `thinking` / `redacted_thinking` /
/// `tool_use` / `tool_result` block type (quotes optionally escaped, any
/// spacing), or a bare `redacted_thinking` marker. Prose about function
/// signatures or tool results does not match.
fn raw_block(text: &str) -> bool {
    use std::sync::LazyLock;
    static RAW: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r#"redacted_thinking|\\?"signature\\?"\s*:|\\?"type\\?"\s*:\s*\\?"(?:thinking|tool_use|tool_result)\\?""#,
        )
        .expect("static raw block pattern")
    });
    RAW.is_match(text)
}

/// `text` with every backslash escape (`\n`, `\t`, `\r`, `\uXXXX`, `\xXX`,
/// `\"`, any run of backslashes) turned into a space, or `None` when it has
/// no backslash: the copy the screen also reads, where a key that followed
/// an escape starts a token of its own.
fn escapes_as_spaces(text: &str) -> Option<String> {
    use std::sync::LazyLock;
    static ESCAPE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"\\+(?:u[0-9A-Fa-f]{4}|x[0-9A-Fa-f]{2}|[nrtbfv0"'/])?"#)
            .expect("static escape pattern")
    });
    text.contains('\\')
        .then(|| ESCAPE.replace_all(text, " ").into_owned())
}

/// Whether the host marked an `on_compaction` summary as written from an
/// input with no model thinking ([`COMPACTION_THINKING_EXCLUDED`]).
fn summary_input_excluded_thinking(event: &HookEvent) -> bool {
    event.data.get(COMPACTION_THINKING_EXCLUDED) == Some(&Value::Bool(true))
}

/// [`cut_inline_reasoning`]'s text alone.
fn strip_inline_reasoning(text: &str) -> Cow<'_, str> {
    cut_inline_reasoning(text).0
}

/// Whether `text` holds an inline reasoning tag (`<think>`, `</thinking>`,
/// …) the screen would cut at.
pub(crate) fn holds_inline_reasoning(text: &str) -> bool {
    matches!(cut_inline_reasoning(text).0, Cow::Owned(_))
}

/// Inline reasoning some models emit in their visible text
/// (`<think>…</think>`, `<thinking>…</thinking>`, any case, a tag may carry
/// attributes) cut out. An opening tag that is never closed hides everything
/// after it; a closing tag with no opening one before it (the prompt
/// template opened the block) hides everything before it. The flag says the
/// reasoning is still open at the end, so a caller reading several blocks
/// drops the ones that follow.
fn cut_inline_reasoning(text: &str) -> (Cow<'_, str>, bool) {
    use std::sync::LazyLock;
    static TAG: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?i)<(/?)think(?:ing)?\b[^<>]{0,64}>")
            .expect("static inline reasoning pattern")
    });
    let mut kept = String::new();
    let mut visible_from = 0;
    let mut open = false;
    let mut cut = false;
    for tag in TAG.captures_iter(text) {
        let Some(whole) = tag.get(0) else {
            continue;
        };
        let closing = tag.get(1).is_some_and(|slash| !slash.as_str().is_empty());
        match (open, closing) {
            (false, false) => {
                kept.push_str(&text[visible_from..whole.start()]);
                open = true;
                cut = true;
            }
            (true, true) => {
                visible_from = whole.end();
                open = false;
            }
            // A nested opening tag: still inside.
            (true, false) => {}
            (false, true) => {
                kept.clear();
                visible_from = whole.end();
                cut = true;
            }
        }
    }
    if !cut {
        return (Cow::Borrowed(text), false);
    }
    if !open {
        kept.push_str(&text[visible_from..]);
    }
    (Cow::Owned(kept), open)
}

/// Host-injected context the session records as user-role text, never the
/// user's own words: a compaction summary or an agent-event payload. A
/// transcript leaves both out.
fn injected_context(text: &str) -> bool {
    compaction_summary(text) || event_payload(text)
}

/// A compaction summary: never delivered as a `before_message`.
fn compaction_summary(text: &str) -> bool {
    text.contains("<context-summary>")
}

/// An agent-event payload (`format_event_for_agent`): delivered as a
/// `before_message`, screened and marked ([`event_mark`]).
fn event_payload(text: &str) -> bool {
    crate::session::display::is_event_payload(text.trim())
}

/// `data` of a `before_message` whose `message` is an agent event (a
/// `synaps send` event, a worker's or a timer's notice), set by the screen:
/// its kind, and that the host vouches for no sender (not the owner, not the
/// `source` its header names).
pub fn event_mark() -> Value {
    json!({MESSAGE_KIND_KEY: "event", "verified": false})
}

/// A message or block that is not plain, model-visible, final text.
fn visible(value: &Value) -> bool {
    ["disclosure", "disclosure_class"].iter().all(|key| {
        value
            .get(key)
            .is_none_or(|v| v.as_str() == Some("model_visible"))
    }) && ["sensitivity", "retention", "retention_class"]
        .iter()
        .all(|key| {
            value
                .get(key)
                .is_none_or(|v| matches!(v.as_str(), Some("normal" | "standard" | "model_visible")))
        })
        && ["private", "restricted"]
            .iter()
            .all(|key| value.get(key).is_none_or(|v| v.as_bool() == Some(false)))
        && value
            .get("channel")
            .is_none_or(|v| v.as_str() == Some("final"))
        && value
            .get("content_class")
            .is_none_or(|v| matches!(v.as_str(), Some("user_text" | "assistant_text")))
}

/// Whether `text` holds a generic high-entropy token: an opaque credential
/// with no known prefix, which no precise rule can match.
///
/// A token is a run of `[A-Za-z0-9+/_-]`, outer `+/_-` trimmed. It counts
/// when it holds at least 24 ASCII letters/digits, both upper- and lower-case
/// letters, at least 3.6 bits of Shannon entropy per byte, and fewer than
/// 60% of its letters/digits in word-like runs (a capital plus three or more
/// lower-case letters, or three or more capitals). Hex ids and hashes (every
/// piece of 8+ bytes between `-`, `_`, `/`, `+` is hex) are kept, and so are
/// identifiers, paths and prose, which are mostly words. Credentials come
/// first: base64 blobs and random-looking ids count too.
fn contains_high_entropy(text: &str) -> bool {
    let token_byte = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'_' | b'-');
    let outer = |b: u8| matches!(b, b'+' | b'/' | b'_' | b'-');
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !token_byte(bytes[i]) {
            i += 1;
            continue;
        }
        let run = i;
        while i < bytes.len() && token_byte(bytes[i]) {
            i += 1;
        }
        let token = &bytes[run..i];
        let lead = token.iter().take_while(|&&b| outer(b)).count();
        let trail = token[lead..]
            .iter()
            .rev()
            .take_while(|&&b| outer(b))
            .count();
        if lead < token.len() && high_entropy(&token[lead..token.len() - trail]) {
            return true;
        }
    }
    false
}

fn high_entropy(token: &[u8]) -> bool {
    let alnum = token.iter().filter(|b| b.is_ascii_alphanumeric()).count();
    if alnum < 24
        || !token.iter().any(u8::is_ascii_uppercase)
        || !token.iter().any(u8::is_ascii_lowercase)
    {
        return false;
    }
    let mut pieces = token
        .split(|b| matches!(b, b'+' | b'/' | b'_' | b'-'))
        .filter(|piece| piece.len() >= 8)
        .peekable();
    if pieces.peek().is_some() && pieces.all(|piece| piece.iter().all(u8::is_ascii_hexdigit)) {
        return false;
    }
    if word_bytes(token) * 10 >= alnum * 6 {
        return false;
    }
    let mut counts = [0usize; 256];
    for &b in token {
        counts[usize::from(b)] += 1;
    }
    let len = token.len() as f64;
    let bits: f64 = counts
        .iter()
        .filter(|&&n| n > 0)
        .map(|&n| {
            let p = n as f64 / len;
            -p * p.log2()
        })
        .sum();
    bits >= 3.6
}

/// Bytes of `token` in word-like runs: a capital plus three or more
/// lower-case letters, or three or more capitals (an acronym; its last
/// capital starts the next word when lower-case letters follow).
fn word_bytes(token: &[u8]) -> usize {
    let lower_run = |from: usize| {
        token[from..]
            .iter()
            .take_while(|b| b.is_ascii_lowercase())
            .count()
    };
    let mut covered = 0;
    let mut i = 0;
    while i < token.len() {
        if token[i].is_ascii_lowercase() {
            let run = lower_run(i);
            if run >= 3 {
                covered += run;
            }
            i += run;
        } else if token[i].is_ascii_uppercase() {
            let caps = token[i..]
                .iter()
                .take_while(|b| b.is_ascii_uppercase())
                .count();
            let end = i + caps;
            let lower = lower_run(end);
            if lower == 0 {
                if caps >= 3 {
                    covered += caps;
                }
                i = end;
            } else {
                if caps > 3 {
                    covered += caps - 1;
                }
                if lower >= 3 {
                    covered += lower + 1;
                }
                i = end + lower;
            }
        } else {
            i += 1;
        }
    }
    covered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::trace::export::{alnum, credential_test_rows, shaped};
    use std::sync::Arc;

    fn tool(output: &str) -> HookEvent {
        HookEvent::after_tool_call("bash", json!({"command": "ls"}), output.to_string())
    }

    fn verdict(stage: &CaptureScrub, event: &HookEvent) -> Option<Dropped> {
        stage.screen(event).err()
    }

    /// An `on_compaction` event whose summary the host marks as written
    /// from an input with no thinking.
    fn summary(text: &str) -> HookEvent {
        HookEvent::on_compaction("a", "b", text, 3, json!({COMPACTION_THINKING_EXCLUDED: true}))
    }

    /// What a handler is sent of `event`: the screened copy, or the event.
    fn sent(stage: &CaptureScrub, event: &HookEvent) -> HookEvent {
        stage
            .screen(event)
            .expect("deliverable")
            .unwrap_or_else(|| event.clone())
    }

    /// What the stage delivers of `messages` as an `on_session_end` transcript.
    fn kept(stage: &CaptureScrub, messages: Vec<Value>) -> Vec<Value> {
        let event =
            HookEvent::on_session_end("chat", Some(messages.into_iter().map(Arc::new).collect()));
        let screened = stage
            .screen(&event)
            .expect("a clean-or-droppable transcript is screened")
            .expect("a transcript is always delivered as a copy");
        screened
            .transcript
            .expect("the copy keeps a transcript")
            .iter()
            .map(|message| (**message).clone())
            .collect()
    }

    #[test]
    fn a_stage_that_cannot_screen_is_not_active() {
        assert!(stage_works(&CaptureScrub::floor()));
        let no_bytes = CaptureScrub::floor().with_limits(0, SCREEN_DEADLINE);
        assert!(!stage_works(&no_bytes), "a stage that drops everything");
        let no_time = CaptureScrub::floor().with_limits(MAX_SCREEN_BYTES, Duration::ZERO);
        assert!(!stage_works(&no_time), "a stage that is out of time");
    }

    #[test]
    fn the_screen_is_active() {
        assert!(is_active());
    }

    #[test]
    fn every_text_bearing_hook_is_capture_class_and_the_decision_hooks_are_not() {
        for kind in HookKind::ALL {
            let expected = !matches!(kind, HookKind::BeforeToolCall | HookKind::OnSessionStart);
            assert_eq!(is_capture_class(*kind), expected, "{kind:?}");
        }
    }

    #[test]
    fn dropped_reasons_are_stable_log_words() {
        let words = [
            (Dropped::Secret, "secret"),
            (Dropped::RawBlock, "raw_block"),
            (Dropped::Oversize, "oversize"),
            (Dropped::Timeout, "timeout"),
            (Dropped::Absent, "scrub_stage_absent"),
            (Dropped::Malformed, "malformed"),
            (Dropped::Injected, "injected_context"),
        ];
        for (reason, word) in words {
            assert_eq!(reason.as_str(), word);
            assert_eq!(reason.to_string(), word);
        }
    }

    #[test]
    fn a_credential_shape_is_withheld_from_tool_input_output_message_and_data() {
        let stage = CaptureScrub::floor();
        for (text, canary) in credential_test_rows() {
            let events = [
                tool(&text),
                HookEvent::after_tool_call("bash", json!({"command": text}), "ok".into()),
                HookEvent::before_message(&text),
                HookEvent::on_message_complete("ok", json!({"note": text})),
                summary(&text),
                HookEvent::on_message_complete("ok", json!([[{"deep": text}]])),
            ];
            for event in &events {
                assert_eq!(
                    verdict(&stage, event),
                    Some(Dropped::Secret),
                    "{canary} in {event:?}"
                );
            }
        }
    }

    #[test]
    fn a_credential_named_value_is_withheld_unless_it_is_only_a_placeholder() {
        let stage = CaptureScrub::floor();
        let call = |input: Value| HookEvent::after_tool_call("http", input, "ok".into());
        for secret in [
            json!({"password": "correct horse"}),
            json!({"headers": {"Authorization": "opaque"}}),
            json!({"api_key": ["opaque"]}),
            json!({"x": {"client_secret": {"inner": "opaque"}}}),
        ] {
            assert_eq!(
                verdict(&stage, &call(secret.clone())),
                Some(Dropped::Secret),
                "{secret}"
            );
        }
        for fine in [
            json!({"password": "[REDACTED]"}),
            json!({"token": ""}),
            json!({"api_key": {"a": "[REDACTED]", "b": [""]}}),
            // A number or flag under a credential-sounding name is no credential.
            json!({"max_tokens": 4096, "refresh": false}),
            json!({"path": "src/main.rs", "count": 3}),
        ] {
            assert!(
                stage.screen(&call(fine.clone())).unwrap().is_none(),
                "{fine}"
            );
        }
        // Too deep to be sure it is only placeholders: not a placeholder.
        let mut deep = json!("");
        for _ in 0..(MAX_DEPTH + 4) {
            deep = json!([deep]);
        }
        assert_eq!(
            verdict(&stage, &call(json!({"token": deep}))),
            Some(Dropped::Secret)
        );
    }

    #[test]
    fn an_opaque_high_entropy_token_is_withheld_and_ordinary_text_is_not() {
        let stage = CaptureScrub::floor();
        let opaque = shaped(&alnum(), 40, 3);
        for text in [
            opaque.clone(),
            format!("the key is {opaque} ok"),
            format!("\"{opaque}\""),
            format!("/{opaque}/"),
        ] {
            assert_eq!(
                verdict(&stage, &tool(&text)),
                Some(Dropped::Secret),
                "{text}"
            );
        }
        for text in [
            "the quick brown fox jumps over the lazy dog",
            "getUserAccountPermissionsForOrganization",
            "A1B2C3D4e5f6a7b8C9D0e1f2A3B4c5d6",
            "123e4567-e89b-12d3-a456-426614174000",
            "9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c",
            "/usr/local/lib/python3.11/site-packages/numpy/core",
            "src/extensions/hooks/capture_scrub.rs",
            "set max_tokens: 4096 and password: String,",
        ] {
            assert!(
                stage.screen(&tool(text)).unwrap().is_none(),
                "over-withheld: {text}"
            );
        }
    }

    #[test]
    fn a_serialized_thinking_signature_or_tool_block_is_withheld() {
        let stage = CaptureScrub::floor();
        for text in [
            r#"{"type": "thinking", "thinking": "plan"}"#,
            r#"{"type":"redacted_thinking","data":"x"}"#,
            r#"{\"type\": \"tool_use\", \"name\": \"bash\"}"#,
            r#"{"type" : "tool_result", "content": "x"}"#,
            r#"{"text": "hi", "signature": "opaque"}"#,
            "redacted_thinking",
        ] {
            assert_eq!(
                verdict(&stage, &tool(text)),
                Some(Dropped::RawBlock),
                "{text}"
            );
            assert_eq!(
                verdict(&stage, &HookEvent::before_message(text)),
                Some(Dropped::RawBlock),
                "{text}"
            );
        }
        for prose in [
            "the function signature: fn main()",
            "the tool_result was empty and the tool_use was fine",
            "a thinking model",
        ] {
            assert!(stage.screen(&tool(prose)).unwrap().is_none(), "{prose}");
        }
    }

    #[test]
    fn inline_reasoning_is_cut_out_of_a_reply_and_a_summary() {
        let stage = CaptureScrub::floor();
        let reply = HookEvent::on_message_complete("<think>plan</think>done", json!({}));
        let screened = stage
            .screen(&reply)
            .unwrap()
            .expect("a copy without the reasoning");
        assert_eq!(screened.message.as_deref(), Some("done"));
        assert_eq!(
            reply.message.as_deref(),
            Some("<think>plan</think>done"),
            "the original is untouched"
        );

        let unclosed = HookEvent::on_message_complete("ok <Thinking>never closed", json!({}));
        let screened = stage.screen(&unclosed).unwrap().expect("cut");
        assert_eq!(screened.message.as_deref(), Some("ok "));

        let folded = summary("s<think>x</think>ummary");
        assert_eq!(
            stage.screen(&folded).unwrap().unwrap().message.as_deref(),
            Some("summary")
        );

        // A secret inside the reasoning goes with the reasoning.
        let hidden = format!("<think>token {}</think>done", shaped(&alnum(), 40, 5));
        let reply = HookEvent::on_message_complete(&hidden, json!({}));
        assert_eq!(
            stage.screen(&reply).unwrap().unwrap().message.as_deref(),
            Some("done")
        );
    }

    #[test]
    fn a_transcript_is_reduced_to_visible_clean_text() {
        let stage = CaptureScrub::floor();
        let leaked = format!("my key is sk-{}", shaped(&alnum(), 40, 6));
        let messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "private plan", "signature": "opaque"},
                {"type": "text", "text": "answer", "cache_control": {"type": "ephemeral"}},
                {"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "ls"}},
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "file list"},
            ]}),
            json!({"role": "user", "_synaps_context": {}, "content": "recalled note"}),
            json!({"role": "user", "content": "<context-summary>old chat</context-summary>"}),
            json!({"role": "user", "content": "<event id=1>ping</event>"}),
            json!({"role": "system", "content": "be brief"}),
            json!({"role": "user", "content": leaked}),
            json!({"role": "assistant", "content": "<think>plan</think>visible"}),
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "first"},
                {"type": "text", "text": leaked},
                {"type": "text", "text": "   "},
            ]}),
            json!({"content": "no role"}),
        ];
        assert_eq!(
            kept(&stage, messages),
            vec![
                json!({"role": "user", "content": "hello"}),
                json!({"role": "assistant", "content": [{"type": "text", "text": "answer"}]}),
                json!({"role": "assistant", "content": "visible"}),
                json!({"role": "assistant", "content": [{"type": "text", "text": "first"}]}),
            ]
        );
    }

    #[test]
    fn a_transcript_message_marked_private_or_not_final_is_not_delivered() {
        let stage = CaptureScrub::floor();
        let messages = vec![
            json!({"role": "user", "content": "kept"}),
            json!({"role": "user", "private": true, "content": "p"}),
            json!({"role": "user", "restricted": true, "content": "r"}),
            json!({"role": "assistant", "channel": "analysis", "content": "a"}),
            json!({"role": "assistant", "disclosure": "internal", "content": "d"}),
            json!({"role": "assistant", "sensitivity": "high", "content": "s"}),
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "shown"},
                {"type": "text", "text": "hidden", "disclosure_class": "operator_only"},
                {"type": "text", "text": "scratch", "content_class": "reasoning"},
            ]}),
        ];
        assert_eq!(
            kept(&stage, messages),
            vec![
                json!({"role": "user", "content": "kept"}),
                json!({"role": "assistant", "content": [{"type": "text", "text": "shown"}]}),
            ]
        );
    }

    #[test]
    fn a_transcript_holds_no_extension_supplied_context_and_no_count() {
        use agent_core::core::extension_context as ec;
        let stage = CaptureScrub::floor();
        let hooked = |content: Value, added: usize| {
            let mut message = json!({"role": "user", "content": content});
            ec::set_hooked(&mut message, added);
            message
        };
        let saved = ec::saved_block_text("notes", "a recalled fact");
        let marker = ec::marker_text("notes");
        let image = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA=="}});
        let messages = vec![
            // A prompt the hook added a saved block and a marker to.
            hooked(
                json!([{"type": "text", "text": "go"}, {"type": "text", "text": saved}, {"type": "text", "text": marker}]),
                2,
            ),
            json!({"role": "assistant", "content": "ok"}),
            // Nothing of the user's text is left: the message is not delivered.
            hooked(json!([image, {"type": "text", "text": saved}]), 1),
            // The count, not the text, tells them apart: the user's own
            // look-alike is the user's words.
            json!({"role": "user", "content": [{"type": "text", "text": "mine"}, {"type": "text", "text": saved}]}),
        ];
        let delivered = kept(&stage, messages);
        assert_eq!(
            delivered,
            vec![
                json!({"role": "user", "content": "go"}),
                json!({"role": "assistant", "content": "ok"}),
                json!({"role": "user", "content": [{"type": "text", "text": "mine"}, {"type": "text", "text": saved}]}),
            ]
        );
        for message in &delivered[..2] {
            let wire = serde_json::to_string(message).unwrap();
            assert!(!wire.contains("Extension context from"), "{wire}");
            assert!(!wire.contains("context was shown here"), "{wire}");
            assert!(!wire.contains(ec::HOOKED_KEY), "{wire}");
        }
    }

    #[test]
    fn a_clean_payload_of_every_capture_hook_is_delivered_as_it_is() {
        let stage = CaptureScrub::floor();
        let clean = [
            tool("total 3\nREADME.md\nsrc"),
            HookEvent::before_message("please fix the build"),
            HookEvent::on_message_complete(
                "done",
                json!({"content_block_count": 1, "has_tool_use": false}),
            ),
            HookEvent::on_compaction(
                "old",
                "new",
                "a short summary",
                12,
                json!({COMPACTION_THINKING_EXCLUDED: true}),
            ),
            HookEvent::on_session_end("chat", None),
            HookEvent::on_context_archived("chat", &"a1".repeat(16), None, "p0", 4, 4, 1),
            HookEvent::on_memory_forgotten(Some("chat"), "mem-1", "note", "p0", "repository"),
        ];
        for event in &clean {
            assert!(matches!(stage.screen(event), Ok(None)), "{event:?}");
        }
    }

    #[test]
    fn the_decision_hook_and_session_start_are_not_screened() {
        let stage = CaptureScrub::floor();
        let secret = format!("sk-{}", shaped(&alnum(), 40, 7));
        let guard = HookEvent::before_tool_call("bash", json!({"command": secret}));
        assert!(matches!(stage.screen(&guard), Ok(None)));
        let start = HookEvent::on_session_start("chat");
        assert!(matches!(stage.screen(&start), Ok(None)));
    }

    #[test]
    fn an_archive_id_that_is_not_32_lowercase_hex_is_not_delivered() {
        let stage = CaptureScrub::floor();
        let archived = |id: &str| HookEvent::on_context_archived("chat", id, None, "p0", 1, 1, 1);
        assert!(matches!(stage.screen(&archived(&"a".repeat(32))), Ok(None)));
        let bad_ids = [
            "a".to_string(),
            String::new(),
            "A".repeat(32),
            "g".repeat(32),
            "a".repeat(33),
        ];
        for bad in &bad_ids {
            assert_eq!(
                verdict(&stage, &archived(bad)),
                Some(Dropped::Malformed),
                "{bad:?}"
            );
        }
        let mut no_id = archived(&"a".repeat(32));
        no_id.data = json!({});
        assert_eq!(verdict(&stage, &no_id), Some(Dropped::Malformed));
    }

    #[test]
    fn the_path_of_a_store_notice_is_screened_per_component() {
        let stage = CaptureScrub::floor();
        let id = "a1".repeat(16);
        let archived = |path: &str| {
            HookEvent::on_context_archived(
                "chat",
                &id,
                Some(std::path::Path::new(path)),
                "p0",
                1,
                1,
                1,
            )
        };
        // A folder name with both cases joined to a hex id by `/` is no key.
        let usual = format!("/work/.tmpAbCdEf/Mixed_Case/s000001/{id}.json");
        assert!(matches!(stage.screen(&archived(&usual)), Ok(None)));
        // A credential in the path, or an opaque component, still is one.
        let leaked = format!("/work/sk-{}/{id}.json", shaped(&alnum(), 40, 3));
        assert_eq!(verdict(&stage, &archived(&leaked)), Some(Dropped::Secret));
        let opaque = format!("/work/{}/{id}.json", shaped(&alnum(), 40, 3));
        assert_eq!(verdict(&stage, &archived(&opaque)), Some(Dropped::Secret));
    }

    #[test]
    fn extra_prefixes_add_to_the_floor_and_never_replace_it() {
        let plain = CaptureScrub::floor();
        let stage = CaptureScrub::floor().with_extra_prefixes([
            "corp_tok_",
            " corp_tok_ ",
            "ab",
            "has space",
            "",
            "caf\u{e9}_",
        ]);
        assert_eq!(stage.extra_prefix_count(), 1);
        // Too short, a space, not ASCII; a blank entry and a repeat are not
        // counted.
        assert_eq!(stage.ignored_prefix_count(), 3);
        let hit = "use corp_tok_Ab12cd34 now";
        assert!(plain.screen(&tool(hit)).unwrap().is_none());
        assert_eq!(verdict(&stage, &tool(hit)), Some(Dropped::Secret));
        assert_eq!(
            verdict(&stage, &HookEvent::before_message(hit)),
            Some(Dropped::Secret)
        );
        // Not at a token start, or with no body behind it: not a credential.
        for miss in [
            "xcorp_tok_Ab12cd34ef",
            "corp_tok_ab",
            "the corp_tok_ prefix",
        ] {
            assert!(stage.screen(&tool(miss)).unwrap().is_none(), "{miss}");
        }
        // The floor is still there with no extras, and with any extras.
        let none = CaptureScrub::floor().with_extra_prefixes(Vec::<String>::new());
        let (text, _) = credential_test_rows().remove(0);
        assert_eq!(verdict(&none, &tool(&text)), Some(Dropped::Secret));
        assert_eq!(verdict(&stage, &tool(&text)), Some(Dropped::Secret));
    }

    #[test]
    fn extra_prefixes_are_capped_and_never_shown() {
        let many: Vec<String> = (0..(MAX_EXTRA_PREFIXES + 20))
            .map(|i| format!("pfx{i:03}_"))
            .collect();
        let stage = CaptureScrub::floor().with_extra_prefixes(&many);
        assert_eq!(stage.extra_prefix_count(), MAX_EXTRA_PREFIXES);
        assert_eq!(stage.ignored_prefix_count(), 20);
        let shown = format!("{stage:?}");
        assert!(!shown.contains("pfx"), "{shown}");
        assert!(shown.contains(&MAX_EXTRA_PREFIXES.to_string()), "{shown}");
    }

    #[test]
    fn a_payload_over_a_budget_is_not_delivered() {
        let big = CaptureScrub::floor().with_limits(20, Duration::from_secs(60));
        assert_eq!(
            verdict(&big, &tool(&"x".repeat(64))),
            Some(Dropped::Oversize)
        );
        assert!(big.screen(&tool("short")).is_ok());

        let late = CaptureScrub::floor().with_limits(MAX_SCREEN_BYTES, Duration::ZERO);
        assert_eq!(verdict(&late, &tool("short")), Some(Dropped::Timeout));

        let stage = CaptureScrub::floor();
        let mut deep = json!("x");
        for _ in 0..(MAX_DEPTH + 4) {
            deep = json!([deep]);
        }
        let nested = HookEvent::on_message_complete("m", deep);
        assert_eq!(verdict(&stage, &nested), Some(Dropped::Oversize));

        let wide =
            HookEvent::on_message_complete("m", Value::Array(vec![Value::Null; MAX_NODES + 1]));
        assert_eq!(verdict(&stage, &wide), Some(Dropped::Oversize));

        // The transcript spends the same node budget (messages with no
        // text are cheap to walk, so this is about the count, not the time).
        let many = vec![json!({"role": "user"}); MAX_NODES + 1];
        let event =
            HookEvent::on_session_end("chat", Some(many.into_iter().map(Arc::new).collect()));
        assert_eq!(verdict(&stage, &event), Some(Dropped::Oversize));
    }

    #[test]
    fn inline_reasoning_is_cut_out_of_tool_output_and_the_users_message() {
        let stage = CaptureScrub::floor();
        // A subagent's last reply, returned as the tool result.
        let reply = "<think>The user wants a list.</think>Here is the list";
        let result = tool(reply);
        assert_eq!(sent(&stage, &result).tool_output.as_deref(), Some("Here is the list"));
        assert_eq!(result.tool_output.as_deref(), Some(reply), "the original is untouched");
        // Never closed: the rest of the text goes, a secret in it too.
        let open = tool(&format!("partial <thinking>key sk-{} and more", shaped(&alnum(), 40, 2)));
        assert_eq!(sent(&stage, &open).tool_output.as_deref(), Some("partial "));
        // A closing tag with no opening one (the prompt template opened the
        // block): what came before it is reasoning.
        let half = tool("the plan is X</think>the answer");
        assert_eq!(sent(&stage, &half).tool_output.as_deref(), Some("the answer"));
        let tagged = tool("<think kind=\"plan\">p</think>ok");
        assert_eq!(sent(&stage, &tagged).tool_output.as_deref(), Some("ok"));

        for (text, shown) in [
            ("explain the <think> tag", "explain the "),
            ("<THINKING>a</THINKING>b", "b"),
            ("a</think>b", "b"),
        ] {
            let message = HookEvent::before_message(text);
            assert_eq!(sent(&stage, &message).message.as_deref(), Some(shown), "{text}");
        }
        // Words about thinking are not a tag.
        for text in ["I think so", "<thinker> is a name", "a < think > b"] {
            assert!(matches!(stage.screen(&tool(text)), Ok(None)), "{text}");
        }
    }

    #[test]
    fn reasoning_left_open_drops_the_rest_of_the_message() {
        let stage = CaptureScrub::floor();
        let messages = vec![
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "first<think>plan"},
                {"type": "text", "text": "more plan"},
                {"type": "text", "text": "end</think>after"},
            ]}),
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "<think>a"},
                {"type": "text", "text": "b</think>"},
            ]}),
            json!({"role": "user", "content": "<think>x</think>mine"}),
            json!({"role": "assistant", "content": "next message"}),
        ];
        assert_eq!(
            kept(&stage, messages),
            vec![
                json!({"role": "assistant", "content": [{"type": "text", "text": "first"}]}),
                json!({"role": "user", "content": "mine"}),
                json!({"role": "assistant", "content": "next message"}),
            ]
        );
    }

    #[test]
    fn a_compaction_summary_is_not_delivered_and_an_event_message_is_screened() {
        let stage = CaptureScrub::floor();
        let summary = HookEvent::before_message("<context-summary>the old chat</context-summary>");
        assert_eq!(verdict(&stage, &summary), Some(Dropped::Injected));
        // An event message arrives as an event: the host's header, the
        // kind marked, inline reasoning cut out.
        for (text, shown) in [
            (
                "<event source=\"subagent\" id=\"7\">Subagent 'w' finished. Preview: <think>plan</think>done</event>",
                "<event source=\"subagent\" id=\"7\">Subagent 'w' finished. Preview: done</event>",
            ),
            (
                "  <event source=\"timer\">ping</event>\n",
                "  <event source=\"timer\">ping</event>\n",
            ),
        ] {
            let copy = sent(&stage, &HookEvent::before_message(text));
            assert_eq!(copy.message.as_deref(), Some(shown), "{text}");
            assert_eq!(copy.data, event_mark(), "{text}");
            assert_eq!(copy.data[MESSAGE_KIND_KEY], "event");
            assert_eq!(copy.data["verified"], false);
        }
        // A credential in an event message withholds it whole, as it does
        // the user's words.
        let keyed = format!(
            "<event source=\"cli\">use sk-{}</event>",
            shaped(&alnum(), 40, 6)
        );
        assert_eq!(
            verdict(&stage, &HookEvent::before_message(&keyed)),
            Some(Dropped::Secret)
        );
        // Words about the envelope are the user's own: no mark.
        let mine = HookEvent::before_message("what is an <event> tag?");
        assert!(matches!(stage.screen(&mine), Ok(None)));
    }

    #[test]
    fn a_compaction_summary_is_sent_only_when_its_input_excluded_thinking() {
        let stage = CaptureScrub::floor();
        let leaked = format!("the plan was to use sk-{}", shaped(&alnum(), 40, 8));
        for data in [
            json!({}),
            json!({COMPACTION_THINKING_EXCLUDED: false}),
            json!({COMPACTION_THINKING_EXCLUDED: "true"}),
        ] {
            for text in ["a short summary", leaked.as_str()] {
                let event = HookEvent::on_compaction("a", "b", text, 3, data.clone());
                let copy = sent(&stage, &event);
                assert_eq!(copy.message, None, "{data}");
                assert_eq!(copy.data, event.data, "the rest is sent as it is");
            }
        }
        assert!(matches!(stage.screen(&summary("a short summary")), Ok(None)));
        assert_eq!(verdict(&stage, &summary(&leaked)), Some(Dropped::Secret));
    }

    #[test]
    fn an_escape_right_before_a_key_does_not_hide_it() {
        let floor = CaptureScrub::floor();
        let stage = CaptureScrub::floor().with_extra_prefixes(["corp_tok_"]);
        // A low-entropy key (one case, two characters): only its prefix
        // gives it away.
        let key = format!("ghp_{}", "a1".repeat(12));
        for escape in [r"\n", r"\t", r"\r", r"\u000a", r"\\n", r#"\""#] {
            let printed = format!(r#"{{"log": "ok{escape}{key}"}}"#);
            assert_eq!(verdict(&floor, &tool(&printed)), Some(Dropped::Secret), "{printed}");
            let operator = format!(r#"{{"log": "ok{escape}corp_tok_abcdefgh12"}}"#);
            assert_eq!(verdict(&stage, &tool(&operator)), Some(Dropped::Secret), "{operator}");
            let message = HookEvent::before_message(&operator);
            assert_eq!(verdict(&stage, &message), Some(Dropped::Secret), "{operator}");
        }
        // Escaped text with nothing behind the escapes is delivered.
        for text in [
            r#"{"log": "line one\nline two\ttabbed \u00e9 \"quoted\""}"#,
            r"C:\Users\dev\project\notes.txt",
        ] {
            assert!(matches!(stage.screen(&tool(text)), Ok(None)), "{text}");
        }
    }

    #[test]
    fn a_key_the_hook_output_cut_splits_is_withheld() {
        use crate::extensions::hooks::events::MAX_HOOK_OUTPUT;
        let stage = CaptureScrub::floor();
        let key = format!("sk-{}", shaped(&alnum(), 40, 9));
        // The cut leaves `KEY=` and the key's first 12 bytes: too short a
        // body for any rule on its own.
        let output = format!("{}KEY={key}\ndone", " ".repeat(MAX_HOOK_OUTPUT - 16));
        let event = HookEvent::after_tool_call("bash", json!({}), output);
        let shown = event.tool_output.as_deref().unwrap();
        assert!(shown.contains(&format!("KEY={}…[truncated", &key[..12])), "the cut splits the key");
        assert_eq!(verdict(&stage, &event), Some(Dropped::Secret));
        let wire = serde_json::to_value(&event).unwrap();
        assert!(wire.get("cut_context").is_none(), "the bytes around the cut are never sent");

        // A clean output cut in a word is still delivered as it is.
        let clean = format!("{}hello world\ndone", " ".repeat(MAX_HOOK_OUTPUT - 4));
        let event = HookEvent::after_tool_call("bash", json!({}), clean);
        assert!(event.cut_context.is_some());
        assert!(matches!(stage.screen(&event), Ok(None)));
    }
}
