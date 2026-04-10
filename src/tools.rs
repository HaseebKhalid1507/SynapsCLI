use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;
use crate::{Result, RuntimeError};

#[derive(Debug, Clone)]
pub enum ToolType {
    Bash,
    Read,
    Write,
    Search,
}

impl ToolType {
    pub fn name(&self) -> &str {
        match self {
            ToolType::Bash => "bash",
            ToolType::Read => "read", 
            ToolType::Write => "write",
            ToolType::Search => "search",
        }
    }
    
    pub fn description(&self) -> &str {
        match self {
            ToolType::Bash => "Execute bash commands (use carefully)",
            ToolType::Read => "Read file contents",
            ToolType::Write => "Write content to file", 
            ToolType::Search => "Search knowledge base using VelociRAG",
        }
    }
    
    pub fn parameters(&self) -> Value {
        match self {
            ToolType::Bash => json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Bash command to execute"
                    }
                },
                "required": ["command"]
            }),
            ToolType::Read => json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to file to read"
                    }
                },
                "required": ["path"]
            }),
            ToolType::Write => json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to file to write"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to file"
                    }
                },
                "required": ["path", "content"]
            }),
            ToolType::Search => json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string", 
                        "description": "Search query for knowledge base"
                    }
                },
                "required": ["query"]
            }),
        }
    }
    
    pub async fn execute(&self, params: Value) -> Result<String> {
        match self {
            ToolType::Bash => execute_bash(params).await,
            ToolType::Read => execute_read(params).await,
            ToolType::Write => execute_write(params).await,
            ToolType::Search => execute_search(params).await,
        }
    }
}

#[derive(Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, ToolType>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        let mut registry = ToolRegistry {
            tools: HashMap::new(),
        };
        
        registry.register(ToolType::Bash);
        registry.register(ToolType::Read); 
        registry.register(ToolType::Write);
        registry.register(ToolType::Search);
        
        registry
    }
    
    pub fn register(&mut self, tool: ToolType) {
        self.tools.insert(tool.name().to_string(), tool);
    }
    
    pub fn get(&self, name: &str) -> Option<&ToolType> {
        self.tools.get(name)
    }
    
    pub fn tools_schema(&self) -> Vec<Value> {
        self.tools.values().map(|tool| {
            json!({
                "name": tool.name(),
                "description": tool.description(),
                "input_schema": tool.parameters()
            })
        }).collect()
    }
}

// Bash tool implementation
async fn execute_bash(params: Value) -> Result<String> {
    let command = params["command"].as_str()
        .ok_or_else(|| RuntimeError::Tool("Missing command parameter".to_string()))?;
    
    let result = timeout(Duration::from_secs(30), async {
        Command::new("bash")
            .arg("-c")
            .arg(command)
            .kill_on_drop(true)
            .output()
            .await
    }).await;
    
    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if output.status.success() {
                let mut result = stdout.to_string();
                if !stderr.is_empty() {
                    result.push_str("\n[stderr]: ");
                    result.push_str(&stderr);
                }
                Ok(result)
            } else {
                Err(RuntimeError::Tool(format!("Command failed (exit {}):\n[stdout]: {}\n[stderr]: {}",
                    output.status.code().unwrap_or(-1), stdout, stderr)))
            }
        }
        Ok(Err(e)) => Err(RuntimeError::Tool(format!("Failed to execute command: {}", e))),
        Err(_) => Err(RuntimeError::Tool("Command timed out".to_string())),
    }
}

// Read tool implementation
async fn execute_read(params: Value) -> Result<String> {
    let path = params["path"].as_str()
        .ok_or_else(|| RuntimeError::Tool("Missing path parameter".to_string()))?;
    
    match tokio::fs::read_to_string(path).await {
        Ok(content) => Ok(content),
        Err(e) => Err(RuntimeError::Tool(format!("Failed to read file: {}", e))),
    }
}

// Write tool implementation
async fn execute_write(params: Value) -> Result<String> {
    let path = params["path"].as_str()
        .ok_or_else(|| RuntimeError::Tool("Missing path parameter".to_string()))?;
    let content = params["content"].as_str()
        .ok_or_else(|| RuntimeError::Tool("Missing content parameter".to_string()))?;
    
    match tokio::fs::write(path, content).await {
        Ok(_) => Ok(format!("Successfully wrote to {}", path)),
        Err(e) => Err(RuntimeError::Tool(format!("Failed to write file: {}", e))),
    }
}

// Search tool implementation (placeholder)
async fn execute_search(params: Value) -> Result<String> {
    let query = params["query"].as_str()
        .ok_or_else(|| RuntimeError::Tool("Missing query parameter".to_string()))?;
    
    // Placeholder - in the future this would integrate with VelociRAG
    Ok(format!("Search functionality not implemented yet for query: {}", query))
}