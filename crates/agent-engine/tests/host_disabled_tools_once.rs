//! `disabled_tools` is applied exactly once, on the fresh registry BEFORE
//! skills/MCP register (the old `apply_config` point). A second pass inside
//! `foreground_runtime()` would see `load_skill`/`search_skills` and strip
//! them — the old boot never did. Own process (config on disk).

use agent_engine::{EngineHost, HostOpts};

#[tokio::test]
async fn disabled_tools_hits_builtins_only_and_never_skill_tools() {
    let home = std::env::temp_dir().join(format!("synaps-disable-once-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config"),
        "disabled_tools = ls, load_skill, search_skills\n",
    )
    .unwrap();
    agent_engine::config::set_base_dir_for_tests(home);

    let h = EngineHost::boot(HostOpts {
        profile: None,
        no_extensions: true,
    })
    .await
    .expect("host boot");
    assert!(EngineHost::install(h).is_ok());
    let h = EngineHost::current().unwrap();
    let gen_after_boot = h.parts().tools.read().await.catalog().generation();

    let fg = h.foreground_runtime().await.unwrap();
    let reg = fg.tools_shared();
    let reg = reg.read().await;
    assert!(reg.get("ls").is_none(), "builtin disabled at boot");
    assert!(reg.get("bash").is_some());
    // Registered after the disable pass → untouched, as before.
    assert!(reg.get("load_skill").is_some(), "skill tools survive");
    assert!(reg.get("search_skills").is_some(), "skill tools survive");
    // No second disable pass: the shared catalog generation is unchanged.
    assert_eq!(reg.catalog().generation(), gen_after_boot);
}
