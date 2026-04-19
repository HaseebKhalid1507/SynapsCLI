//! Integration tests for interactive shell sessions.

use synaps_cli::tools::shell::{SessionManager, session::{SessionOpts, SendResult}};
use synaps_cli::tools::shell::config::ShellConfig;
use std::collections::HashMap;
use std::time::Duration;

fn default_opts() -> SessionOpts {
    SessionOpts {
        command: None,
        working_directory: None,
        env: HashMap::new(),
        rows: None,
        cols: None,
        readiness_timeout_ms: None,
        idle_timeout: None,
    }
}

#[tokio::test]
async fn test_basic_echo() {
    let manager = SessionManager::new(ShellConfig::default());
    
    let mut opts = default_opts();
    opts.command = Some("bash".to_string());
    
    let (session_id, _initial, _status) = manager.create_session(opts, None).await.expect("create session");
    
    let result = manager.send_input(&session_id, "echo hello\n", Some(1000), None).await.expect("send input");
    
    assert!(result.output.contains("hello"), "Expected 'hello' in output: {}", result.output);
    assert_eq!(result.status, "active");
    
    manager.close_session(&session_id).await.expect("close session");
}

#[tokio::test]
async fn test_python_repl() {
    let manager = SessionManager::new(ShellConfig::default());
    
    let mut opts = default_opts();
    opts.command = Some("python3".to_string());
    
    let (session_id, _initial, _status) = manager.create_session(opts, None).await.expect("create session");
    
    // Wait for Python to start up fully
    tokio::time::sleep(Duration::from_millis(500)).await;
    
    let result = manager.send_input(&session_id, "print(1+1)\n", Some(2000), None).await.expect("send input");
    
    // Python output might have echoes - check for "2" but be flexible about format
    let output_lower = result.output.to_lowercase();
    assert!(output_lower.contains("2") || result.output.trim().ends_with("2"), 
            "Expected '2' in Python output: '{}'", result.output);
    
    let _exit_result = manager.send_input(&session_id, "exit()\n", Some(1000), None).await;
    
    manager.close_session(&session_id).await.expect("close session");
}

#[tokio::test]
async fn test_ctrl_c_interrupt() {
    let manager = SessionManager::new(ShellConfig::default());
    
    let mut opts = default_opts();
    opts.command = Some("bash".to_string());
    
    let (session_id, _initial, _status) = manager.create_session(opts, None).await.expect("create session");
    
    // Start a long-running process
    let _result = manager.send_input(&session_id, "sleep 999\n", Some(500), None).await;
    
    tokio::time::sleep(Duration::from_millis(200)).await;
    
    // Send Ctrl-C
    let interrupt_result = manager.send_input(&session_id, "\x03", Some(1000), None).await.expect("send ctrl-c");
    assert_eq!(interrupt_result.status, "active", "Session should still be active after Ctrl-C");
    
    // Verify session is still responsive
    let test_result = manager.send_input(&session_id, "echo test\n", Some(1000), None).await.expect("send test echo");
    assert!(test_result.output.contains("test"), "Session should still respond after Ctrl-C");
    
    manager.close_session(&session_id).await.expect("close session");
}

#[tokio::test]
async fn test_ctrl_d_eof() {
    let manager = SessionManager::new(ShellConfig::default());
    
    let mut opts = default_opts();
    opts.command = Some("cat".to_string());
    
    let (session_id, _initial, _status) = manager.create_session(opts, None).await.expect("create session");
    
    tokio::time::sleep(Duration::from_millis(200)).await;
    
    // Send EOF (Ctrl-D)
    let result = manager.send_input(&session_id, "\x04", Some(1000), None).await.expect("send eof");
    assert_eq!(result.status, "exited", "Process should have exited after EOF");
    
    manager.close_session(&session_id).await.expect("close session");
}

