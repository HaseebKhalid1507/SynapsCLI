//! Agent cards (`synaps:card/1`): the pure data types and their bounds.
//!
//! `agent-ui/architecture.md` v3.2, Errata #12: persistence formats live in
//! core, so the card's serde data is here and the logic (validation,
//! `ui_sanitize`, the contract, answer parsing, the fold) stays in
//! `agent_engine::ui`, which re-exports everything below under its old
//! paths. C2 adds `SurfaceState`, `AnswerState`, `LogEntry`, `Tombstone`
//! and `LedgerRecord` here (with `Phase`, `WaitMode`, `Verb`, `Downgrade`,
//! `Delivery` and `Origin`); `session_surfaces` persists them.
//!
//! Nothing here validates a doc: a [`CardDoc`] read back from storage or
//! the wire must go through `agent_engine::ui::revalidate`.

use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The catalog major this module implements (`synaps:card/1`).
pub const CATALOG: u32 = 1;
/// The normalizer version folded into every contract (§5.4).
pub const NORMALIZER: u32 = 1;

// ── Bounds (§4.4, §5.1). Every one is tested at N and N+1. ───────────────
// Character bounds count Unicode scalar values of the raw string (after
// `\r\n` / `\r` become `\n`), as JSON Schema does.

/// A `ui` input, serialized compactly. Refused, never truncated.
pub const INPUT_MAX_BYTES: usize = 8 * 1024;
/// Nesting of a `ui` input, scalars counted: `{"options":[{"label":"x"}]}`
/// is 4.
pub const INPUT_MAX_DEPTH: usize = 4;
/// What normalizing can add to an input of at most [`INPUT_MAX_BYTES`]:
/// - `{"label":…}` around each of 9 string options: 9 × 10;
/// - `,"multi":false`: 14;
/// - a text field with no `type`, `value` or `optional`:
///   `"type":"text","value":"","optional":false,` is 42, 5 fields;
/// - a string default becoming `[…]` with `multi`: 2;
/// - a default typed in another case and stored in the label's spelling.
///   Case folding shrinks a character by at most 2 bytes (U+212A KELVIN
///   SIGN, 3 bytes, folds to `k`), so ≤ 2 × 60 for each of 9 labels.
///
/// Sanitizing never grows a string (it drops characters; `\t` and a folded
/// `\n` are 2 bytes in JSON and become a 1-byte space), nor does an empty
/// optional string becoming absent.
pub const NORMALIZE_GROWTH_MAX: usize = 9 * 10 + 14 + 5 * 42 + 2 + 9 * 2 * 60;
/// A normalized doc, serialized compactly: what an accepted input can
/// become. Create and `revalidate` both measure the doc against it, so a
/// doc that was accepted always revalidates (Shady #439 H1).
pub const DOC_MAX_BYTES: usize = INPUT_MAX_BYTES + NORMALIZE_GROWTH_MAX;
pub const QUESTION_MAX: usize = 120;
pub const DETAIL_MAX: usize = 1500;
pub const DETAIL_MAX_LINES: usize = 30;
pub const OPTIONS_MIN: usize = 2;
pub const OPTIONS_MAX: usize = 9;
pub const OPTION_LABEL_MAX: usize = 60;
pub const HINT_MAX: usize = 120;
pub const NOTE_PROMPT_MAX: usize = 80;
pub const DEFAULT_MAX: usize = 9;
pub const FIELDS_MIN: usize = 1;
pub const FIELDS_MAX: usize = 5;
/// `^[a-z][a-z0-9_]{0,23}$`
pub const FIELD_ID_MAX: usize = 24;
pub const FIELD_LABEL_MAX: usize = 60;
pub const PLACEHOLDER_MAX: usize = 80;
/// A `text` value: the agent's prefill and the user's answer.
pub const TEXT_MAX: usize = 200;
/// A `long` value: the agent's prefill and the user's answer.
pub const LONG_MAX: usize = 2000;
/// A note typed for a `note` option.
pub const NOTE_MIN: usize = 1;
pub const NOTE_MAX: usize = 500;
/// Answer values, serialized compactly (§3.3 #11).
pub const ANSWER_MAX_BYTES: usize = 8 * 1024;
/// Nesting of answer values, scalars counted: `{"fields":{"f":"x"}}` is 3.
pub const ANSWER_MAX_DEPTH: usize = 3;
/// A late answer's `<asked>` capsule, encoded (§3.6).
pub const CAPSULE_MAX_BYTES: usize = 1024;
/// The capsule's copy of the detail.
pub const CAPSULE_DETAIL_MAX: usize = 400;
/// `origin.task`: the excerpt of the message that started the card's turn.
pub const TASK_EXCERPT_MAX: usize = 80;

