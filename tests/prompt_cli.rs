use serde_json::Value;
use std::{fs, process::Command};
use tempfile::tempdir;

fn synaps(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_synaps"))
        .args(args)
        .env_clear()
        .output()
        .expect("run synaps")
}

fn manifest(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("prompt.json");
    fs::write(&path, body).unwrap();
    (dir, path)
}

#[test]
fn validate_accepts_a_reference_only_manifest_without_runtime() {
    let (_dir, path) = manifest(
        r#"{"schema":"synaps-prompt/1","kernel":"kernel.base","adapters":["adapter.provider","adapter.model"]}"#,
    );
    let output = synaps(&["prompt", "validate", path.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn validate_rejects_invalid_schema_and_reference() {
    let (_dir, path) = manifest(r#"{"schema":"wrong","kernel":"kernel.base"}"#);
    assert!(!synaps(&["prompt", "validate", path.to_str().unwrap()])
        .status
        .success());

    let (_dir, path) =
        manifest(r#"{"schema":"synaps-prompt/1","kernel":"kernel.base","adapters":[""]}"#);
    assert!(!synaps(&["prompt", "validate", path.to_str().unwrap()])
        .status
        .success());
}

#[test]
fn inspect_emits_ordered_metadata_without_prompt_content() {
    let canary = "PROMPT_CONTENT_MUST_NOT_LEAK";
    let (_dir, path) = manifest(&format!(
        r#"{{"schema":"synaps-prompt/1","kernel":"kernel.base","adapters":["adapter.provider","adapter.model"],"modules":[{{"id":"canary.unused","version":"1","source":"user","priority":99,"selectors":{{}},"mutability":"mutable_guidance","content":"{canary}"}}]}}"#
    ));
    let output = synaps(&[
        "prompt",
        "inspect",
        "--manifest",
        path.to_str().unwrap(),
        "--model",
        "openai/gpt-4o",
        "--json",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains(canary));
    let json: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(json["model"], "openai/gpt-4o");
    assert_eq!(json["modules"][0]["id"], "kernel.base");
    assert_eq!(json["modules"][1]["id"], "adapter.provider");
    assert_eq!(json["modules"][2]["id"], "adapter.model");
    for module in json["modules"].as_array().unwrap() {
        assert_eq!(module["sha256"].as_str().unwrap().len(), 64);
        assert!(module.get("selectors").is_some());
        assert!(module.get("content").is_none());
    }
}

#[test]
fn inspect_rejects_unqualified_model() {
    let (_dir, path) = manifest(r#"{"schema":"synaps-prompt/1","kernel":"kernel.base"}"#);
    let output = synaps(&[
        "prompt",
        "inspect",
        "--manifest",
        path.to_str().unwrap(),
        "--model",
        "gpt-4o",
        "--json",
    ]);
    assert!(!output.status.success());
}
