use agent_core::prompt::PromptManifest;
use std::fs;

fn manifest(path: &str) -> String {
    format!("schema: synaps-prompt/1\nkernel: kernel\nmodules:\n- id: kernel\n  version: v\n  source: user\n  path: {path}\n  priority: 0\n  selectors: {{}}\n  mutability: immutable_policy\n")
}

#[test]
fn module_paths_cannot_escape_manifest_directory_lexically() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("outside"), "CANARY").unwrap();
    let dir = root.path().join("manifest");
    fs::create_dir(&dir).unwrap();
    let error = PromptManifest::parse(&manifest("../outside"))
        .unwrap()
        .registry(Some(&dir))
        .err()
        .expect("escape rejected")
        .to_string();
    assert!(error.contains("confined"));
    assert!(!error.contains("outside") && !error.contains("CANARY"));
}

#[cfg(unix)]
#[test]
fn module_paths_cannot_escape_manifest_directory_through_symlinks() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("secret"), "CANARY").unwrap();
    let dir = root.path().join("manifest");
    fs::create_dir(&dir).unwrap();
    symlink(root.path().join("secret"), dir.join("module")).unwrap();
    let error = PromptManifest::parse(&manifest("module"))
        .unwrap()
        .registry(Some(&dir))
        .err()
        .expect("escape rejected")
        .to_string();
    assert!(error.contains("confined"));
    assert!(!error.contains("secret") && !error.contains("CANARY"));
}
