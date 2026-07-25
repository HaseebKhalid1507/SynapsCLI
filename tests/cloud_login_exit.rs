use std::process::Command;

#[test]
fn every_cloud_login_failure_exits_nonzero() {
    let exe = env!("CARGO_BIN_EXE_synaps");
    for provider in ["azure-openai", "aws-bedrock", "google-vertex"] {
        let home = tempfile::tempdir().unwrap();
        let status = Command::new(exe)
            .args(["login", "--provider", provider])
            .env("HOME", home.path())
            .env("SYNAPS_HOME", home.path())
            .env_remove("SYNAPS_AZURE_CLIENT_ID")
            .env_remove("SYNAPS_VERTEX_CLIENT_ID")
            .env_remove("SYNAPS_GOOGLE_VERTEX_CLIENT_ID")
            .env_remove("SYNAPS_AWS_START_URL")
            .status()
            .unwrap();
        assert!(
            !status.success(),
            "{provider} login failure returned success"
        );
    }
}
