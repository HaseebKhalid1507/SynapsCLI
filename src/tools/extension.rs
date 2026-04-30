use std::sync::Arc;

use serde_json::{json, Value};

use crate::{Result, RuntimeError, ToolContext};
use crate::extensions::runtime::ExtensionHandler;
use crate::extensions::runtime::process::RegisteredExtensionToolSpec;

pub struct ExtensionTool {
    runtime_name: String,
    description: String,
    input_schema: Value,
    handler: Arc<dyn ExtensionHandler>,
    tool_name: String,
}

impl ExtensionTool {
    pub fn new(plugin_id: &str, spec: RegisteredExtensionToolSpec, handler: Arc<dyn ExtensionHandler>) -> Self {
        Self {
            runtime_name: format!("{}:{}", plugin_id, spec.name),
            description: spec.description,
            input_schema: spec.input_schema,
            handler,
            tool_name: spec.name,
        }
    }
}

#[async_trait::async_trait]
impl crate::tools::Tool for ExtensionTool {
    fn name(&self) -> &str {
        &self.runtime_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.input_schema.clone()
    }

    async fn execute(&self, params: Value, _ctx: ToolContext) -> Result<String> {
        let request = crate::extensions::hooks::events::HookEvent {
            kind: crate::extensions::hooks::events::HookKind::BeforeToolCall,
            tool_name: Some(self.tool_name.clone()),
            tool_runtime_name: Some(self.runtime_name.clone()),
            tool_input: Some(json!({
                "name": self.tool_name,
                "input": params,
            })),
            tool_output: None,
            message: None,
            session_id: None,
            transcript: None,
            data: json!({"method": "tool.call"}),
        };
        let _ = self.handler.handle(&request).await;
        Err(RuntimeError::Tool(
            "extension tool execution is not wired yet".to_string(),
        ))
    }
}
