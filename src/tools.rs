use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use crate::{Result, RuntimeError};

fn expand_path(path: &str) -> PathBuf {
    if path.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(&path[2..]);
        }
    } else if path == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(path)
}

// ── Tool Trait ──────────────���──────────────────────���───────────────────────

/// Context passed to tool execution — channels for streaming output and events.
pub struct ToolContext {
    pub tx_delta: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    pub tx_events: Option<tokio::sync::mpsc::UnboundedSender<crate::StreamEvent>>,
}

/// The core trait for all tools. Implement this to add a new tool.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// Tool name as it appears in the API (e.g. "bash", "read").
    fn name(&self) -> &str;

    /// Human-readable description sent to the model.
    fn description(&self) -> &str;

    /// JSON Schema for the tool's parameters.
    fn parameters(&self) -> Value;

    /// Execute the tool with the given parameters.
    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String>;
}

// ── Tool Registry ──────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    /// Cached schema — rebuilt on register(), shared via Arc for zero-copy reads.
    cached_schema: Arc<Vec<Value>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(BashTool),
            Arc::new(ReadTool),
            Arc::new(WriteTool),
            Arc::new(EditTool),
            Arc::new(GrepTool),
            Arc::new(FindTool),
            Arc::new(LsTool),
            Arc::new(SubagentTool),
        ];
        Self::from_tools(tools)
    }

    /// Registry without subagent tool — used for subagent runtimes to prevent recursion.
    pub fn without_subagent() -> Self {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(BashTool),
            Arc::new(ReadTool),
            Arc::new(WriteTool),
            Arc::new(EditTool),
            Arc::new(GrepTool),
            Arc::new(FindTool),
            Arc::new(LsTool),
        ];
        Self::from_tools(tools)
    }

    fn from_tools(tool_list: Vec<Arc<dyn Tool>>) -> Self {
        let mut tools = HashMap::new();
        for tool in &tool_list {
            tools.insert(tool.name().to_string(), Arc::clone(tool));
        }

        let cached_schema = tool_list.iter().map(|tool| {
            json!({
                "name": tool.name(),
                "description": tool.description(),
                "input_schema": tool.parameters()
            })
        }).collect();

        ToolRegistry { tools, cached_schema: Arc::new(cached_schema) }
    }

    /// Register an additional tool at runtime (e.g. MCP tools, custom tools).
    /// If a tool with the same name exists, it is replaced.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();

        // Get mutable access to schema (Arc::make_mut clones only if shared)
        let schema = Arc::make_mut(&mut self.cached_schema);

        // Remove existing schema entry if replacing
        if self.tools.contains_key(&name) {
            schema.retain(|s| s["name"].as_str() != Some(&name));
        }

        schema.push(json!({
            "name": tool.name(),
            "description": tool.description(),
            "input_schema": tool.parameters()
        }));
        self.tools.insert(name, tool);
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    pub fn tools_schema(&self) -> Arc<Vec<Value>> {
        Arc::clone(&self.cached_schema)
    }
}

// ── Bash Tool ──��───────────────────────────────────��───────────────────────

pub struct BashTool;