#[tokio::test]
async fn test_working_directory() {
    let manager = SessionManager::new(ShellConfig::default());
    
    let mut opts = default_opts();
    opts.command = Some("bash".to_string());
    opts.working_directory = Some("/tmp".to_string());
    
    let (session_id, _initial, _status) = manager.create_session(opts, None).await.expect("create session");
    
    let result = manager.send_input(&session_id, "pwd\n", Some(1000), None).await.expect("send pwd");
    
    assert!(result.output.contains("/tmp"), "Expected '/tmp' in pwd output: {}", result.output);
    
    manager.close_session(&session_id).await.expect("close session");
}

#[tokio::test]
async fn test_environment_variables() {
    let manager = SessionManager::new(ShellConfig::default());
    
    let mut env = HashMap::new();
    env.insert("MY_VAR".to_string(), "test123".to_string());
    
    let mut opts = default_opts();
    opts.command = Some("bash".to_string());
    opts.env = env;
    
    let (session_id, _initial, _status) = manager.create_session(opts, None).await.expect("create session");
    
    let result = manager.send_input(&session_id, "echo $MY_VAR\n", Some(1000), None).await.expect("send echo env var");
    
    assert!(result.output.contains("test123"), "Expected 'test123' in output: {}", result.output);
    
    manager.close_session(&session_id).await.expect("close session");
}

#[tokio::test]
async fn test_max_sessions() {
    let config = ShellConfig {
        max_sessions: 2,
        ..Default::default()
    };
    let manager = SessionManager::new(config);
    
    let mut opts1 = default_opts();
    opts1.command = Some("bash".to_string());
    let mut opts2 = default_opts();
    opts2.command = Some("bash".to_string());
    let mut opts3 = default_opts();
    opts3.command = Some("bash".to_string());
    
    // Create first two sessions successfully
    let (session1, _, _) = manager.create_session(opts1, None).await.expect("create first session");
    let (session2, _, _) = manager.create_session(opts2, None).await.expect("create second session");
    
    // Third session should fail
    let result = manager.create_session(opts3, None).await;
    assert!(result.is_err(), "Third session should fail due to limit");
    assert!(result.unwrap_err().to_string().contains("maximum session limit"));
    
    manager.close_session(&session1).await.expect("close session1");
    manager.close_session(&session2).await.expect("close session2");
}

#[tokio::test]
async fn test_session_not_found() {
    let manager = SessionManager::new(ShellConfig::default());
    
    let result = manager.send_input("shell_99", "echo test\n", Some(1000), None).await;
    
    assert!(result.is_err(), "Should error for non-existent session");
    assert!(result.unwrap_err().to_string().contains("not found"));
}

#[tokio::test]
async fn test_double_close() {
    let manager = SessionManager::new(ShellConfig::default());
    
    let mut opts = default_opts();
    opts.command = Some("bash".to_string());
    
    let (session_id, _initial, _status) = manager.create_session(opts, None).await.expect("create session");
    
    // First close should work
    let result1 = manager.close_session(&session_id).await;
    assert!(result1.is_ok(), "First close should succeed");
    
    // Second close should also work (idempotent)
    let result2 = manager.close_session(&session_id).await;
    assert!(result2.is_ok(), "Second close should succeed (idempotent)");
    assert_eq!(result2.unwrap(), "", "Second close should return empty string");
}

#[tokio::test]
async fn test_process_exit() {
    let manager = SessionManager::new(ShellConfig::default());
    
    let mut opts = default_opts();
    opts.command = Some("bash -c \"exit 42\"".to_string());
    
    let (session_id, _initial, _status) = manager.create_session(opts, None).await.expect("create session");
    
    tokio::time::sleep(Duration::from_millis(200)).await;
    
    // Try to send input to exited process
    let result = manager.send_input(&session_id, "echo test\n", Some(1000), None).await;
    
    // Should fail because process has exited
    assert!(result.is_err(), "Should fail to send to exited process");
    assert!(result.unwrap_err().to_string().contains("is not active"));
    
    manager.close_session(&session_id).await.expect("close session");
}