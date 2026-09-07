//! Explicit NOTE scope through the short tools and the pinned Axel service.
//! Opt-in only: SYNAPS_AXEL_TEST_BIN=/absolute/service cargo test -p synaps-engine
//! --test memory_user_scope -- --ignored --test-threads=1
//! All repositories, storage and bodies are synthetic temporary fixtures. No
//! user config, existing brain, migration, or live repository identity is used.
#![cfg(unix)]

use agent_core::config::{MemoryBackendConfig, MemoryBackendKind};
use agent_engine::memory_backend::MemoryBinding;
use agent_engine::tools::memory::{
    MemoryFetchTool, MemoryForgetTool, MemorySearchTool, MemoryStoreTool, LOWER_AUTHORITY_HEADER,
};
use agent_engine::tools::{Tool, ToolCapabilities, ToolChannels, ToolContext, ToolLimits};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::Path;

const USER_KEY: &str = "p0000000000000000";

// Separate integration-test process; the shared serial key also prevents any
// future env-mutating tests here from racing these captured host bindings.
struct EnvGuard {
    key: &'static str,
    old: Option<OsString>,
}
impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let old = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, old }
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.old {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

fn context(binding: &MemoryBinding) -> ToolContext {
    ToolContext {
        channels: ToolChannels {
            tx_delta: None,
            tx_events: None,
        },
        capabilities: ToolCapabilities {
            launch_cancel: None,
            memory_backend: Some(binding.clone()),
            watcher_exit_path: None,
            tool_register_tx: None,
            session_manager: None,
            subagent_registry: None,
            event_queue: None,
            delegation_parent: None,
            codex_parent_plan: None,
            secret_prompt: None,
            orchestration: None,
            tool_activation: None,
            mcp_leases: None,
            extension_leases: None,
            memory_context: None,
        },
        limits: ToolLimits {
            max_tool_output: 30_000,
            max_tool_buffer: 256 * 1024,
            bash_timeout: 30,
            bash_max_timeout: 300,
            subagent_timeout: 300,
        },
    }
}

fn binding(config: &MemoryBackendConfig, root: &Path) -> MemoryBinding {
    let _root = EnvGuard::set("SYNAPS_PROJECT_ROOT", root);
    MemoryBinding::from_config(config)
}

async fn run(tool: &dyn Tool, params: Value, binding: &MemoryBinding) -> String {
    tool.execute(params, context(binding)).await.unwrap()
}

fn stored_id(output: &str) -> String {
    output
        .split_whitespace()
        .find(|word| word.starts_with("mem-"))
        .expect("store output must carry its exact stable ID")
        .to_owned()
}

#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
#[ignore = "requires explicitly supplied pinned Axel service; temporary synthetic data only"]
async fn explicit_user_notes_are_shared_but_repository_notes_and_history_are_not() {
    let executable = std::path::PathBuf::from(
        std::env::var_os("SYNAPS_AXEL_TEST_BIN").expect("explicit synthetic-test service path"),
    );
    assert!(executable.is_absolute());
    let temp = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let _base = EnvGuard::set("SYNAPS_BASE_DIR", temp.path());
    let root_a = temp.path().join("repository-a");
    let root_b = temp.path().join("repository-b");
    std::fs::create_dir(&root_a).unwrap();
    std::fs::create_dir(&root_b).unwrap();
    let config = MemoryBackendConfig {
        kind: MemoryBackendKind::Axel,
        executable: Some(executable),
        brain: Some(temp.path().join("synthetic.r8")),
        user_scope: true,
    };
    let a = binding(&config, &root_a);
    let b = binding(&config, &root_b);
    let key_a = a.scope().unwrap().key().to_owned();
    assert_ne!(key_a, b.scope().unwrap().key());
    assert_ne!(key_a, USER_KEY);

    // Omitted scope still stores in the repository even when user notes are enabled.
    let local = run(
        &MemoryStoreTool,
        json!({"content":"SYNTHETIC_REPOSITORY_ONLY_81", "project":key_a}),
        &a,
    )
    .await;
    assert!(local.contains("repository scope"));
    let local_id = stored_id(&local);
    let shared = run(
        &MemoryStoreTool,
        json!({"scope":"user", "project":USER_KEY, "content":"SYNTHETIC_USER_NOTE_26"}),
        &a,
    )
    .await;
    assert!(shared.contains("user scope"));
    assert!(shared.contains(USER_KEY));
    let shared_id = stored_id(&shared);

    // User notes from A are visible from B, only with an explicit user scope.
    let search = run(
        &MemorySearchTool,
        json!({"scope":"user", "project":USER_KEY, "query":"SYNTHETIC_USER_NOTE"}),
        &b,
    )
    .await;
    assert!(search.starts_with(LOWER_AUTHORITY_HEADER));
    assert!(search.contains(&shared_id));
    assert!(!search.contains(&local_id));
    let fetched = run(
        &MemoryFetchTool,
        json!({"scope":"user", "project":USER_KEY, "ids":[shared_id]}),
        &b,
    )
    .await;
    assert!(fetched.starts_with(LOWER_AUTHORITY_HEADER));
    assert!(fetched.contains("SYNTHETIC_USER_NOTE_26"));
    assert!(fetched.contains("user scope"));

    for repo in [&a, &b] {
        for params in [json!({}), json!({"scope":"repository"})] {
            let search = run(&MemorySearchTool, params, repo).await;
            assert!(!search.contains(&shared_id));
            if repo.scope().unwrap().key() == key_a {
                assert!(search.contains(&local_id));
            } else {
                assert!(!search.contains(&local_id));
            }
        }
        assert!(MemoryFetchTool
            .execute(json!({"ids":[shared_id]}), context(repo))
            .await
            .is_err());
        assert!(MemoryFetchTool
            .execute(json!({"scope":"user", "ids":[local_id]}), context(repo))
            .await
            .is_err());
    }

    // Project confirms the selected host scope; it never implicitly selects user.
    for tool in [
        &MemorySearchTool as &dyn Tool,
        &MemoryFetchTool,
        &MemoryStoreTool,
        &MemoryForgetTool,
    ] {
        for (scope, project) in [("repository", USER_KEY), ("user", key_a.as_str())] {
            let err = tool
                .execute(
                    json!({"scope":scope, "project":project, "content":"synthetic denied", "ids":[shared_id], "id":shared_id}),
                    context(&a),
                )
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("cross-project"),
                "{}: {err}",
                tool.name()
            );
        }
    }

    // No history dispatch in user scope, even with valid opt-in and a real backend.
    for (tool, params) in [
        (
            &MemorySearchTool as &dyn Tool,
            json!({"scope":"user", "source":"history"}),
        ),
        (
            &MemoryFetchTool,
            json!({"scope":"user", "ids":[shared_id,"ctx-synthetic"]}),
        ),
        (
            &MemoryForgetTool,
            json!({"scope":"user", "id":"ctx-synthetic"}),
        ),
    ] {
        let err = tool.execute(params, context(&a)).await.unwrap_err();
        assert!(err.to_string().contains("notes-only"));
    }

    let denied = binding(
        &MemoryBackendConfig {
            user_scope: false,
            ..config
        },
        &root_b,
    );
    for tool in [
        &MemorySearchTool as &dyn Tool,
        &MemoryFetchTool,
        &MemoryStoreTool,
        &MemoryForgetTool,
    ] {
        let err = tool.execute(
            json!({"scope":"user", "content":"synthetic denied", "ids":[shared_id], "id":shared_id}),
            context(&denied),
        ).await.unwrap_err();
        assert!(err.to_string().contains("memory.user_scope = true"));
    }

    // A cross-scope forget cannot delete the note; explicit user forget from B can.
    assert!(MemoryForgetTool
        .execute(json!({"id":shared_id}), context(&a))
        .await
        .is_err());
    run(
        &MemoryForgetTool,
        json!({"scope":"user", "project":USER_KEY, "id":shared_id}),
        &b,
    )
    .await;
    assert!(MemoryFetchTool
        .execute(json!({"scope":"user", "ids":[shared_id]}), context(&a))
        .await
        .is_err());
    assert!(!run(&MemorySearchTool, json!({"scope":"user"}), &a)
        .await
        .contains(&shared_id));
    assert!(run(&MemoryFetchTool, json!({"ids":[local_id]}), &a)
        .await
        .contains("SYNTHETIC_REPOSITORY_ONLY_81"));
    assert_eq!(a.scope().unwrap().key(), key_a);
    assert!(!temp.path().join("context-archives").exists());
    assert!(!temp.path().join("memory").exists());
    assert!(!temp.path().join("config").exists());
}