// ── Identifiers (v3.2 #11, §4.4). Out of bound at create → `unavailable`.

/// `SurfaceState.call_id`, the creating `tool_use` id: 1–128 bytes of
/// ASCII graphic ([`is_call_id`]).
pub const CALL_ID_MAX_BYTES: usize = 128;
/// A session id, `prepared.parent` and `moved_to.to`: ≤128 bytes of
/// `[A-Za-z0-9_-]` ([`is_session_id`]). Minted ids are 20 bytes.
pub const SESSION_ID_MAX_BYTES: usize = 128;
/// `origin.turn`: the `_synaps_turn` uuid ([`is_turn_id`]); `None` means
/// the card's answers count as late.
pub const TURN_ID_BYTES: usize = 36;
/// `AnswerState.by`, the roster label: ≤64 bytes after `ui_sanitize`, cut
/// with `…`, never refused (`agent_engine::ui::roster_label`).
pub const BY_MAX_BYTES: usize = 64;
/// A stored time (`raised_at`, `answer.at`, a tombstone's `at`), as JSON
/// writes it (quotes aside): the longest RFC 3339 the host writes,
/// `2026-11-23T05:42:00.123456789+00:00`. A shorter time that doesn't
/// parse still loads, and counts as oldest (v3.3 #3).
pub const TIME_MAX_BYTES: usize = 35;

// ── System budget (§4.4, R2-M3). C1 measures these on worst-case images;
// C2 (sidecar) and C3 (wire) enforce them. ────────────────────────────────

pub const SURFACE_STATE_MAX_BYTES: usize = 24 * 1024;
pub const ATTACHED_MAX_STATES: usize = 12;
pub const ATTACHED_MAX_BYTES: usize = 288 * 1024;
pub const LEDGER_MAX_RECORDS: usize = 16;
pub const LEDGER_MAX_VALUE_BYTES: usize = 64 * 1024;
pub const TOMBSTONES_MAX: usize = 128;
pub const TOMBSTONE_MAX_BYTES: usize = 1024;
pub const ACTION_LOG_MAX: usize = 5;
pub const SIDECAR_MAX_BYTES: usize = 512 * 1024;

/// `true` when `s` is `<prefix>` + 16 lowercase hex: a surface id (`s_`) or
/// an answer id (`a_`).
pub fn is_host_id(s: &str, prefix: &str) -> bool {
    s.strip_prefix(prefix).is_some_and(|hex| {
        hex.len() == 16 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// A `ui` call id within bounds: 1–128 bytes, every one ASCII graphic.
pub fn is_call_id(s: &str) -> bool {
    (1..=CALL_ID_MAX_BYTES).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_graphic())
}

/// A session id within bounds: 1–128 bytes of `[A-Za-z0-9_-]`.
pub fn is_session_id(s: &str) -> bool {
    (1..=SESSION_ID_MAX_BYTES).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A `_synaps_turn` id: a hyphenated lowercase uuid, 36 bytes.
pub fn is_turn_id(s: &str) -> bool {
    s.len() == TURN_ID_BYTES
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => matches!(b, b'0'..=b'9' | b'a'..=b'f'),
        })
}

/// Why a card's agent was stopped (`origin.stopped`, the rows of §3.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StopKind {
    User,
    System,
    Reload,
    Crash,
}