#[async_trait::async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str { "bash" }

    fn description(&self) -> &str {
        "Execute a bash command and return its output. Use for running programs, installing packages, git operations, and any shell commands. Commands time out after 30 seconds."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The bash command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (default: 30, max: 300)"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let command = params["command"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing command parameter".to_string()))?;

        let timeout_secs = params["timeout"].as_u64().unwrap_or(30).min(300);

        let mut child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(command)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| RuntimeError::Tool(e.to_string()))?;

        let stdout = child.stdout.take()
            .ok_or_else(|| RuntimeError::Tool("Failed to capture stdout".to_string()))?;
        let stderr = child.stderr.take()
            .ok_or_else(|| RuntimeError::Tool("Failed to capture stderr".to_string()))?;

        let (tx_inter, mut rx_inter) = tokio::sync::mpsc::unbounded_channel::<String>();

        let tx_o = tx_inter.clone();
        let txd1 = ctx.tx_delta.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut reader = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let msg = format!("{}\n", line);
                let _ = tx_o.send(msg.clone());
                if let Some(ref t) = txd1 { let _ = t.send(msg); }
            }
        });

        let tx_e = tx_inter.clone();
        let txd2 = ctx.tx_delta.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut reader = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let msg = format!("{}\n", line);
                let _ = tx_e.send(msg.clone());
                if let Some(ref t) = txd2 { let _ = t.send(msg); }
            }
        });

        drop(tx_inter);

        let result = tokio::time::timeout(tokio::time::Duration::from_secs(timeout_secs), async {
            let mut full_output = String::new();
            while let Some(line) = rx_inter.recv().await {
                full_output.push_str(&line);
                if full_output.len() > 30_000 {
                    full_output.truncate(30_000);
                    full_output.push_str("\n\n[output truncated at 30KB]");
                    break;
                }
            }
            let status = child.wait().await.map_err(|e| RuntimeError::Tool(e.to_string()))?;
            Ok::<_, RuntimeError>((status, full_output))
        }).await;

        match result {
            Ok(Ok((status, output))) => {
                if status.success() {
                    Ok(output)
                } else {
                    Err(RuntimeError::Tool(format!(
                        "Command failed (exit {}):\n{}",
                        status.code().unwrap_or(-1), output
                    )))
                }
            }
            Ok(Err(e)) => Err(RuntimeError::Tool(format!("Failed to execute command: {}", e))),
            Err(_) => Err(RuntimeError::Tool(format!("Command timed out after {}s", timeout_secs))),
        }
    }
}

// ── Read Tool ───────���───────────────────────────────��──────────────────────

pub struct ReadTool;

#[async_trait::async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str { "read" }

    fn description(&self) -> &str {
        "Read the contents of a file. Returns lines with line numbers. Reads up to 500 lines by default. For large files, use offset and limit to read in sections."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read"
                },
                "offset": {
                    "type": "integer",
                    "description": "Line number to start reading from (0-indexed, default: 0)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read (default: all lines)"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, params: Value, _ctx: ToolContext) -> Result<String> {
        let raw_path = params["path"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing path parameter".to_string()))?;
        let path = expand_path(raw_path);

        let content = tokio::fs::read_to_string(&path).await
            .map_err(|e| RuntimeError::Tool(format!("Failed to read file '{}': {}", path.display(), e)))?;

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        let offset = params["offset"].as_u64().unwrap_or(0) as usize;
        let limit = params["limit"].as_u64().map(|l| l as usize).unwrap_or(500.min(total_lines));

        let start = offset.min(total_lines);
        let end = (start + limit).min(total_lines);

        let mut result = String::new();
        for (i, line) in lines[start..end].iter().enumerate() {
            result.push_str(&format!("{}\t{}\n", start + i + 1, line));
        }

        if total_lines > end {
            result.push_str(&format!("\n... ({} more lines)", total_lines - end));
        }

        Ok(result)
    }
}

// ── Write Tool ─────────────────────────────────────────��───────────────────

pub struct WriteTool;

#[async_trait::async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str { "write" }

    fn description(&self) -> &str {
        "Create or overwrite a file with the given content. Creates parent directories if needed. Use this for creating new files or completely rewriting existing ones."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, params: Value, _ctx: ToolContext) -> Result<String> {
        let raw_path = params["path"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing path parameter".to_string()))?;
        let content = params["content"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing content parameter".to_string()))?;

        let path = expand_path(raw_path);

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await
                    .map_err(|e| RuntimeError::Tool(format!("Failed to create directories: {}", e)))?;
            }
        }

        let tmp_path = path.with_extension("agent-tmp");
        tokio::fs::write(&tmp_path, content).await
            .map_err(|e| RuntimeError::Tool(format!("Failed to write file: {}", e)))?;
        tokio::fs::rename(&tmp_path, &path).await
            .map_err(|e| {
                let tmp = tmp_path.clone();
                tokio::spawn(async move { let _ = tokio::fs::remove_file(tmp).await; });
                RuntimeError::Tool(format!("Failed to finalize write: {}", e))
            })?;

        let line_count = content.lines().count();
        Ok(format!("Wrote {} lines ({} bytes) to {}", line_count, content.len(), path.display()))
    }
}

