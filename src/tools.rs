use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;
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

#[derive(Debug, Clone)]
pub enum ToolType {
    Bash,
    Read,
    Write,
    Edit,
    Grep,
    Find,
    Ls,
    Subagent,
}

impl ToolType {
    pub fn name(&self) -> &str {
        match self {
            ToolType::Bash => "bash",
            ToolType::Read => "read",
            ToolType::Write => "write",
            ToolType::Edit => "edit",
            ToolType::Grep => "grep",
            ToolType::Find => "find",
            ToolType::Ls => "ls",
            ToolType::Subagent => "subagent",
        }
    }

    pub fn description(&self) -> &str {
        match self {
            ToolType::Bash => "Execute a bash command and return its output. Use for running programs, installing packages, git operations, and any shell commands. Commands time out after 30 seconds.",
            ToolType::Read => "Read the contents of a file. Returns lines with line numbers. Reads up to 500 lines by default. For large files, use offset and limit to read in sections.",
            ToolType::Write => "Create or overwrite a file with the given content. Creates parent directories if needed. Use this for creating new files or completely rewriting existing ones.",
            ToolType::Edit => "Make a surgical edit to a file by replacing an exact string match. The old_string must appear exactly once in the file. Provide enough surrounding context to make the match unique.",
            ToolType::Grep => "Search file contents using regex patterns. Returns matching lines with file paths and line numbers. Supports file type filtering and context lines.",
            ToolType::Find => "Find files by name using glob patterns. Searches recursively from the given path. Excludes .git directories.",
            ToolType::Ls => "List directory contents with details (permissions, size, modification date). Defaults to current directory.",
            ToolType::Subagent => "Dispatch a one-shot subagent with a specific system prompt to perform a task. The subagent gets its own tool suite (bash, read, write, edit, grep, find, ls) and runs autonomously until done. Use for delegation — give a focused task to a specialist agent. Provide either an agent name (resolves from ~/.synaps-cli/agents/<name>.md) or a system_prompt string directly.",
        }
    }

    pub fn parameters(&self) -> Value {
        match self {
            ToolType::Bash => json!({
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
            }),
            ToolType::Read => json!({
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
            }),
            ToolType::Write => json!({
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
            }),
            ToolType::Edit => json!({
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
            }),
            ToolType::Grep => json!({
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
            }),
            ToolType::Find => json!({
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
            }),
            ToolType::Ls => json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path to list (default: current directory)"
                    }
                },
                "required": []
            }),
            ToolType::Subagent => json!({
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
            }),
        }
    }

    pub async fn execute(&self, params: Value, tx_delta: Option<tokio::sync::mpsc::UnboundedSender<String>>, tx_events: Option<tokio::sync::mpsc::UnboundedSender<crate::StreamEvent>>) -> Result<String> {
        let start_time = std::time::Instant::now();
        tracing::info!("Executing tool");
        let res = match self {
            ToolType::Bash => execute_bash(params, tx_delta).await,
            ToolType::Read => execute_read(params).await,
            ToolType::Write => execute_write(params).await,
            ToolType::Edit => execute_edit(params).await,
            ToolType::Grep => execute_grep(params).await,
            ToolType::Find => execute_find(params).await,
            ToolType::Ls => execute_ls(params).await,
            ToolType::Subagent => execute_subagent(params, tx_events).await,
        };
        tracing::debug!("Tool execution finished in {:?}", start_time.elapsed());
        res
    }
}

#[derive(Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, ToolType>,
    /// Cached schema — built once at construction, returned by reference on every API call.
    cached_schema: Vec<Value>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        let mut tools = HashMap::new();
        for tool in [
            ToolType::Bash, ToolType::Read, ToolType::Write,
            ToolType::Edit, ToolType::Grep, ToolType::Find, ToolType::Ls,
            ToolType::Subagent,
        ] {
            tools.insert(tool.name().to_string(), tool);
        }

        let cached_schema = tools.values().map(|tool| {
            json!({
                "name": tool.name(),
                "description": tool.description(),
                "input_schema": tool.parameters()
            })
        }).collect();

        ToolRegistry { tools, cached_schema }
    }

    pub fn get(&self, name: &str) -> Option<&ToolType> {
        self.tools.get(name)
    }

    pub fn tools_schema(&self) -> Vec<Value> {
        self.cached_schema.clone()
    }
}

// ── Bash ────────────────────────────────────────────────────────────────────

