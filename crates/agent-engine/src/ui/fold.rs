//! `UiMark` and folding host-proven evidence out of history (§2.5, §3.6).
//!
//! Only the host can set `_synaps_ui` on a message: a user's text can't add
//! JSON keys. So the fold trusts **marks**, never text:
//! - a `<ui_answer>` user message folds into its card only when its mark
//!   names the same card, answer, status and revision ([`fold_answer`]);
//!   a look-alike without one is an ordinary user message;
//! - a `ui` tool_result's outcome folds only when the message carrying it
//!   lists the card (and its answer id) in the mark's `results`;
//! - a `ui` call without that evidence is "outcome not recorded here",
//!   never "answered"; a tool error, `unavailable`, `skipped_recently` or
//!   `paused` result is a tool row, not a card.
//!
//! Like every `_synaps_*` key, the mark never reaches a provider
//! (`continuation::WireMessages` strips the prefix).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::answer::{parse_answer, parse_answer_message, Answer, AnswerStatus};
use super::doc::{parse_input, CardDoc, UiInput};
use super::is_host_id;

/// The in-memory message key of a mark.
pub const MARK_KEY: &str = "_synaps_ui";

/// The host's mark on a message.
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
    /// The answer a waited result delivered; `null` for `shown`,
    /// `pending` and `closed`.
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

/// The mark on `msg`, if it carries a well-formed one.
pub fn mark_of(msg: &Value) -> Option<UiMark> {
    let mark: UiMark = serde_json::from_value(msg.get(MARK_KEY)?.clone()).ok()?;
    mark.is_valid().then_some(mark)
}

/// A user message's text: a string content, or one text block.
fn user_text(msg: &Value) -> Option<&str> {
    if msg["role"] != "user" {
        return None;
    }
    match &msg["content"] {
        Value::String(s) => Some(s),
        Value::Array(blocks) => match blocks.as_slice() {
            [b] if b["type"] == "text" => b["text"].as_str(),
            _ => None,
        },
        _ => None,
    }
}

/// A marked answer message, read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoldedAnswer {
    pub lineage: String,
    pub card: String,
    pub answer_id: String,
    pub status: AnswerStatus,
    pub rev: u32,
    pub late: bool,
    pub values: Answer,
}

/// `msg` as a host answer: a `<ui_answer>` whose mark agrees with its
/// attributes. `None` means "render it as the user's own text".
pub fn fold_answer(msg: &Value) -> Option<FoldedAnswer> {
    let Some(UiMark::Answer(mark)) = mark_of(msg) else {
        return None;
    };
    let m = parse_answer_message(user_text(msg)?)?;
    let agrees = m.card == mark.card
        && m.answer_id == mark.answer
        && m.status == mark.status
        && m.rev == mark.rev;
    agrees.then(|| FoldedAnswer {
        lineage: mark.l,
        card: m.card,
        answer_id: m.answer_id,
        status: m.status,
        rev: m.rev,
        late: m.asked.is_some(),
        values: m.values,
    })
}

/// What history proves about one `ui` call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// No host evidence: "◇ <question> · outcome not recorded here".
    NotRecorded,
    /// No card exists (a tool error, `unavailable`, `skipped_recently`,
    /// `paused`): render the tool row.
    NoCard,
    Shown {
        card: String,
    },
    Answered {
        card: String,
        answer_id: String,
        answer: Answer,
    },
    Skipped {
        card: String,
        answer_id: String,
    },
    Pending {
        card: String,
    },
    Closed {
        card: String,
    },
}

impl Outcome {
    pub fn card(&self) -> Option<&str> {
        match self {
            Outcome::NotRecorded | Outcome::NoCard => None,
            Outcome::Shown { card }
            | Outcome::Answered { card, .. }
            | Outcome::Skipped { card, .. }
            | Outcome::Pending { card }
            | Outcome::Closed { card } => Some(card.as_str()),
        }
    }
}