// ── Edit Tool ─────────────────────────────────────────��────────────────────

pub struct EditTool;

#[async_trait::async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str { "edit" }

    fn description(&self) -> &str {
        "Make a surgical edit to a file by replacing an exact string match. The old_string must appear exactly once in the file. Provide enough surrounding context to make the match unique."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit"
                },
                "old_string": {
                    "type": "string",
                    "description": "The exact text to find and replace. Must match exactly once in the file."
                },
                "new_string": {
                    "type": "string",
                    "description": "The replacement text"
                }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, params: Value, _ctx: ToolContext) -> Result<String> {
        let raw_path = params["path"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing path parameter".to_string()))?;
        let old_string = params["old_string"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing old_string parameter".to_string()))?;
        let new_string = params["new_string"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing new_string parameter".to_string()))?;

        let path = expand_path(raw_path);

        let content = tokio::fs::read_to_string(&path).await
            .map_err(|e| RuntimeError::Tool(format!("Failed to read file '{}': {}", path.display(), e)))?;

        let count = content.matches(old_string).count();

        if count == 0 {
            return Err(RuntimeError::Tool(format!(
                "old_string not found in '{}'. Make sure it matches exactly, including whitespace and indentation.",
                path.display()
            )));
        }

        if count > 1 {
            return Err(RuntimeError::Tool(format!(
                "old_string found {} times in '{}'. It must be unique — include more surrounding context.",
                count, path.display()
            )));
        }

        let new_content = content.replacen(old_string, new_string, 1);

        let tmp_path = path.with_extension("agent-tmp");
        tokio::fs::write(&tmp_path, &new_content).await
            .map_err(|e| RuntimeError::Tool(format!("Failed to write file: {}", e)))?;
        tokio::fs::rename(&tmp_path, &path).await
            .map_err(|e| {
                let tmp = tmp_path.clone();
                tokio::spawn(async move { let _ = tokio::fs::remove_file(tmp).await; });
                RuntimeError::Tool(format!("Failed to finalize edit: {}", e))
            })?;

        let old_lines: Vec<&str> = old_string.lines().collect();
        let new_lines: Vec<&str> = new_string.lines().collect();
        Ok(format!(
            "Edited {} — replaced {} line(s) with {} line(s)",
            path.display(), old_lines.len(), new_lines.len()
        ))
    }
}

// ── Grep Tool ──────────────────────────────────────���───────────────────────

pub struct GrepTool;

#[async_trait::async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str { "grep" }

    fn description(&self) -> &str {
        "Search file contents using regex patterns. Returns matching lines with file paths and line numbers. Supports file type filtering and context lines."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search in (default: current directory)"
                },
                "include": {
                    "type": "string",
                    "description": "Glob pattern to filter files (e.g. \"*.rs\", \"*.py\")"
                },
                "context": {
                    "type": "integer",
                    "description": "Number of context lines to show before and after each match"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, params: Value, _ctx: ToolContext) -> Result<String> {
        let pattern = params["pattern"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing pattern parameter".to_string()))?;
        let path = expand_path(params["path"].as_str().unwrap_or("."));
        let include = params["include"].as_str();
        let context = params["context"].as_u64();

        let mut cmd = Command::new("grep");
        cmd.arg("-rn");
        cmd.arg("--color=never");

        if let Some(glob) = include {
            cmd.arg("--include").arg(glob);
        }

        if let Some(ctx) = context {
            cmd.arg(format!("-C{}", ctx));
        }

        cmd.arg("--exclude-dir=.git");
        cmd.arg("--exclude-dir=node_modules");
        cmd.arg("--exclude-dir=target");

        cmd.arg("--").arg(pattern).arg(&path);

        let output = tokio::time::timeout(Duration::from_secs(15), cmd.output()).await
            .map_err(|_| RuntimeError::Tool("Grep timed out after 15s".to_string()))?
            .map_err(|e| RuntimeError::Tool(format!("Failed to execute grep: {}", e)))?;

        let stdout = String::from_utf8_lossy(&output.stdout);

        if stdout.is_empty() {
            Ok("No matches found.".to_string())
        } else {
            let result = stdout.to_string();
            if result.len() > 50000 {
                let truncated: String = result.chars().take(50000).collect();
                Ok(format!("{}\n\n... (output truncated, {} total bytes)", truncated, result.len()))
            } else {
                Ok(result)
            }
        }
    }
}

// ── Find Tool ─────────────────��──────────────────────────────────���─────────

pub struct FindTool;

#[async_trait::async_trait]
impl Tool for FindTool {
    fn name(&self) -> &str { "find" }

    fn description(&self) -> &str {
        "Find files by name using glob patterns. Searches recursively from the given path. Excludes .git directories."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match file names (e.g. \"*.rs\", \"Cargo.*\")"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (default: current directory)"
                },
                "type": {
                    "type": "string",
                    "description": "Filter by type: \"f\" for files, \"d\" for directories"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, params: Value, _ctx: ToolContext) -> Result<String> {
        let pattern = params["pattern"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing pattern parameter".to_string()))?;
        let path = expand_path(params["path"].as_str().unwrap_or("."));
        let file_type = params["type"].as_str();

        let mut cmd = Command::new("find");
        cmd.arg(&path);

        cmd.args(["-not", "-path", "*/.git/*"]);
        cmd.args(["-not", "-path", "*/node_modules/*"]);
        cmd.args(["-not", "-path", "*/target/*"]);

        if let Some(t) = file_type {
            cmd.arg("-type").arg(t);
        }

        cmd.arg("-name").arg(pattern);

        let output = tokio::time::timeout(Duration::from_secs(10), cmd.output()).await
            .map_err(|_| RuntimeError::Tool("Find timed out after 10s".to_string()))?
            .map_err(|e| RuntimeError::Tool(format!("Failed to execute find: {}", e)))?;

        let stdout = String::from_utf8_lossy(&output.stdout);

        if stdout.is_empty() {
            Ok("No files found.".to_string())
        } else {
            Ok(stdout.trim().to_string())
        }
    }
}

// ── Ls Tool ───────────���───────────────────────────────��────────────────────

pub struct LsTool;

#[async_trait::async_trait]
impl Tool for LsTool {
    fn name(&self) -> &str { "ls" }

    fn description(&self) -> &str {
        "List directory contents with details (permissions, size, modification date). Defaults to current directory."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path to list (default: current directory)"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, params: Value, _ctx: ToolContext) -> Result<String> {
        let path = expand_path(params["path"].as_str().unwrap_or("."));

        let output = Command::new("ls")
            .arg("-lah")
            .arg(&path)
            .output()
            .await
            .map_err(|e| RuntimeError::Tool(format!("Failed to execute ls: {}", e)))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            Err(RuntimeError::Tool(format!("ls failed: {}", stderr)))
        } else if stdout.is_empty() {
            Ok("Directory is empty.".to_string())
        } else {
            Ok(stdout.to_string())
        }
    }
}

// ── Subagent Tool ───────────────��──────────────────────────────────────────

/// Resolve an agent name to a system prompt.
pub fn resolve_agent_prompt(name: &str) -> std::result::Result<String, String> {
    if name.contains('/') {
        let path = expand_path(name);
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read agent file '{}': {}", path.display(), e))?;
        return Ok(strip_frontmatter(&content));
    }

    let agents_dir = crate::config::base_dir().join("agents");
    let agent_path = agents_dir.join(format!("{}.md", name));

    if agent_path.exists() {
        let content = std::fs::read_to_string(&agent_path)
            .map_err(|e| format!("Failed to read agent '{}': {}", agent_path.display(), e))?;
        return Ok(strip_frontmatter(&content));
    }

    Err(format!(
        "Agent '{}' not found. Searched:\n  - {}\nCreate the file or pass a system_prompt directly.",
        name, agent_path.display()
    ))
}

fn strip_frontmatter(content: &str) -> String {
    if content.starts_with("---") {
        if let Some(end) = content[3..].find("\n---") {
            // end is relative to content[3..], so closing "---" starts at 3+end+1
            // skip past the "\n---" (4 bytes) to get the body
            return content[3 + end + 4..].trim().to_string();
        }
    }
    content.to_string()
}

pub struct SubagentTool;

#[async_trait::async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &str { "subagent" }

    fn description(&self) -> &str {
        "Dispatch a one-shot subagent with a specific system prompt to perform a task. The subagent gets its own tool suite (bash, read, write, edit, grep, find, ls) and runs autonomously until done. Use for delegation — give a focused task to a specialist agent. Provide either an agent name (resolves from ~/.synaps-cli/agents/<name>.md) or a system_prompt string directly."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "description": "Agent name — resolves to ~/.synaps-cli/agents/<name>.md. Mutually exclusive with system_prompt."
                },
                "system_prompt": {
                    "type": "string",
                    "description": "Inline system prompt for the subagent. Use when you don't have a named agent file."
                },
                "task": {
                    "type": "string",
                    "description": "The task/prompt to send to the subagent."
                },
                "model": {
                    "type": "string",
                    "description": "Model override (default: claude-sonnet-4-20250514). Use claude-opus-4-6 for complex tasks."
                }
            },
            "required": ["task"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let task = params["task"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'task' parameter".to_string()))?
            .to_string();

        let agent_name = params["agent"].as_str().map(|s| s.to_string());
        let inline_prompt = params["system_prompt"].as_str().map(|s| s.to_string());
        let model_override = params["model"].as_str().map(|s| s.to_string());

        let system_prompt = match (&agent_name, &inline_prompt) {
            (Some(name), _) => {
                resolve_agent_prompt(name)
                    .map_err(|e| RuntimeError::Tool(e))?
            }
            (None, Some(prompt)) => prompt.clone(),
            (None, None) => {
                return Err(RuntimeError::Tool(
                    "Must provide either 'agent' (name) or 'system_prompt' (inline). Got neither.".to_string()
                ));
            }
        };

        let label = agent_name.as_deref().unwrap_or("inline").to_string();
        let model = model_override.unwrap_or_else(|| "claude-sonnet-4-20250514".to_string());
        let task_preview: String = task.chars().take(80).collect();

        tracing::info!("Dispatching subagent '{}' with model {}", label, model);

        if let Some(ref tx) = ctx.tx_events {
            let _ = tx.send(crate::StreamEvent::SubagentStart {
                agent_name: label.clone(),
                task_preview: task_preview.clone(),
            });
        }

        let start_time = std::time::Instant::now();

        let (result_tx, result_rx) = tokio::sync::oneshot::channel::<std::result::Result<SubagentResult, String>>();
        let label_inner = label.clone();
        let model_inner = model.clone();
        let tx_events_inner = ctx.tx_events.clone();

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let _thread_handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            let result = rt.block_on(async move {
                use futures::StreamExt;

                let mut runtime = match crate::Runtime::new().await {
                    Ok(r) => r,
                    Err(e) => return Err(format!("Failed to create subagent runtime: {}", e)),
                };

                runtime.set_system_prompt(system_prompt);
                runtime.set_model(model);
                runtime.set_tools(crate::ToolRegistry::without_subagent());

                let cancel = crate::CancellationToken::new();
                let cancel_inner = cancel.clone();

                tokio::spawn(async move {
                    let _ = shutdown_rx.await;
                    cancel_inner.cancel();
                });

                let mut stream = runtime.run_stream(task, cancel).await;

                let mut final_text = String::new();
                let mut tool_count = 0u32;
                let mut total_input_tokens = 0u64;
                let mut total_output_tokens = 0u64;
                let mut total_cache_read = 0u64;
                let mut total_cache_creation = 0u64;

                let timeout_fut = tokio::time::sleep(Duration::from_secs(300));
                tokio::pin!(timeout_fut);

                loop {
                    tokio::select! {
                        event = stream.next() => {
                            let Some(event) = event else { break };
                            match event {
                                crate::StreamEvent::Thinking(_) => {
                                    if let Some(ref tx) = tx_events_inner {
                                        let _ = tx.send(crate::StreamEvent::SubagentUpdate {
                                            agent_name: label_inner.clone(),
                                            status: "thinking...".to_string(),
                                        });
                                    }
                                }
                                crate::StreamEvent::Text(text) => {
                                    final_text.push_str(&text);
                                }
                                crate::StreamEvent::ToolUseStart(name) => {
                                    tool_count += 1;
                                    if let Some(ref tx) = tx_events_inner {
                                        let _ = tx.send(crate::StreamEvent::SubagentUpdate {
                                            agent_name: label_inner.clone(),
                                            status: format!("⚙ {} (tool #{})", name, tool_count),
                                        });
                                    }
                                }
                                crate::StreamEvent::ToolUse { tool_name, .. } => {
                                    if let Some(ref tx) = tx_events_inner {
                                        let _ = tx.send(crate::StreamEvent::SubagentUpdate {
                                            agent_name: label_inner.clone(),
                                            status: format!("running {}", tool_name),
                                        });
                                    }
                                }
                                crate::StreamEvent::ToolResult { .. } => {
                                    if let Some(ref tx) = tx_events_inner {
                                        let _ = tx.send(crate::StreamEvent::SubagentUpdate {
                                            agent_name: label_inner.clone(),
                                            status: format!("done tool #{}", tool_count),
                                        });
                                    }
                                }
                                crate::StreamEvent::Usage {
                                    input_tokens, output_tokens,
                                    cache_read_input_tokens, cache_creation_input_tokens,
                                    model: _,
                                } => {
                                    total_input_tokens += input_tokens;
                                    total_output_tokens += output_tokens;
                                    total_cache_read += cache_read_input_tokens;
                                    total_cache_creation += cache_creation_input_tokens;
                                }
                                crate::StreamEvent::Error(e) => {
                                    return Err(e);
                                }
                                crate::StreamEvent::Done => break,
                                _ => {}
                            }
                        }
                        _ = &mut timeout_fut => {
                            return Err("Subagent timed out after 300s".to_string());
                        }
                    }
                }

                Ok(SubagentResult {
                    text: final_text,
                    model: model_inner,
                    input_tokens: total_input_tokens,
                    output_tokens: total_output_tokens,
                    cache_read: total_cache_read,
                    cache_creation: total_cache_creation,
                    tool_count,
                })
            });

            let _ = result_tx.send(result);
        });

        let result = result_rx.await;
        let elapsed = start_time.elapsed().as_secs_f64();

        drop(shutdown_tx);

        let log_dir = crate::config::base_dir().join("logs").join("subagents");
        let _ = std::fs::create_dir_all(&log_dir);
        let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");

        match result {
            Ok(Ok(sa_result)) => {
                let preview: String = sa_result.text.chars().take(120).collect();

                if let Some(ref tx) = ctx.tx_events {
                    let _ = tx.send(crate::StreamEvent::Usage {
                        input_tokens: sa_result.input_tokens,
                        output_tokens: sa_result.output_tokens,
                        cache_read_input_tokens: sa_result.cache_read,
                        cache_creation_input_tokens: sa_result.cache_creation,
                        model: Some(sa_result.model),
                    });
                    let _ = tx.send(crate::StreamEvent::SubagentDone {
                        agent_name: label.clone(),
                        result_preview: preview,
                        duration_secs: elapsed,
                    });
                }

                let log_content = format!(
                    "# Subagent: {}\nDate: {}\nModel: {}\nTask: {}\nDuration: {:.1}s\nTokens: {}in/{}out ({}cr/{}cw)\nTools used: {}\n\n## Result\n\n{}\n",
                    label, timestamp, params["model"].as_str().unwrap_or("sonnet"),
                    task_preview, elapsed,
                    sa_result.input_tokens, sa_result.output_tokens,
                    sa_result.cache_read, sa_result.cache_creation,
                    sa_result.tool_count, sa_result.text,
                );
                let log_path = log_dir.join(format!("{}-{}.md", timestamp, label));
                let _ = std::fs::write(&log_path, &log_content);

                Ok(format!("[subagent:{}] {}", label, sa_result.text))
            }
            Ok(Err(e)) => {
                if let Some(ref tx) = ctx.tx_events {
                    let _ = tx.send(crate::StreamEvent::SubagentDone {
                        agent_name: label.clone(),
                        result_preview: format!("ERROR: {}", e),
                        duration_secs: elapsed,
                    });
                }
                let log_path = log_dir.join(format!("{}-{}-error.md", timestamp, label));
                let _ = std::fs::write(&log_path, format!("# Subagent ERROR: {}\nTask: {}\nError: {}\n", label, task_preview, e));
                Ok(format!("[subagent:{} ERROR] {}", label, e))
            }
            Err(_) => {
                if let Some(ref tx) = ctx.tx_events {
                    let _ = tx.send(crate::StreamEvent::SubagentDone {
                        agent_name: label.clone(),
                        result_preview: "Task panicked or dropped".to_string(),
                        duration_secs: elapsed,
                    });
                }
                Ok(format!("[subagent:{} ERROR] Subagent task panicked or was dropped", label))
            }
        }
    }
}

