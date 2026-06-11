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
        worktrees_dir.join("synaps-skills/plugin-maker-plugin/contracts/extensions.json"),
        monorepo_dir.join("synaps-skills/plugin-builder-plugin/contracts/extensions.json"),
        monorepo_dir.join("synaps-skills/plugin-maker-plugin/contracts/extensions.json"),
    ];
    // Cross-repo check: only meaningful on machines with the synaps-skills
    // repo checked out alongside this one AND shipping a contracts dir. The
    // plugin was renamed (plugin-builder → plugin-maker) and current versions
    // don't vendor a contract copy — skip instead of failing the whole suite
    // on a neighbor repo's layout.
    let Some(plugin_builder_contract) = candidates.iter().find(|path| path.exists()) else {
        eprintln!(
            "skipping contract sync check — no plugin-builder contract found at any of: {}",
            candidates
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        return;
    };

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
