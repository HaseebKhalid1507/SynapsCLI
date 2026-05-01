use std::sync::Arc;

use serde_json::Value;

use crate::{Result, RuntimeError, ToolContext};
use crate::extensions::runtime::ExtensionHandler;
use crate::extensions::runtime::process::RegisteredExtensionToolSpec;

pub struct ExtensionTool {
    runtime_name: String,
    description: String,
    input_schema: Value,
    handler: Arc<dyn ExtensionHandler>,
    tool_name: String,
    plugin_id: String,
}

impl ExtensionTool {
    pub fn new(plugin_id: &str, spec: RegisteredExtensionToolSpec, handler: Arc<dyn ExtensionHandler>) -> Self {
        Self {
            runtime_name: format!("{}:{}", plugin_id, spec.name),
            description: spec.description,
            input_schema: spec.input_schema,
            handler,
            tool_name: spec.name,
            plugin_id: plugin_id.to_string(),
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
        let value = self
            .handler
            .call_tool(&self.tool_name, params)
            .await
            .map_err(RuntimeError::Tool)?;
        if let Some(text) = value.as_str() {
            Ok(text.to_string())
        } else if let Some(text) = value.get("content").and_then(Value::as_str) {
            Ok(text.to_string())
        } else {
            Ok(value.to_string())
        }
    }

    fn extension_id(&self) -> Option<&str> {
        Some(&self.plugin_id)
    }
}