struct SubagentResult {
    text: String,
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    cache_read: u64,
    cache_creation: u64,
    tool_count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_expand_path_home_prefix() {
        let home = env::var("HOME").expect("HOME env var should be set");
        let result = expand_path("~/foo");
        assert_eq!(result, PathBuf::from(home).join("foo"));
    }

    #[test]
    fn test_expand_path_tilde_alone() {
        let home = env::var("HOME").expect("HOME env var should be set");
        let result = expand_path("~");
        assert_eq!(result, PathBuf::from(home));
    }

    #[test]
    fn test_expand_path_absolute_unchanged() {
        let result = expand_path("/absolute/path");
        assert_eq!(result, PathBuf::from("/absolute/path"));
    }

    #[test]
    fn test_expand_path_relative_unchanged() {
        let result = expand_path("relative/path");
        assert_eq!(result, PathBuf::from("relative/path"));
    }

    #[test]
    fn test_strip_frontmatter_removes_frontmatter() {
        let content = "---\ntitle: test\ndate: 2023-01-01\n---\nThis is the content.";
        let result = strip_frontmatter(content);
        assert_eq!(result, "This is the content.");
    }

    #[test]
    fn test_strip_frontmatter_without_frontmatter() {
        let content = "This is just plain content.";
        let result = strip_frontmatter(content);
        assert_eq!(result, content);
    }

