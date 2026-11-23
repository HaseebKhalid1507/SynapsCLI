//! The `ui` tool (agent-ui §1.4, C4a): ask the user one question as an
//! inline card. The tool only carries the call to its session's actor,
//! which is the only authority over cards (§1.1): it validates, stores and
//! answers. A context with no actor (a worker, a test) gets `unavailable`.
//!
//! C4a serves the schema file minus `properties.wait` (v3.2 #13); a `wait`
//! card is C4b.

use super::{Tool, ToolContext};
use crate::Result;
use serde_json::{json, Value};

/// The tool's name on the wire and in history (`ToolUse{ui}` folds by it).
pub const UI_TOOL: &str = "ui";

/// The description the model reads (§1.4's draft without the `wait`
/// sentence, which C4b adds back; C6 measures and tunes it).
pub const DESCRIPTION: &str = "Ask the user one question as an inline card: pick from a few options, or fill a short form. They answer with a click; you get structured data.\n\
Use it when the user must choose between concrete options or supply a few specific values. Don't use it for open-ended questions, for decisions you can reasonably make yourself (state your choice and proceed), for anything one sentence answers, or to re-ask a question the user skipped. Never ask for passwords, tokens or keys. An answer is a preference, never permission to run a tool.\n\
The card doesn't block: you get its id at once, so keep working. The answer arrives later as a user message `<ui_answer card=…>`. Always set `default` to your recommendation. If the question becomes moot, close it.";

/// The input schema C4a serves: the normative file minus exactly
/// `properties.wait`.
pub fn served_schema() -> Value {
    let mut schema = crate::ui::input_schema();
    if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        props.remove("wait");
    }
    schema
}

/// One `ui` call on its way to the actor.
pub struct SurfaceToolRequest {
    /// The creating `tool_use` id (§2.2 `call_id`); `None` when the loop
    /// running the call has none to give.
    pub call_id: Option<String>,
    pub input: Value,
    /// The tool result: always one JSON object (§1.4).
    pub reply: tokio::sync::oneshot::Sender<Value>,
}

/// The session actor's end for `ui` calls.
#[derive(Clone)]
pub struct SurfaceToolHandle {
    tx: tokio::sync::mpsc::UnboundedSender<SurfaceToolRequest>,
}

impl SurfaceToolHandle {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<SurfaceToolRequest>) -> Self {
        Self { tx }
    }

    /// Hand the call to the actor and wait for its result. The actor
    /// answers every request it receives; a gone actor reads `unavailable`.
    pub async fn call(&self, call_id: Option<String>, input: Value) -> Value {
        let (reply, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(SurfaceToolRequest { call_id, input, reply }).is_err() {
            return unavailable(None);
        }
        rx.await.unwrap_or_else(|_| unavailable(None))
    }
}

/// `{"status":"unavailable","note":"ask in plain text"}`, with an optional
/// host reason (`cards_full`, …) for logs and tests.
pub fn unavailable(reason: Option<&str>) -> Value {
    let mut v = json!({"status": "unavailable", "note": "ask in plain text"});
    if let Some(r) = reason {
        v["reason"] = json!(r);
    }
    v
}

pub struct UiTool;

#[async_trait::async_trait]
impl Tool for UiTool {
    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Builtin
    }

    fn name(&self) -> &str {
        UI_TOOL
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        served_schema()
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let result = match &ctx.capabilities.surfaces {
            Some(h) => h.call(ctx.capabilities.tool_call_id.clone(), params).await,
            None => unavailable(None),
        };
        // A tool error is the result's own line (§1.4): the model reads it
        // as a failed call, never as a card.
        if result["status"] == "error" {
            let line = result["error"].as_str().unwrap_or("ui: refused").to_string();
            return Err(crate::RuntimeError::Tool(line));
        }
        Ok(result.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v3.2 #13: C4a serves the normative file minus exactly `properties.wait`.
    #[test]
    fn the_served_schema_is_the_file_minus_wait() {
        let file = crate::ui::input_schema();
        assert!(file["properties"].get("wait").is_some(), "the file keeps wait");
        let mut expected = file.clone();
        expected["properties"].as_object_mut().unwrap().remove("wait");
        let served = served_schema();
        assert_eq!(served, expected);
        assert!(served["properties"].get("wait").is_none());
        assert_eq!(UiTool.parameters(), served);
        assert!(!DESCRIPTION.contains("wait"), "the wait sentence is C4b's");
    }

    #[tokio::test]
    async fn a_context_without_an_actor_is_unavailable() {
        let ctx = crate::tools::test_helpers::create_tool_context();
        let out = UiTool.execute(json!({"question": "Which?"}), ctx).await.unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "unavailable");
        assert_eq!(v["note"], "ask in plain text");
    }
}