/// Whether a field is one line (question, labels, hints, placeholders, text
/// values) or many (`detail`, `long` values).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lines {
    Single,
    Multi,
}

// ── The doc (§5.1) ───────────────────────────────────────────────────────

/// One normalized question (§5). Exactly the contract's preimage; `wait` is
/// the call's mode, not part of the doc.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardDoc {
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<CardOption>,
    #[serde(default)]
    pub multi: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<DefaultChoice>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<CardField>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// The prompt of the free-text note this option asks for when picked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The agent's recommendation: one label, or (with `multi`) a list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DefaultChoice {
    One(String),
    Many(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardField {
    pub id: String,
    pub label: String,
    #[serde(rename = "type")]
    pub kind: FieldKind,
    /// The prefill: `""` / `false` when the agent gave none.
    pub value: FieldValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    pub optional: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldKind {
    Text,
    Long,
    Check,
}

impl FieldKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FieldKind::Text => "text",
            FieldKind::Long => "long",
            FieldKind::Check => "check",
        }
    }

    /// The bound on a `text` / `long` value; `None` for a check.
    pub fn max_chars(self) -> Option<usize> {
        match self {
            FieldKind::Text => Some(TEXT_MAX),
            FieldKind::Long => Some(LONG_MAX),
            FieldKind::Check => None,
        }
    }

    pub fn lines(self) -> Lines {
        if self == FieldKind::Long {
            Lines::Multi
        } else {
            Lines::Single
        }
    }
}

/// How a card is answered (§5.2), derived by the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// `options`, not `multi`, no `fields`: a click or digit answers at once.
    Quick,
    /// Anything else: a Send button.
    Form,
}

impl CardDoc {
    pub fn mode(&self) -> Mode {
        if !self.options.is_empty() && !self.multi && self.fields.is_empty() {
            Mode::Quick
        } else {
            Mode::Form
        }
    }

    /// The option with exactly this label.
    pub fn option(&self, label: &str) -> Option<&CardOption> {
        self.options.iter().find(|o| o.label == label)
    }
}

// ── Answers (§5.3) ───────────────────────────────────────────────────────

/// A field's value: a prefill in the doc, or the user's answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FieldValue {
    Text(String),
    Check(bool),
}

/// The chosen label(s).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Choice {
    One(String),
    Many(Vec<String>),
}

/// Exactly what the model gets (§5.3). A skip is the empty answer `{}`.
///
/// Serializes with its keys in a fixed order (`choice`, `note`, `fields`)
/// and its fields in the doc's order, whatever `serde_json`'s map type, so
/// the model reads what the user read. Deserializing keeps the order it
/// reads and refuses unknown or repeated keys and repeated field ids.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Answer {
    pub choice: Option<Choice>,
    pub note: Option<String>,
    /// Every field of the doc, in the doc's order.
    pub fields: Option<Vec<(String, FieldValue)>>,
}

/// What an accepted action was (§2.2 `AnswerStatus`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnswerStatus {
    Answered,
    Skipped,
    Revised,
}

impl AnswerStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AnswerStatus::Answered => "answered",
            AnswerStatus::Skipped => "skipped",
            AnswerStatus::Revised => "revised",
        }
    }
}

impl Answer {
    /// The answer of a skip.
    pub fn skip() -> Self {
        Self::default()
    }

    /// The labels chosen, in order.
    pub fn chosen(&self) -> Vec<&str> {
        match &self.choice {
            None => Vec::new(),
            Some(Choice::One(l)) => vec![l.as_str()],
            Some(Choice::Many(ls)) => ls.iter().map(String::as_str).collect(),
        }
    }

    /// Compact JSON, keys in answer order.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("an answer always serializes")
    }
}

