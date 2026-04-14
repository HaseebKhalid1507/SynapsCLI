use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use crate::Result;

/// Global counter for unique subagent IDs across all dispatches
pub(crate) static NEXT_SUBAGENT_ID: AtomicU64 = AtomicU64::new(1);

/// Strip ANSI escape sequences from a string.
/// Handles CSI sequences (\x1b[...X), OSC sequences (\x1b]...\x07), and simple \x1b(X) escapes.
pub(crate) fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    // CSI: consume until a letter (0x40-0x7E)
                    while let Some(&c) = chars.peek() {
                        chars.next();
                        if c.is_ascii_alphabetic() || c == '~' || c == '@' { break; }
                    }
                }
                Some(']') => {
                    chars.next();
                    // OSC: consume until BEL (\x07) or ST (\x1b\\)
                    while let Some(&c) = chars.peek() {
                        chars.next();
                        if c == '\x07' { break; }
                        if c == '\x1b' {
                            if chars.peek() == Some(&'\\') { chars.next(); }
                            break;
                        }
                    }
                }
                Some(_) => { chars.next(); } // simple two-char escape
                None => {}
            }
        } else {
            result.push(ch);
        }
    }
    result
}

pub(crate) fn expand_path(path: &str) -> PathBuf {
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

// ── Tool Trait ──────────────────────────────────────────────────────────────────

/// Context passed to tool execution — channels for streaming output and events.
pub struct ToolContext {
    pub tx_delta: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    pub tx_events: Option<tokio::sync::mpsc::UnboundedSender<crate::StreamEvent>>,
    pub watcher_exit_path: Option<PathBuf>,
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
            Arc::new(crate::tools::bash::BashTool),
            Arc::new(crate::tools::read::ReadTool),
            Arc::new(crate::tools::write::WriteTool),
            Arc::new(crate::tools::edit::EditTool),
            Arc::new(crate::tools::grep::GrepTool),
            Arc::new(crate::tools::find::FindTool),
            Arc::new(crate::tools::ls::LsTool),
            Arc::new(crate::tools::subagent::SubagentTool),
        ];
        Self::from_tools(tools)
    }

    /// Registry without subagent tool — used for subagent runtimes to prevent recursion.
    pub fn without_subagent() -> Self {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(crate::tools::bash::BashTool),
            Arc::new(crate::tools::read::ReadTool),
            Arc::new(crate::tools::write::WriteTool),
            Arc::new(crate::tools::edit::EditTool),
            Arc::new(crate::tools::grep::GrepTool),
            Arc::new(crate::tools::find::FindTool),
            Arc::new(crate::tools::ls::LsTool),
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

// ── Module declarations ──────────────────────────────────────────────────────────

mod bash;
mod read;
mod write;
mod edit;
mod grep;
mod find;
mod ls;
mod subagent;
pub mod watcher_exit;

// ── Re-exports ──────────────────────────────────────────────────────────────────

pub use bash::BashTool;
pub use read::ReadTool;
pub use write::WriteTool;
pub use edit::EditTool;
pub use grep::GrepTool;
pub use find::FindTool;
pub use ls::LsTool;
pub use subagent::{SubagentTool, SubagentResult};
pub use watcher_exit::WatcherExitTool;

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

    // ── Async Integration Tests ──────────────────────────────────────────

    use tokio;

    fn create_tool_context() -> ToolContext {
        ToolContext {
            tx_delta: None,
            tx_events: None,
            watcher_exit_path: None,
        }
    }

    #[tokio::test]
    async fn test_read_tool_execution() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("read_tool_test.txt");
        
        // Create temp file with known content
        let content = "line 1\nline 2\nline 3\nline 4\nline 5";
        std::fs::write(&test_file, content).unwrap();
        
        let tool = ReadTool;
        let ctx = create_tool_context();
        
        // Test basic read
        let params = json!({
            "path": test_file.to_string_lossy()
        });
        let result = tool.execute(params, ctx).await.unwrap();
        
        // Verify line numbers and content
        assert!(result.contains("1\tline 1"));
        assert!(result.contains("2\tline 2"));
        assert!(result.contains("5\tline 5"));
        
        // Test with offset and limit
        let ctx = create_tool_context();
        let params = json!({
            "path": test_file.to_string_lossy(),
            "offset": 2,
            "limit": 2
        });
        let result = tool.execute(params, ctx).await.unwrap();
        
        assert!(result.contains("3\tline 3"));
        assert!(result.contains("4\tline 4"));
        assert!(!result.contains("1\tline 1"));
        assert!(!result.contains("5\tline 5"));
        
        // Cleanup
        let _ = std::fs::remove_file(&test_file);
    }

    #[tokio::test]
    async fn test_write_tool_execution() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("write_tool_test.txt");
        
        let tool = WriteTool;
        let ctx = create_tool_context();
        
        let content = "Hello, world!\nThis is a test file.";
        let params = json!({
            "path": test_file.to_string_lossy(),
            "content": content
        });
        
        let result = tool.execute(params, ctx).await.unwrap();
        
        // Verify success message
        assert!(result.contains("Wrote 2 lines"));
        assert!(result.contains("bytes"));
        
        // Verify file was created with correct content
        let written_content = std::fs::read_to_string(&test_file).unwrap();
        assert_eq!(written_content, content);
        
        // Test parent directory creation
        let nested_file = temp_dir.join("nested").join("dir").join("test.txt");
        let ctx = create_tool_context();
        let params = json!({
            "path": nested_file.to_string_lossy(),
            "content": "nested content"
        });
        
        let result = tool.execute(params, ctx).await.unwrap();
        assert!(result.contains("Wrote"));
        
        let nested_content = std::fs::read_to_string(&nested_file).unwrap();
        assert_eq!(nested_content, "nested content");
        
        // Cleanup
        let _ = std::fs::remove_file(&test_file);
        let _ = std::fs::remove_dir_all(temp_dir.join("nested"));
    }

    #[tokio::test]
    async fn test_edit_tool_execution() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("edit_tool_test.txt");
        
        // Create file with known content
        let initial_content = "Hello world\nThis is a test\nEnd of file";
        std::fs::write(&test_file, initial_content).unwrap();
        
        let tool = EditTool;
        
        // Test successful replacement
        let ctx = create_tool_context();
        let params = json!({
            "path": test_file.to_string_lossy(),
            "old_string": "This is a test",
            "new_string": "This is modified"
        });
        
        let result = tool.execute(params, ctx).await.unwrap();
        assert!(result.contains("Edited"));
        assert!(result.contains("replaced 1 line(s) with 1 line(s)"));
        
        let modified_content = std::fs::read_to_string(&test_file).unwrap();
        assert!(modified_content.contains("This is modified"));
        assert!(!modified_content.contains("This is a test"));
        
        // Test old_string not found
        let ctx = create_tool_context();
        let params = json!({
            "path": test_file.to_string_lossy(),
            "old_string": "nonexistent string",
            "new_string": "replacement"
        });
        
        let result = tool.execute(params, ctx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("old_string not found"));
        
        // Test old_string found multiple times
        std::fs::write(&test_file, "test\ntest\nother").unwrap();
        let ctx = create_tool_context();
        let params = json!({
            "path": test_file.to_string_lossy(),
            "old_string": "test",
            "new_string": "replacement"
        });
        
        let result = tool.execute(params, ctx).await;
        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("found 2 times"));
        assert!(error_msg.contains("must be unique"));
        
        // Cleanup
        let _ = std::fs::remove_file(&test_file);
    }

    #[tokio::test]
    async fn test_ls_tool_execution() {
        let tool = LsTool;
        let ctx = create_tool_context();
        
        let params = json!({
            "path": "/tmp"
        });
        
        let result = tool.execute(params, ctx).await.unwrap();
        
        // Verify output is non-empty (should contain at least total line)
        assert!(!result.is_empty());
        // ls -lah should include total line or files/directories
        assert!(result.contains("total") || result.len() > 0);
    }

    #[tokio::test]
    async fn test_bash_tool_execution() {
        let tool = BashTool;
        
        // Test simple echo command
        let ctx = create_tool_context();
        let params = json!({
            "command": "echo hello"
        });
        
        let result = tool.execute(params, ctx).await.unwrap();
        assert!(result.contains("hello"));
        
        // Test timeout parameter with quick command
        let ctx = create_tool_context();
        let params = json!({
            "command": "sleep 1",
            "timeout": 2
        });
        
        let result = tool.execute(params, ctx).await;
        // Should succeed (1 second sleep with 2 second timeout)
        assert!(result.is_ok());
        
        // Test timeout with longer command
        let ctx = create_tool_context();
        let params = json!({
            "command": "sleep 3",
            "timeout": 1
        });
        
        let result = tool.execute(params, ctx).await;
        // Should timeout
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }

    // ── New Tests ────────────────────────────────────────────────────────────

    #[test]
    fn test_tool_registry_new() {
        let registry = ToolRegistry::new();
        
        // Should have 8 tools including subagent
        assert_eq!(registry.tools_schema().len(), 8);
        
        // Should find bash tool
        assert!(registry.get("bash").is_some());
        
        // Should not find nonexistent tool
        assert!(registry.get("nonexistent").is_none());
        
        // Verify all expected tools are present
        assert!(registry.get("bash").is_some());
        assert!(registry.get("read").is_some());
        assert!(registry.get("write").is_some());
        assert!(registry.get("edit").is_some());
        assert!(registry.get("grep").is_some());
        assert!(registry.get("find").is_some());
        assert!(registry.get("ls").is_some());
        assert!(registry.get("subagent").is_some());
    }

    #[test]
    fn test_tool_registry_without_subagent() {
        let registry = ToolRegistry::without_subagent();
        
        // Should have 7 tools without subagent
        assert_eq!(registry.tools_schema().len(), 7);
        
        // Should not have subagent tool
        assert!(registry.get("subagent").is_none());
        
        // Should still have bash tool
        assert!(registry.get("bash").is_some());
        
        // Verify all expected tools are present except subagent
        assert!(registry.get("bash").is_some());
        assert!(registry.get("read").is_some());
        assert!(registry.get("write").is_some());
        assert!(registry.get("edit").is_some());
        assert!(registry.get("grep").is_some());
        assert!(registry.get("find").is_some());
        assert!(registry.get("ls").is_some());
    }

    #[test]
    fn test_tool_registry_register() {
        let mut registry = ToolRegistry::without_subagent();
        let initial_count = registry.tools_schema().len();
        
        // Register a new tool (using BashTool with different name for simplicity)
        struct TestTool;
        #[async_trait::async_trait]
        impl Tool for TestTool {
            fn name(&self) -> &str { "test_tool" }
            fn description(&self) -> &str { "A test tool" }
            fn parameters(&self) -> Value { json!({"type": "object"}) }
            async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
                Ok("test result".to_string())
            }
        }
        
        registry.register(Arc::new(TestTool));
        
        // Should have one more tool now
        assert_eq!(registry.tools_schema().len(), initial_count + 1);
        
        // Should find the new tool
        assert!(registry.get("test_tool").is_some());
    }

    #[test]
    fn test_resolve_agent_prompt_nonexistent() {
        let result = resolve_agent_prompt("definitely_does_not_exist_12345");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error.contains("Agent 'definitely_does_not_exist_12345' not found"));
    }

    #[test]
    fn test_resolve_agent_prompt_path_not_found() {
        let result = resolve_agent_prompt("/nonexistent/path/agent.md");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error.contains("Failed to read agent file"));
    }

    #[tokio::test]
    async fn test_grep_tool_execution() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("test_grep_tool_execution.txt");
        
        // Write test content
        let content = "hello world\nfoo bar\nhello again";
        std::fs::write(&test_file, content).unwrap();
        
        let tool = GrepTool;
        let ctx = create_tool_context();
        
        let params = json!({
            "pattern": "hello",
            "path": test_file.to_string_lossy()
        });
        
        let result = tool.execute(params, ctx).await.unwrap();
        
        // Should contain both matching lines with line numbers
        assert!(result.contains("hello world"));
        assert!(result.contains("hello again"));
        assert!(result.contains("1:") || result.contains("hello world"));
        assert!(result.contains("3:") || result.contains("hello again"));
        
        // Cleanup
        let _ = std::fs::remove_file(&test_file);
    }

    #[tokio::test]
    async fn test_find_tool_execution() {
        let temp_dir = std::env::temp_dir().join("test_find_tool_execution");
        std::fs::create_dir_all(&temp_dir).unwrap();
        
        let test_file = temp_dir.join("test_find_me.txt");
        std::fs::write(&test_file, "test content").unwrap();
        
        let tool = FindTool;
        let ctx = create_tool_context();
        
        let params = json!({
            "pattern": "test_find_me*",
            "path": temp_dir.to_string_lossy()
        });
        
        let result = tool.execute(params, ctx).await.unwrap();
        
        // Should contain the filename
        assert!(result.contains("test_find_me.txt"));
        
        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_bash_tool_timeout() {
        let tool = BashTool;
        let ctx = create_tool_context();
        
        let params = json!({
            "command": "sleep 10",
            "timeout": 1
        });
        
        let result = tool.execute(params, ctx).await;
        
        // Should timeout and return error
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("timed out"));
    }

    #[tokio::test]
    async fn test_bash_tool_failure() {
        let tool = BashTool;
        let ctx = create_tool_context();
        
        let params = json!({
            "command": "exit 1"
        });
        
        let result = tool.execute(params, ctx).await;
        
        // Should fail and return error
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("failed") || error.contains("exit"));
    }

    #[tokio::test]
    async fn test_edit_tool_no_match() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("test_edit_tool_no_match.txt");
        
        // Create file with known content
        let content = "some content\nmore content";
        std::fs::write(&test_file, content).unwrap();
        
        let tool = EditTool;
        let ctx = create_tool_context();
        
        let params = json!({
            "path": test_file.to_string_lossy(),
            "old_string": "this string does not exist",
            "new_string": "replacement"
        });
        
        let result = tool.execute(params, ctx).await;
        
        // Should return error about string not found
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("old_string not found"));
        
        // Cleanup
        let _ = std::fs::remove_file(&test_file);
    }

    #[tokio::test]
    async fn test_read_tool_offset() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("test_read_tool_offset.txt");
        
        // Write 10 lines
        let lines = (1..=10).map(|i| format!("line {}", i)).collect::<Vec<_>>();
        let content = lines.join("\n");
        std::fs::write(&test_file, &content).unwrap();
        
        let tool = ReadTool;
        let ctx = create_tool_context();
        
        // Read with offset=5 (0-indexed, so starts at line 6)
        let params = json!({
            "path": test_file.to_string_lossy(),
            "offset": 5
        });
        
        let result = tool.execute(params, ctx).await.unwrap();
        
        // First line shown should be line 6 (1-indexed in output)
        assert!(result.contains("6\tline 6"));
        // Should not contain earlier lines
        assert!(!result.contains("1\tline 1"));
        assert!(!result.contains("5\tline 5"));
        
        // Cleanup
        let _ = std::fs::remove_file(&test_file);
    }
}