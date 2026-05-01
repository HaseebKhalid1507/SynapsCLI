#[test]
fn plugin_builder_extension_contract_matches_synaps_contract() {
    let synaps = std::fs::read_to_string("docs/extensions/contract.json")
        .expect("Synaps extension contract should exist");
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let worktrees_dir = manifest_dir
        .parent()
        .expect("SynapsCLI should live under a parent directory");
    let monorepo_dir = worktrees_dir
        .parent()
        .expect("worktree parent should have a parent directory");

    let candidates = [
        worktrees_dir.join("synaps-skills-skill-extension-builder/plugin-builder-plugin/contracts/extensions.json"),
        worktrees_dir.join("synaps-skills/plugin-builder-plugin/contracts/extensions.json"),
        monorepo_dir.join("synaps-skills/plugin-builder-plugin/contracts/extensions.json"),
    ];
    let plugin_builder_contract = candidates
        .iter()
        .find(|path| path.exists())
        .unwrap_or_else(|| {
            panic!(
                "plugin-builder extension contract should exist at one of: {}",
                candidates
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        });

    let builder = std::fs::read_to_string(plugin_builder_contract).unwrap_or_else(|err| {
        panic!(
            "failed to read plugin-builder extension contract at {}: {}",
            plugin_builder_contract.display(),
            err
        )
    });

    let synaps_json: serde_json::Value = serde_json::from_str(&synaps).unwrap();
    let builder_json: serde_json::Value = serde_json::from_str(&builder).unwrap();
    assert_eq!(builder_json, synaps_json);
}