impl Serialize for Answer {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let len = self.choice.is_some() as usize
            + self.note.is_some() as usize
            + self.fields.is_some() as usize;
        let mut m = s.serialize_map(Some(len))?;
        if let Some(c) = &self.choice {
            m.serialize_entry("choice", c)?;
        }
        if let Some(n) = &self.note {
            m.serialize_entry("note", n)?;
        }
        if let Some(f) = &self.fields {
            m.serialize_entry("fields", &OrderedFields(f))?;
        }
        m.end()
    }
}

struct OrderedFields<'a>(&'a [(String, FieldValue)]);

impl Serialize for OrderedFields<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in self.0 {
            m.serialize_entry(k, v)?;
        }
        m.end()
    }
}

impl<'de> Deserialize<'de> for Answer {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct AnswerVisitor;
        impl<'de> Visitor<'de> for AnswerVisitor {
            type Value = Answer;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an answer object {choice?, note?, fields?}")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Answer, A::Error> {
                let mut a = Answer::default();
                while let Some(k) = m.next_key::<String>()? {
                    let dup = match k.as_str() {
                        "choice" => a.choice.replace(m.next_value()?).is_some(),
                        "note" => a.note.replace(m.next_value()?).is_some(),
                        "fields" => a.fields.replace(m.next_value::<FieldList>()?.0).is_some(),
                        _ => {
                            return Err(de::Error::custom(
                                "unknown key; an answer has choice, note and fields",
                            ))
                        }
                    };
                    if dup {
                        return Err(de::Error::custom(format!("duplicate key {k}")));
                    }
                }
                Ok(a)
            }
        }
        d.deserialize_map(AnswerVisitor)
    }
}

struct FieldList(Vec<(String, FieldValue)>);

impl<'de> Deserialize<'de> for FieldList {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct FieldsVisitor;
        impl<'de> Visitor<'de> for FieldsVisitor {
            type Value = FieldList;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object of field values")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<FieldList, A::Error> {
                let mut out: Vec<(String, FieldValue)> = Vec::new();
                while let Some((k, v)) = m.next_entry::<String, FieldValue>()? {
                    if out.iter().any(|(id, _)| *id == k) {
                        return Err(de::Error::custom("duplicate field id"));
                    }
                    out.push((k, v));
                }
                Ok(FieldList(out))
            }
        }
        d.deserialize_map(FieldsVisitor)
    }
}

// ── Marks (§2.5) ─────────────────────────────────────────────────────────

/// The in-memory message key of a mark. Like every `_synaps_*` key it never
/// reaches a provider.
pub const MARK_KEY: &str = "_synaps_ui";

/// The host's mark on a message. History evidence only: the Digest carries
/// the daemon's fold result, not marks (v3.2 #8).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UiMark {
    /// On a `<ui_answer>` message: `{"l","card","answer","rev","status"}`.
    Answer(AnswerMark),
    /// On the user message carrying `ui` tool results:
    /// `{"l","results":[{"card","answer"|null}]}`.
    Results(ResultsMark),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnswerMark {
    /// The sidecar's lineage.
    pub l: String,
    pub card: String,
    pub answer: String,
    pub rev: u32,
    pub status: AnswerStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultsMark {
    pub l: String,
    pub results: Vec<ResultMark>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultMark {
    pub card: String,
    /// The answer a waited result delivered; `null` for `shown`, `pending`,
    /// `closed` and `unconfirmed`.
    pub answer: Option<String>,
}

impl UiMark {
    pub fn lineage(&self) -> &str {
        match self {
            UiMark::Answer(m) => &m.l,
            UiMark::Results(m) => &m.l,
        }
    }

    /// Well formed: a lineage, host ids, a revision in 1..=4.
    pub fn is_valid(&self) -> bool {
        let answer_ok = |a: &str| is_host_id(a, "a_");
        !self.lineage().is_empty()
            && match self {
                UiMark::Answer(m) => {
                    is_host_id(&m.card, "s_") && answer_ok(&m.answer) && (1..=4).contains(&m.rev)
                }
                UiMark::Results(m) => {
                    !m.results.is_empty()
                        && m.results.iter().all(|r| {
                            is_host_id(&r.card, "s_") && r.answer.as_deref().is_none_or(answer_ok)
                        })
                }
            }
    }
}

// ── Retained state (C2: §2.2, §3.7, §3.11) ─────────────────────────────
// The sidecar's and the wire's card state. `session_surfaces` stores it;
// nothing here is wired to a session yet.

/// What the user sees of a card (§3.1). `Unknown`: recovery couldn't tell
/// (a restored or foreign history, §3.11 step 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Open,
    Answered,
    Skipped,
    Withdrawn,
    Expired,
    Closed,
    Unknown,
}

impl Phase {
    /// Nothing more can be answered on it (a revisable receipt is not
    /// terminal: its `offers` say so).
    pub fn is_open(self) -> bool {
        self == Phase::Open
    }
}

/// Is a tool blocked on the card now (§3.1 mode)? Named `WaitMode`
/// because C1's [`Mode`] is the answer mode (quick or form).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WaitMode {
    Live,
    Waiting,
}

