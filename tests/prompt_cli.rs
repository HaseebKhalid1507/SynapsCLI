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
        r#"{"schema":"synaps-prompt/1","kernel":"kernel.base","adapters":["adapter.provider","adapter.model"],"modules":[{"id":"kernel.base","version":"1","source":"builtin","priority":0,"selectors":{},"mutability":"immutable_policy","content":"kernel"},{"id":"adapter.provider","version":"1","source":"builtin","priority":10,"selectors":{"provider":"openai"},"mutability":"immutable_policy","content":"provider"},{"id":"adapter.model","version":"1","source":"builtin","priority":20,"selectors":{"family":"gpt"},"mutability":"immutable_policy","content":"family"}]}"#,
    );
    let output = synaps(&[
        "prompt",
        "validate",
        path.to_str().unwrap(),
        "--model",
        "openai/gpt-4o",
        "--family",
        "gpt",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn validate_rejects_invalid_schema_and_reference() {
    let (_dir, path) = manifest(r#"{"schema":"wrong","kernel":"kernel.base"}"#);
    assert!(!synaps(&[
        "prompt",
        "validate",
        path.to_str().unwrap(),
        "--model",
        "openai/gpt-4o"
    ])
    .status
    .success());

    let (_dir, path) =
        manifest(r#"{"schema":"synaps-prompt/1","kernel":"kernel.base","adapters":[""]}"#);
    assert!(!synaps(&[
        "prompt",
        "validate",
        path.to_str().unwrap(),
        "--model",
        "openai/gpt-4o"
    ])
    .status
    .success());
}

#[test]
fn inspect_emits_ordered_metadata_without_prompt_content() {
    let canary = "PROMPT_CONTENT_MUST_NOT_LEAK";
    let (_dir, path) = manifest(&format!(
        r#"{{"schema":"synaps-prompt/1","kernel":"kernel.base","adapters":["adapter.provider","adapter.model"],"modules":[{{"id":"kernel.base","version":"1","source":"user","priority":0,"selectors":{{}},"mutability":"immutable_policy","content":"{canary}"}},{{"id":"adapter.provider","version":"1","source":"builtin","priority":10,"selectors":{{"provider":"openrouter"}},"mutability":"immutable_policy","content":"provider"}},{{"id":"adapter.model","version":"1","source":"builtin","priority":20,"selectors":{{"family":"gpt"}},"mutability":"immutable_policy","content":"family"}}]}}"#
    ));
    let output = synaps(&[
        "prompt",
        "inspect",
        "--manifest",
        path.to_str().unwrap(),
        "--model",
        "openrouter/openai/gpt-4o",
        "--family",
        "gpt",
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
    assert_eq!(json["foreground_model"], "openrouter/openai/gpt-4o");
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

#[test]
fn validate_compiles_real_modules_and_reports_ambiguity() {
    let (_dir, path) = manifest(
        r#"{"schema":"synaps-prompt/1","kernel":"kernel","adapters":["a","b"],"modules":[{"id":"kernel","version":"1","source":"user","priority":0,"selectors":{},"mutability":"immutable_policy","content":"k"},{"id":"a","version":"1","source":"user","priority":1,"selectors":{"provider":"openai"},"mutability":"mutable_guidance","content":"a"},{"id":"b","version":"1","source":"user","priority":1,"selectors":{"provider":"openai"},"mutability":"mutable_guidance","content":"b"}]}"#,
    );
    let output = synaps(&[
        "prompt",
        "validate",
        path.to_str().unwrap(),
        "--model",
        "openai/gpt",
    ]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ambiguous"));
}

#[test]
fn prompt_dispatch_is_offline_before_profile_initialization() {
    let (_dir, path) = manifest(
        r#"{"schema":"synaps-prompt/1","kernel":"kernel","modules":[{"id":"kernel","version":"1","source":"user","priority":0,"selectors":{},"mutability":"immutable_policy","content":"k"}]}"#,
    );
    let home = tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_synaps"))
        .args([
            "--profile",
            "../../bad",
            "prompt",
            "validate",
            path.to_str().unwrap(),
            "--model",
            "openai/gpt",
        ])
        .env_clear()
        .env("HOME", home.path())
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn manifest_paths_are_relative_and_inspection_is_safe() {
    let dir = tempdir().unwrap();
    fs::create_dir(dir.path().join("modules")).unwrap();
    fs::write(dir.path().join("modules/kernel.md"), "path bytes\n").unwrap();
    let path = dir.path().join("prompt.json");
    fs::write(&path, r#"{"schema":"synaps-prompt/1","kernel":"kernel","modules":[{"id":"kernel","version":"1","source":"user","path":"modules/kernel.md","priority":0,"selectors":{},"mutability":"immutable_policy"}]}"#).unwrap();
    let output = synaps(&[
        "prompt",
        "inspect",
        "--manifest",
        path.to_str().unwrap(),
        "--model",
        "openrouter/openai/gpt",
        "--json",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains("path bytes"));
    assert!(!text.contains(dir.path().to_str().unwrap()));
    let json: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(json["modules"][0]["safe_source"], "modules/kernel.md");
    assert_eq!(
        json["modules"][0]["sha256"],
        "a7d3a3ec56663700b396c4388c6e768624075a358653616edc8fc38e96bfc2a2"
    );
}

#[test]
fn validate_requires_qualified_model() {
    let (_dir, path) = manifest(
        r#"{"schema":"synaps-prompt/1","kernel":"kernel","modules":[{"id":"kernel","version":"1","source":"user","priority":0,"selectors":{},"mutability":"immutable_policy","content":"k"}]}"#,
    );
    assert!(!synaps(&["prompt", "validate", path.to_str().unwrap()])
        .status
        .success());
}
