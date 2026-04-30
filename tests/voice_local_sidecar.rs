use std::io::Write;
use std::process::{Command, Stdio};

fn local_sidecar_binary() -> String {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_synaps-voice-local") {
        return path;
    }
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.file_name().is_some_and(|name| name == "deps") {
        path.pop();
    }
    path.push(format!("synaps-voice-local{}", std::env::consts::EXE_SUFFIX));
    path.to_string_lossy().to_string()
}

#[test]
fn local_sidecar_shell_emits_ready_and_configured_fake_transcript() {
    let mut child = Command::new(local_sidecar_binary())
        .args(["--mock-transcript", "local shell transcript"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let stdin = child.stdin.as_mut().unwrap();
    writeln!(stdin, "{{\"type\":\"init\",\"config\":{{\"mode\":\"dictation\",\"language\":\"en\",\"protocol_version\":1}}}}").unwrap();
    writeln!(stdin, "{{\"type\":\"voice_control_pressed\"}}").unwrap();
    writeln!(stdin, "{{\"type\":\"voice_control_released\"}}").unwrap();
    writeln!(stdin, "{{\"type\":\"shutdown\"}}").unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(stdout.contains(r#"{"type":"hello","protocol_version":1,"extension":"synaps-voice-local""#), "{stdout}");
    assert!(stdout.contains(r#"{"type":"status","state":"ready","capabilities":["stt"]}"#), "{stdout}");
    assert!(stdout.contains(r#"{"type":"final_transcript","text":"local shell transcript"}"#), "{stdout}");
}

#[test]
fn local_sidecar_reports_missing_model_path_without_opening_mic() {
    let mut child = Command::new(local_sidecar_binary())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let stdin = child.stdin.as_mut().unwrap();
    writeln!(stdin, "{{\"type\":\"init\",\"config\":{{\"mode\":\"dictation\",\"language\":\"en\",\"protocol_version\":1}}}}").unwrap();
    writeln!(stdin, "{{\"type\":\"voice_control_pressed\"}}").unwrap();
    writeln!(stdin, "{{\"type\":\"shutdown\"}}").unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(stdout.contains(r#"{"type":"error","message":"local voice sidecar"#), "{stdout}");
    assert!(!stdout.contains("microphone"), "{stdout}");
}
