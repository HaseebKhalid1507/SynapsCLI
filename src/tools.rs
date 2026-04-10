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
        }
    }

    pub async fn execute(&self, params: Value) -> Result<String> {
        match self {
            ToolType::Bash => execute_bash(params).await,
            ToolType::Read => execute_read(params).await,
            ToolType::Write => execute_write(params).await,
            ToolType::Edit => execute_edit(params).await,
            ToolType::Grep => execute_grep(params).await,
            ToolType::Find => execute_find(params).await,
            ToolType::Ls => execute_ls(params).await,
        }
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

async fn execute_bash(params: Value) -> Result<String> {
    let command = params["command"].as_str()
        .ok_or_else(|| RuntimeError::Tool("Missing command parameter".to_string()))?;

    let timeout_secs = params["timeout"].as_u64().unwrap_or(30).min(300);

    let result = timeout(Duration::from_secs(timeout_secs), async {
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
                // Cap output to avoid bloating message history
                if result.len() > 30_000 {
                    result.truncate(30_000);
                    result.push_str("\n\n[output truncated at 30KB]");
                }
                Ok(result)
            } else {
                Err(RuntimeError::Tool(format!(
                    "Command failed (exit {}):\n[stdout]: {}\n[stderr]: {}",
                    output.status.code().unwrap_or(-1), stdout, stderr
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