/// A client verb a card offers (§2.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verb {
    Answer,
    Skip,
    Revise,
}

impl Verb {
    /// Every verb: `offers` is a set of these.
    pub const ALL: [Verb; 3] = [Verb::Answer, Verb::Skip, Verb::Revise];
}

/// Why a waiting card became live (T13, T14, T17, T21).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Downgrade {
    Timeout,
    Abandoned,
    Reload,
    Crash,
}

/// An answer's delivery (§3.1), per ledger record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Queued,
    Handed,
    InContext,
    Recorded,
    Settled,
    Unknown,
}

/// Where a card came from (§2.2, §3.6, §3.9).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Origin {
    /// The `_synaps_turn` uuid of the turn that raised it; `None`: its
    /// answers count as late.
    pub turn: Option<String>,
    /// The first [`TASK_EXCERPT_MAX`] characters of the message that
    /// started that turn, persisted at create (R3-M1).
    pub task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<StopKind>,
}

/// One retained card (§2.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceState {
    /// `s_` + 16 hex.
    pub id: String,
    pub call_id: String,
    /// +1 on every change; clients upsert by `(id, rev)`.
    pub rev: u64,
    pub contract: String,
    pub catalog: u32,
    pub mode: WaitMode,
    pub phase: Phase,
    pub offers: Vec<Verb>,
    pub doc: CardDoc,
    pub origin: Origin,
    /// The session's stop epoch when this state was drawn.
    pub epoch: u32,
    pub resume: bool,
    /// Durably accepted action ids on this card (≤ [`ACTION_LOG_MAX`]).
    pub actions: Vec<String>,
    pub raised_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgraded: Option<Downgrade>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<AnswerState>,
}

/// A view of a card's latest ledger record (§2.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerState {
    pub rev: u32,
    pub action_id: String,
    pub answer_id: String,
    pub status: AnswerStatus,
    pub values: Answer,
    pub delivery: Delivery,
    pub wake: bool,
    pub by: String,
    pub at: String,
}

/// One durably accepted action (§3.7, v3.2 #10): its payload hash
/// (`hex128(sha256(verb, expect_rev, epoch, values))`) and the answer
/// revision it produced. Only accepted actions are logged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogEntry {
    pub action_id: String,
    pub hash: String,
    pub rev: u32,
}

/// What is left of a card that left the retained set (§3.7, §4.4): its id,
/// when, and its action log for retries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tombstone {
    pub id: String,
    pub at: String,
    pub log: Vec<LogEntry>,
}

