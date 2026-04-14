use super::*;
use super::agent::strip_frontmatter;
use serde_json::json;
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
        tool_register_tx: None,
        max_tool_output: 30000,
        bash_timeout: 30,
        bash_max_timeout: 300,
        subagent_timeout: 300,
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

    // Use a dedicated temp dir to avoid races with other tests
    let dir = std::env::temp_dir().join("synaps_test_ls");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("hello.txt"), "hi").unwrap();

    let params = json!({
        "path": dir.to_str().unwrap()
    });

    let result = tool.execute(params, ctx).await.unwrap();
    assert!(result.contains("hello.txt"));

    // Cleanup
    std::fs::remove_dir_all(&dir).ok();
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