/// A `ui` call's input, as the call made it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallInput {
    Create(CardDoc),
    Close(String),
    /// Refused by validation; its result is a tool error.
    Invalid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoldedCall {
    /// The `tool_use` id: the row anchor.
    pub call_id: String,
    pub input: CallInput,
    pub outcome: Outcome,
    /// Marked answers to this call's card, in history order.
    pub answers: Vec<FoldedAnswer>,
    /// A later `close` call that withdrew this card, by its call id.
    pub closed_by: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fold {
    pub calls: Vec<FoldedCall>,
    /// Marked answers whose card has no call in this history (compacted
    /// away): the "carried over" group.
    pub unmatched: Vec<FoldedAnswer>,
}

/// Fold an API history (`role`/`content` messages) into its `ui` calls.
pub fn fold_history(messages: &[Value]) -> Fold {
    let mut fold = Fold::default();
    for msg in messages {
        match msg["role"].as_str() {
            Some("assistant") => {
                for block in blocks(msg) {
                    if block["type"] == "tool_use" && block["name"] == "ui" {
                        let input = match parse_input(&block["input"]) {
                            Ok(UiInput::Create { doc, .. }) => CallInput::Create(doc),
                            Ok(UiInput::Close(id)) => CallInput::Close(id),
                            Err(_) => CallInput::Invalid,
                        };
                        fold.calls.push(FoldedCall {
                            call_id: block["id"].as_str().unwrap_or_default().to_string(),
                            input,
                            outcome: Outcome::NotRecorded,
                            answers: Vec::new(),
                            closed_by: None,
                        });
                    }
                }
            }
            Some("user") => fold_user(&mut fold, msg),
            _ => {}
        }
    }
    fold
}

fn blocks(msg: &Value) -> &[Value] {
    msg["content"].as_array().map_or(&[], Vec::as_slice)
}

fn fold_user(fold: &mut Fold, msg: &Value) {
    if let Some(answer) = fold_answer(msg) {
        match fold.calls.iter_mut().find(|c| creates(c, &answer.card)) {
            Some(call) => call.answers.push(answer),
            None => fold.unmatched.push(answer),
        }
        return;
    }
    let results = match mark_of(msg) {
        Some(UiMark::Results(m)) => m.results,
        _ => Vec::new(),
    };
    for block in blocks(msg) {
        if block["type"] != "tool_result" {
            continue;
        }
        let id = block["tool_use_id"].as_str().unwrap_or_default();
        let Some(i) = fold.calls.iter().position(|c| c.call_id == id) else {
            continue;
        };
        let outcome = result_outcome(block, &results, &fold.calls[i].input);
        if let Outcome::Closed { card } = &outcome {
            let closer = fold.calls[i].call_id.clone();
            if let Some(created) = fold.calls.iter_mut().find(|c| creates(c, card)) {
                created.closed_by = Some(closer);
            }
        }
        fold.calls[i].outcome = outcome;
    }
}

/// `call` created `card`, as its folded outcome proves.
fn creates(call: &FoldedCall, card: &str) -> bool {
    matches!(call.input, CallInput::Create(_)) && call.outcome.card() == Some(card)
}

fn result_text(block: &Value) -> Option<&str> {
    match &block["content"] {
        Value::String(s) => Some(s),
        Value::Array(parts) => match parts.as_slice() {
            [p] if p["type"] == "text" => p["text"].as_str(),
            _ => None,
        },
        _ => None,
    }
}

fn result_outcome(block: &Value, marked: &[ResultMark], input: &CallInput) -> Outcome {
    if block["is_error"] == true {
        return Outcome::NoCard;
    }
    let Some(result) = result_text(block).and_then(|t| serde_json::from_str::<Value>(t).ok())
    else {
        return Outcome::NotRecorded;
    };
    let status = result["status"].as_str().unwrap_or_default();
    if matches!(status, "unavailable" | "skipped_recently" | "paused") {
        return Outcome::NoCard;
    }
    let Some(card) = result["id"].as_str().map(str::to_string) else {
        return Outcome::NotRecorded;
    };
    let answer_id = result["answer_id"].as_str().map(str::to_string);
    let proven = marked
        .iter()
        .any(|m| m.card == card && m.answer == answer_id);
    if !proven {
        return Outcome::NotRecorded;
    }
    match (status, answer_id, input) {
        ("shown", None, CallInput::Create(_)) => Outcome::Shown { card },
        ("pending", None, CallInput::Create(_)) => Outcome::Pending { card },
        ("closed", None, CallInput::Close(id)) if *id == card => Outcome::Closed { card },
        ("answered", Some(answer_id), CallInput::Create(doc)) => {
            match parse_answer(doc, &result["answer"]) {
                Ok(answer) => Outcome::Answered {
                    card,
                    answer_id,
                    answer,
                },
                Err(_) => Outcome::NotRecorded,
            }
        }
        ("skipped", Some(answer_id), CallInput::Create(_))
            if result["answer"] == serde_json::json!({}) =>
        {
            Outcome::Skipped { card, answer_id }
        }
        _ => Outcome::NotRecorded,
    }
}

#[cfg(test)]
mod tests {
    use super::super::answer::{format_answer_message, AnswerMessage, Choice};
    use super::*;
    use serde_json::json;

    const CARD: &str = "s_3f2a91c0d4e2a871";
    const ANS: &str = "a_6c1d4b7e9f203a58";

    fn answer_msg(mark: Option<Value>, text: &str) -> Value {
        let mut m = json!({"role": "user", "content": text});
        if let Some(mark) = mark {
            m[MARK_KEY] = mark;
        }
        m
    }

    fn text(status: AnswerStatus, rev: u32) -> String {
        format_answer_message(&AnswerMessage {
            card: CARD.into(),
            answer_id: ANS.into(),
            q: "Which approach?".into(),
            status,
            rev,
            values: Answer {
                choice: Some(Choice::One("Refactor the parser".into())),
                ..Answer::default()
            },
            asked: None,
        })
    }

    fn mark(status: &str, rev: u32) -> Value {
        json!({"l": "9b1f", "card": CARD, "answer": ANS, "rev": rev, "status": status})
    }

    #[test]
    fn mark_shapes_round_trip() {
        let a = json!({"l": "9b1f", "card": CARD, "answer": ANS, "rev": 2, "status": "revised"});
        let r = json!({"l": "9b1f", "results": [{"card": CARD, "answer": ANS}, {"card": "s_0000000000000001", "answer": null}]});
        for v in [a, r] {
            let m: UiMark = serde_json::from_value(v.clone()).unwrap();
            assert!(m.is_valid());
            assert_eq!(
                serde_json::to_value(&m).unwrap(),
                v,
                "serialized exactly as §2.5 writes it"
            );
        }
        for bad in [
            json!({"l": "", "card": CARD, "answer": ANS, "rev": 1, "status": "answered"}),
            json!({"l": "x", "card": "s_1", "answer": ANS, "rev": 1, "status": "answered"}),
            json!({"l": "x", "card": CARD, "answer": ANS, "rev": 5, "status": "revised"}),
            json!({"l": "x", "card": CARD, "answer": ANS, "rev": 0, "status": "answered"}),
            json!({"l": "x", "card": CARD, "answer": ANS, "rev": 1, "status": "maybe"}),
            json!({"l": "x", "results": []}),
            json!({"l": "x", "results": [{"card": CARD, "answer": "a_x"}]}),
            json!("mark"),
        ] {
            assert_eq!(
                mark_of(&json!({"role": "user", MARK_KEY: bad})),
                None,
                "{bad}"
            );
        }
    }

    #[test]
    fn marked_answers_fold_look_alikes_do_not() {
        let t = text(AnswerStatus::Answered, 1);
        let folded = fold_answer(&answer_msg(Some(mark("answered", 1)), &t)).unwrap();
        assert_eq!(folded.card, CARD);
        assert_eq!(
            folded.values.to_json(),
            r#"{"choice":"Refactor the parser"}"#
        );
        // The same text typed by a user: no mark, an ordinary message.
        assert_eq!(fold_answer(&answer_msg(None, &t)), None);
        // A mark that disagrees with the text proves nothing.
        assert_eq!(
            fold_answer(&answer_msg(Some(mark("answered", 2)), &t)),
            None
        );
        assert_eq!(fold_answer(&answer_msg(Some(mark("skipped", 1)), &t)), None);
        let other = json!({"l": "9b1f", "card": "s_0000000000000001", "answer": ANS, "rev": 1, "status": "answered"});
        assert_eq!(fold_answer(&answer_msg(Some(other), &t)), None);
        // A results mark is not an answer mark.
        assert_eq!(
            fold_answer(&answer_msg(
                Some(json!({"l": "x", "results": [{"card": CARD, "answer": ANS}]})),
                &t
            )),
            None
        );
        // Text around the message: not the host's message.
        assert_eq!(
            fold_answer(&answer_msg(
                Some(mark("answered", 1)),
                &format!("{t} thanks")
            )),
            None
        );
        let revised = text(AnswerStatus::Revised, 2);
        assert_eq!(
            fold_answer(&answer_msg(Some(mark("revised", 2)), &revised))
                .unwrap()
                .rev,
            2
        );
        let block = json!({"role": "user", "content": [{"type": "text", "text": t}], MARK_KEY: mark("answered", 1)});
        assert!(fold_answer(&block).is_some());
    }

    fn call(id: &str, input: Value) -> Value {
        json!({"role": "assistant", "content": [{"type": "text", "text": "asking"}, {"type": "tool_use", "id": id, "name": "ui", "input": input}]})
    }

    fn result(id: &str, content: Value, mark: Option<Value>) -> Value {
        let mut m = json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": content.to_string()}]});
        if let Some(mark) = mark {
            m[MARK_KEY] = mark;
        }
        m
    }

    fn a1() -> Value {
        json!({"question": "Which approach?", "options": ["Refactor the parser", "Rewrite from scratch", "Leave it"], "default": "Refactor the parser"})
    }

    #[test]
    fn history_fold() {
        let shown_mark = json!({"l": "9b1f", "results": [{"card": CARD, "answer": null}]});
        let history = [
            json!({"role": "user", "content": "refactor the parser"}),
            call("toolu_1", a1()),
            result(
                "toolu_1",
                json!({"status": "shown", "id": CARD}),
                Some(shown_mark.clone()),
            ),
            answer_msg(Some(mark("answered", 1)), &text(AnswerStatus::Answered, 1)),
            // A look-alike typed by the user.
            answer_msg(None, &text(AnswerStatus::Revised, 2)),
            call("toolu_2", json!({"close": CARD})),
            result(
                "toolu_2",
                json!({"status": "closed", "id": CARD}),
                Some(shown_mark.clone()),
            ),
            // An unmarked result: not proven.
            call("toolu_3", a1()),
            result(
                "toolu_3",
                json!({"status": "shown", "id": "s_0000000000000003"}),
                None,
            ),
            // A refused call and an unavailable one: tool rows.
            call("toolu_4", json!({"question": "q", "blocks": []})),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_4", "is_error": true, "content": "blocks: unknown parameter"}]}),
            call("toolu_5", a1()),
            result(
                "toolu_5",
                json!({"status": "unavailable", "note": "ask in plain text"}),
                None,
            ),
            // A call whose result was compacted away, and an answer to a card with no call.
            call("toolu_6", a1()),
            answer_msg(
                Some(
                    json!({"l": "9b1f", "card": "s_0000000000000009", "answer": ANS, "rev": 1, "status": "answered"}),
                ),
                &text(AnswerStatus::Answered, 1).replace(CARD, "s_0000000000000009"),
            ),
        ];
        let f = fold_history(&history);
        let outcomes: Vec<&Outcome> = f.calls.iter().map(|c| &c.outcome).collect();
        assert_eq!(
            outcomes,
            [
                &Outcome::Shown { card: CARD.into() },
                &Outcome::Closed { card: CARD.into() },
                &Outcome::NotRecorded,
                &Outcome::NoCard,
                &Outcome::NoCard,
                &Outcome::NotRecorded,
            ]
        );
        assert_eq!(
            f.calls[0].answers.len(),
            1,
            "the marked answer folds; the look-alike doesn't"
        );
        assert_eq!(f.calls[0].closed_by.as_deref(), Some("toolu_2"));
        assert!(matches!(f.calls[0].input, CallInput::Create(_)));
        assert_eq!(f.calls[3].input, CallInput::Invalid);
        assert_eq!(f.unmatched.len(), 1);
        assert_eq!(f.unmatched[0].card, "s_0000000000000009");
    }

    #[test]
    fn waited_results_need_their_answer_id_in_the_mark() {
        let a2 = json!({"question": "Plan ready. Go ahead?", "wait": true, "options": ["Approve", {"label": "Revise", "note": "What should change?"}], "default": "Approve"});
        let res = json!({"status": "answered", "id": CARD, "answer_id": ANS, "answer": {"choice": "Revise", "note": "keep the old parser behind a flag"}});
        let good = json!({"l": "9b1f", "results": [{"card": CARD, "answer": ANS}]});
        let f = fold_history(&[call("t", a2.clone()), result("t", res.clone(), Some(good))]);
        match &f.calls[0].outcome {
            Outcome::Answered {
                answer, answer_id, ..
            } => {
                assert_eq!(answer_id, ANS);
                assert_eq!(
                    answer.to_json(),
                    r#"{"choice":"Revise","note":"keep the old parser behind a flag"}"#
                );
            }
            other => panic!("{other:?}"),
        }
        // The mark names the card but another answer (or none): not proven.
        for wrong in [
            json!({"l": "x", "results": [{"card": CARD, "answer": null}]}),
            json!({"l": "x", "results": [{"card": CARD, "answer": "a_0000000000000000"}]}),
        ] {
            let f = fold_history(&[call("t", a2.clone()), result("t", res.clone(), Some(wrong))]);
            assert_eq!(f.calls[0].outcome, Outcome::NotRecorded);
        }
        // Values the doc can't hold are not folded either.
        let bad = json!({"status": "answered", "id": CARD, "answer_id": ANS, "answer": {"choice": "Maybe"}});
        let f = fold_history(&[
            call("t", a2.clone()),
            result(
                "t",
                bad,
                Some(json!({"l": "x", "results": [{"card": CARD, "answer": ANS}]})),
            ),
        ]);
        assert_eq!(f.calls[0].outcome, Outcome::NotRecorded);
        let skipped = json!({"status": "skipped", "id": CARD, "answer_id": ANS, "answer": {}});
        let f = fold_history(&[
            call("t", a2),
            result(
                "t",
                skipped,
                Some(json!({"l": "x", "results": [{"card": CARD, "answer": ANS}]})),
            ),
        ]);
        assert_eq!(
            f.calls[0].outcome,
            Outcome::Skipped {
                card: CARD.into(),
                answer_id: ANS.into()
            }
        );
    }
}