/// One accepted answer, skip or revision, until it is settled (§3.11).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerRecord {
    pub answer_id: String,
    pub card: String,
    pub rev: u32,
    pub status: AnswerStatus,
    pub values: Answer,
    /// Consent to start a turn, for `epoch` only (§3.9).
    pub wake: bool,
    pub epoch: u32,
    pub delivery: Delivery,
    /// The sanitized question, kept for a late delivery (R3-M1).
    pub q: String,
    /// The built `<asked>` capsule (≤ [`CAPSULE_MAX_BYTES`]), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asked: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_ids() {
        assert!(is_host_id("s_3f2a91c0d4e2a871", "s_"));
        assert!(is_host_id("a_6c1d2e3f4a5b6c7d", "a_"));
        for bad in [
            "s_3f2a91c0d4e2a87",
            "s_3f2a91c0d4e2a8711",
            "s_3F2A91C0D4E2A871",
            "a_3f2a91c0d4e2a871",
            "s_3f2a91c0d4e2a87g",
        ] {
            assert!(!is_host_id(bad, "s_"), "{bad}");
        }
    }

    /// v3.2 #11: every identifier bound at N and N+1.
    #[test]
    fn identifier_bounds() {
        assert!(is_call_id(&"~".repeat(CALL_ID_MAX_BYTES)));
        assert!(!is_call_id(&"~".repeat(CALL_ID_MAX_BYTES + 1)));
        assert!(is_call_id("t") && !is_call_id(""));
        assert!(is_call_id("toolu_01AbC\"\\!"), "every ASCII graphic byte");
        for bad in ["a b", "a\tb", "a\u{7f}", "é", "a\u{200B}"] {
            assert!(!is_call_id(bad), "{bad:?}");
        }

        assert!(is_session_id(&"a".repeat(SESSION_ID_MAX_BYTES)));
        assert!(!is_session_id(&"a".repeat(SESSION_ID_MAX_BYTES + 1)));
        assert!(is_session_id("20261123-054200-a1b2") && is_session_id("A-z_0"));
        for bad in ["", "a.b", "a/b", "a b", "é", "..", "a:b"] {
            assert!(!is_session_id(bad), "{bad:?}");
        }

        let uuid = "3f2a91c0-d4e2-4a87-9b1f-0123456789ab";
        assert_eq!(uuid.len(), TURN_ID_BYTES);
        assert!(is_turn_id(uuid));
        for bad in [
            "3f2a91c0-d4e2-4a87-9b1f-0123456789a",
            "3f2a91c0-d4e2-4a87-9b1f-0123456789abc",
            "3F2A91C0-D4E2-4A87-9B1F-0123456789AB",
            "3f2a91c0d4e24a879b1f0123456789ab----",
            "",
        ] {
            assert!(!is_turn_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn stop_kinds_are_the_rows_of_3_9() {
        let all = [
            StopKind::User,
            StopKind::System,
            StopKind::Reload,
            StopKind::Crash,
        ];
        let names: Vec<String> = all
            .iter()
            .map(|k| {
                serde_json::to_value(k)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(names, ["user", "system", "reload", "crash"]);
        assert!(serde_json::from_str::<StopKind>("\"driver_revoke\"").is_err());
    }

    /// The growth bound adds up as documented.
    #[test]
    fn doc_max_is_the_input_cap_plus_normalizing() {
        assert_eq!(NORMALIZE_GROWTH_MAX, 1396);
        assert_eq!(DOC_MAX_BYTES, 9588);
        assert_eq!(r#"{"label":}"#.len(), 10);
        assert_eq!(r#","multi":false"#.len(), 14);
        assert_eq!(r#""type":"text","value":"","optional":false,"#.len(), 42);
        assert_eq!('\u{212A}'.len_utf8() - 'k'.len_utf8(), 2);
    }

    #[test]
    fn answers_refuse_repeated_keys_and_field_ids() {
        for t in [
            r#"{"choice":"a","choice":"b"}"#,
            r#"{"note":"a","note":"b"}"#,
            r#"{"fields":{},"fields":{}}"#,
            r#"{"fields":{"a":"x","a":"y"}}"#,
            r#"{"choice":"a","extra":1}"#,
        ] {
            assert!(serde_json::from_str::<Answer>(t).is_err(), "{t}");
        }
        let a: Answer = serde_json::from_str(r#"{"fields":{"b":"1","a":true}}"#).unwrap();
        assert_eq!(
            a.to_json(),
            r#"{"fields":{"b":"1","a":true}}"#,
            "read in its own order"
        );
    }
}