async fn execute_bash(params: Value, tx_delta: Option<tokio::sync::mpsc::UnboundedSender<String>>) -> Result<String> {
    let command = params["command"].as_str()
        .ok_or_else(|| RuntimeError::Tool("Missing command parameter".to_string()))?;

    let timeout_secs = params["timeout"].as_u64().unwrap_or(30).min(300);

    // Spawn child
    let mut child = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(command)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| RuntimeError::Tool(e.to_string()))?;

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let (tx_inter, mut rx_inter) = tokio::sync::mpsc::unbounded_channel::<String>();

    let tx_o = tx_inter.clone();
    let txd1 = tx_delta.clone();
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
    let txd2 = tx_delta.clone();
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let msg = format!("{}\n", line);
            let _ = tx_e.send(msg.clone());
            if let Some(ref t) = txd2 { let _ = t.send(msg); }
        }
    });

    // Drop the local sender so rx_inter terminates when the tasks complete
    drop(tx_inter);

    let result = tokio::time::timeout(tokio::time::Duration::from_secs(timeout_secs), async {
        let mut full_output = String::new();
        while let Some(line) = rx_inter.recv().await {
            full_output.push_str(&line);
            // Cap output to avoid bloating message history
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

// ── Read ────────────────────────────────────────────────────────────────────

async fn execute_read(params: Value) -> Result<String> {
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

// ── Write ───────────────────────────────────────────────────────────────────

async fn execute_write(params: Value) -> Result<String> {
    let raw_path = params["path"].as_str()
        .ok_or_else(|| RuntimeError::Tool("Missing path parameter".to_string()))?;
    let content = params["content"].as_str()
        .ok_or_else(|| RuntimeError::Tool("Missing content parameter".to_string()))?;

    let path = expand_path(raw_path);

    // Create parent directories if needed
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await
                .map_err(|e| RuntimeError::Tool(format!("Failed to create directories: {}", e)))?;
        }
    }

    // Atomic write: write to temp file, then rename
    let tmp_path = path.with_extension("agent-tmp");
    tokio::fs::write(&tmp_path, content).await
        .map_err(|e| RuntimeError::Tool(format!("Failed to write file: {}", e)))?;
    tokio::fs::rename(&tmp_path, &path).await
        .map_err(|e| {
            // Clean up temp file on rename failure
            let tmp = tmp_path.clone();
            tokio::spawn(async move { let _ = tokio::fs::remove_file(tmp).await; });
            RuntimeError::Tool(format!("Failed to finalize write: {}", e))
        })?;

    let line_count = content.lines().count();
    Ok(format!("Wrote {} lines ({} bytes) to {}", line_count, content.len(), path.display()))
}

// ── Edit ────────────────────────────────────────────────────────────────────

async fn execute_edit(params: Value) -> Result<String> {
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

    // Atomic write
    let tmp_path = path.with_extension("agent-tmp");
    tokio::fs::write(&tmp_path, &new_content).await
        .map_err(|e| RuntimeError::Tool(format!("Failed to write file: {}", e)))?;
    tokio::fs::rename(&tmp_path, &path).await
        .map_err(|e| {
            let tmp = tmp_path.clone();
            tokio::spawn(async move { let _ = tokio::fs::remove_file(tmp).await; });
            RuntimeError::Tool(format!("Failed to finalize edit: {}", e))
        })?;

    // Show what changed
    let old_lines: Vec<&str> = old_string.lines().collect();
    let new_lines: Vec<&str> = new_string.lines().collect();
    Ok(format!(
        "Edited {} — replaced {} line(s) with {} line(s)",
        path.display(), old_lines.len(), new_lines.len()
    ))
}

// ── Grep ────────────────────────────────────────────────────────────────────

async fn execute_grep(params: Value) -> Result<String> {
    let pattern = params["pattern"].as_str()
        .ok_or_else(|| RuntimeError::Tool("Missing pattern parameter".to_string()))?;
    let path = expand_path(params["path"].as_str().unwrap_or("."));
    let include = params["include"].as_str();
    let context = params["context"].as_u64();

    let mut cmd = Command::new("grep");
    cmd.arg("-rn"); // recursive, line numbers
    cmd.arg("--color=never");

    if let Some(glob) = include {
        cmd.arg("--include").arg(glob);
    }

    if let Some(ctx) = context {
        cmd.arg(format!("-C{}", ctx));
    }

    // Exclude common noise directories
    cmd.arg("--exclude-dir=.git");
    cmd.arg("--exclude-dir=node_modules");
    cmd.arg("--exclude-dir=target");

    cmd.arg("--").arg(pattern).arg(&path);

    let output = timeout(Duration::from_secs(15), cmd.output()).await
        .map_err(|_| RuntimeError::Tool("Grep timed out after 15s".to_string()))?
        .map_err(|e| RuntimeError::Tool(format!("Failed to execute grep: {}", e)))?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    if stdout.is_empty() {
        Ok("No matches found.".to_string())
    } else {
        // Truncate output if too large
        let result = stdout.to_string();
        if result.len() > 50000 {
            let truncated: String = result.chars().take(50000).collect();
            Ok(format!("{}\n\n... (output truncated, {} total bytes)", truncated, result.len()))
        } else {
            Ok(result)
        }
    }
}

// ── Find ────────────────────────────────────────────────────────────────────

async fn execute_find(params: Value) -> Result<String> {
    let pattern = params["pattern"].as_str()
        .ok_or_else(|| RuntimeError::Tool("Missing pattern parameter".to_string()))?;
    let path = expand_path(params["path"].as_str().unwrap_or("."));
    let file_type = params["type"].as_str();

    let mut cmd = Command::new("find");
    cmd.arg(&path);

    // Exclude .git and other noise
    cmd.args(["-not", "-path", "*/.git/*"]);
    cmd.args(["-not", "-path", "*/node_modules/*"]);
    cmd.args(["-not", "-path", "*/target/*"]);

    // Type filter
    if let Some(t) = file_type {
        cmd.arg("-type").arg(t);
    }

    cmd.arg("-name").arg(pattern);

    // Sort by path for consistent output
    let output = timeout(Duration::from_secs(10), cmd.output()).await
        .map_err(|_| RuntimeError::Tool("Find timed out after 10s".to_string()))?
        .map_err(|e| RuntimeError::Tool(format!("Failed to execute find: {}", e)))?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    if stdout.is_empty() {
        Ok("No files found.".to_string())
    } else {
        Ok(stdout.trim().to_string())
    }
}

// ── Ls ──────────────────────────────────────────────────────────────────────

async fn execute_ls(params: Value) -> Result<String> {
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

// ── Subagent ───────────────────────────────────────────────────────────────

/// Resolve an agent name to a system prompt.
/// Search order:
///   1. ~/.synaps-cli/agents/<name>.md
///   2. Absolute/relative path (if name contains '/')
pub fn resolve_agent_prompt(name: &str) -> std::result::Result<String, String> {
    // If it looks like a path, try it directly
    if name.contains('/') {
        let path = expand_path(name);
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read agent file '{}': {}", path.display(), e))?;
        return Ok(strip_frontmatter(&content));
    }

    // Resolve from ~/.synaps-cli/agents/<name>.md
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

/// Strip YAML frontmatter (---...---) from markdown content.
fn strip_frontmatter(content: &str) -> String {
    if content.starts_with("---") {
        if let Some(end) = content[3..].find("---") {
            return content[end + 6..].trim().to_string();
        }
    }
    content.to_string()
}

async fn execute_subagent(params: Value, tx_events: Option<tokio::sync::mpsc::UnboundedSender<crate::StreamEvent>>) -> Result<String> {
    let task = params["task"].as_str()
        .ok_or_else(|| RuntimeError::Tool("Missing 'task' parameter".to_string()))?
        .to_string();

    let agent_name = params["agent"].as_str().map(|s| s.to_string());
    let inline_prompt = params["system_prompt"].as_str().map(|s| s.to_string());
    let model_override = params["model"].as_str().map(|s| s.to_string());

    // Resolve system prompt
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

    // Emit SubagentStart event for TUI
    if let Some(ref tx) = tx_events {
        let _ = tx.send(crate::StreamEvent::SubagentStart {
            agent_name: label.clone(),
            task_preview,
        });
    }

    let start_time = std::time::Instant::now();

    // Spawn on a dedicated thread with its own tokio runtime.
    // run_stream futures aren't Send due to recursive async,
    // so we isolate on a single-threaded runtime.
    let (result_tx, result_rx) = tokio::sync::oneshot::channel::<std::result::Result<String, String>>();
    let label_inner = label.clone();
    let tx_events_inner = tx_events.clone();

    std::thread::spawn(move || {
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

            let cancel = crate::CancellationToken::new();
            let mut stream = runtime.run_stream(task, cancel).await;

            let mut final_text = String::new();
            let mut tool_count = 0u32;

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

            Ok(final_text)
        });

        let _ = result_tx.send(result);
    });

    let result = result_rx.await;
    let elapsed = start_time.elapsed().as_secs_f64();

    match result {
        Ok(Ok(response)) => {
            let preview: String = response.chars().take(120).collect();
            if let Some(ref tx) = tx_events {
                let _ = tx.send(crate::StreamEvent::SubagentDone {
                    agent_name: label.clone(),
                    result_preview: preview,
                    duration_secs: elapsed,
                });
            }
            Ok(format!("[subagent:{}] {}", label, response))
        }
        Ok(Err(e)) => {
            if let Some(ref tx) = tx_events {
                let _ = tx.send(crate::StreamEvent::SubagentDone {
                    agent_name: label.clone(),
                    result_preview: format!("ERROR: {}", e),
                    duration_secs: elapsed,
                });
            }
            Ok(format!("[subagent:{} ERROR] {}", label, e))
        }
        Err(_) => {
            if let Some(ref tx) = tx_events {
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