    #[test]
    fn test_strip_frontmatter_only_opening_delimiter() {
        let content = "---\ntitle: test\nno closing delimiter";
        let result = strip_frontmatter(content);
        assert_eq!(result, content);
    }

    #[test]
    fn test_bash_tool_schema() {
        let tool = BashTool;
        assert_eq!(tool.name(), "bash");
        assert!(!tool.description().is_empty());
        
        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"].is_object());
        assert!(params["required"].is_array());
    }

    #[test]
    fn test_read_tool_schema() {
        let tool = ReadTool;
        assert_eq!(tool.name(), "read");
        assert!(!tool.description().is_empty());
        
        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"].is_object());
        assert!(params["required"].is_array());
    }

    #[test]
    fn test_write_tool_schema() {
        let tool = WriteTool;
        assert_eq!(tool.name(), "write");
        assert!(!tool.description().is_empty());
        
        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"].is_object());
        assert!(params["required"].is_array());
    }

    #[test]
    fn test_edit_tool_schema() {
        let tool = EditTool;
        assert_eq!(tool.name(), "edit");
        assert!(!tool.description().is_empty());
        
        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"].is_object());
        assert!(params["required"].is_array());
    }

    #[test]
    fn test_grep_tool_schema() {
        let tool = GrepTool;
        assert_eq!(tool.name(), "grep");
        assert!(!tool.description().is_empty());
        
        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"].is_object());
        assert!(params["required"].is_array());
    }

    #[test]
    fn test_find_tool_schema() {
        let tool = FindTool;
        assert_eq!(tool.name(), "find");
        assert!(!tool.description().is_empty());
        
        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"].is_object());
        assert!(params["required"].is_array());
    }

    #[test]
    fn test_ls_tool_schema() {
        let tool = LsTool;
        assert_eq!(tool.name(), "ls");
        assert!(!tool.description().is_empty());
        
        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"].is_object());
        assert!(params["required"].is_array());
    }

    #[test]
    fn test_subagent_tool_schema() {
        let tool = SubagentTool;
        assert_eq!(tool.name(), "subagent");
        assert!(!tool.description().is_empty());
        
        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"].is_object());
        assert!(params["required"].is_array());
    }
}
