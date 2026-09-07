//! Synthetic real-service checks for one brain across repositories/worktrees.
use super::*;
use crate::tools::Tool;
use agent_core::memory::store::{MemoryProvenance, MemoryRetention};
use std::process::Command;

fn git(root: &Path, args: &[&str]) {
    let result = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}
fn note(body: &str) -> NewMemoryRecord {
    NewMemoryRecord {
        content: body.into(),
        tags: vec![],
        provenance: MemoryProvenance {
            source: "synthetic-test".into(),
            session: None,
        },
        sensitivity: MemorySensitivity::Normal,
        retention: MemoryRetention::Standard,
    }
}
fn binding(base: &Path, root: &Path, exe: &Path) -> MemoryBinding {
    let identity = ProjectScope::discover_repository_with_override(root, None).unwrap();
    let mut binding = MemoryBinding::new(
        base.to_path_buf(),
        Ok(identity.scope.clone()),
        &MemoryBackendConfig {
            kind: MemoryBackendKind::Axel,
            executable: Some(exe.to_path_buf()),
            brain: Some(base.join("shared.r8")),
            user_scope: true,
        },
    );
    Arc::get_mut(&mut binding.0).unwrap().repository = Some(identity);
    binding
}

#[tokio::test]
#[ignore = "requires SYNAPS_AXEL_TEST_BIN; synthetic repositories and brain only"]
async fn shared_brain_worktrees_moves_aliases_user_notes_and_foreign_isolation() {
    let exe =
        PathBuf::from(std::env::var_os("SYNAPS_AXEL_TEST_BIN").expect("explicit test service"));
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let repo = base.join("repo");
    let worktree = base.join("worktree");
    let foreign = base.join("foreign");
    std::fs::create_dir(&repo).unwrap();
    std::fs::create_dir(&foreign).unwrap();
    git(&repo, &["init", "--quiet"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Synthetic",
            "-c",
            "user.email=synthetic@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
            "--quiet",
        ],
    );
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "feature",
            worktree.to_str().unwrap(),
        ],
    );
    git(&foreign, &["init", "--quiet"]);
    let main = binding(&base, &repo, &exe);
    let worker = binding(&base, &worktree, &exe);
    let other = binding(&base, &foreign, &exe);
    assert_eq!(main.scope().unwrap().key(), worker.scope().unwrap().key());
    assert_ne!(main.scope().unwrap().key(), other.scope().unwrap().key());
    let saved = main.store(note("shared worktree knowledge")).await.unwrap();
    let id = saved.id.as_deref().unwrap();
    assert_eq!(worker.fetch(&[id]).await.unwrap()[0], saved);
    assert!(other.search(Default::default()).await.unwrap().is_empty());
    assert!(other.fetch(&[id]).await.is_err());
    assert!(other.forget(id).await.is_err());
    assert_eq!(main.fetch(&[id]).await.unwrap()[0], saved);
    assert!(
        saved.meta.as_ref().unwrap()["_synaps_repository"]["source_worktree_project"].is_string()
    );

    // Round-trip actual production metadata (including worktree provenance) and TTL.
    let inventory = main.rpc("export", json!({"full":true})).await.unwrap();
    let export_file = base.join("production-export.json");
    std::fs::write(&export_file, serde_json::to_vec(&inventory).unwrap()).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&export_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let restore_dir = base.join("restore");
    std::fs::create_dir(&restore_dir).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&restore_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let restore = MemoryBinding::new(
        base.clone(),
        Ok(main.scope().unwrap().clone()),
        &MemoryBackendConfig {
            kind: MemoryBackendKind::Axel,
            executable: Some(exe.clone()),
            brain: Some(restore_dir.join("restore.r8")),
            user_scope: false,
        },
    );
    let source = migration::MigrationSource::Export {
        path: export_file,
        project: main.scope().unwrap().key().into(),
    };
    let preview = migration::preview_from(&restore, &source).await.unwrap();
    migration::apply_from(&restore, &source, &preview.manifest_digest)
        .await
        .unwrap();
    let recovered = restore.fetch(&[id]).await.unwrap().remove(0);
    assert_eq!(recovered.content, saved.content);
    assert_eq!(recovered.id, saved.id);
    assert_eq!(
        recovered.meta.as_ref().unwrap()["_synaps_repository"],
        saved.meta.as_ref().unwrap()["_synaps_repository"]
    );

    // Existing path-key records remain under their original identities. Explicit
    // operator linking exposes them, never copies or rewrites their provenance.
    let legacy = main.with_scope(ProjectScope::for_root(&worktree).unwrap());
    assert_ne!(legacy.scope().unwrap().key(), main.scope().unwrap().key());
    let old = legacy
        .store(note("preexisting worktree note"))
        .await
        .unwrap();
    let old_id = old.id.as_deref().unwrap();
    assert!(main.fetch(&[old_id]).await.is_err());
    main.rpc("scope_alias", json!({"alias_project":legacy.scope().unwrap().key(),"alias_root":worktree,"canonical_root":repo})).await.unwrap();
    assert_eq!(main.fetch(&[old_id]).await.unwrap()[0], old);
    assert!(main
        .search(ProjectMemoryQuery {
            content_contains: Some("preexisting".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .iter()
        .any(|d| d.id == old_id));
    worker.forget(old_id).await.unwrap();
    assert!(legacy.fetch(&[old_id]).await.is_err());

    // Explicit user notes are shared, but never part of repository search/history.
    let user = main.for_user_notes().unwrap();
    let global = user.store(note("explicit user preference")).await.unwrap();
    let global_id = global.id.as_deref().unwrap();
    assert_eq!(
        other
            .for_user_notes()
            .unwrap()
            .fetch(&[global_id])
            .await
            .unwrap()[0],
        global
    );
    assert!(main.fetch(&[global_id]).await.is_err());
    assert!(user.history_search("", 1).await.is_err());
    assert!(user
        .rpc("capture_query", json!({"capture_id":"a".repeat(64)}))
        .await
        .is_err());
    let context = || {
        let mut c = crate::tools::test_helpers::create_tool_context();
        c.capabilities.memory_backend = Some(worker.clone());
        c
    };
    let found = crate::tools::memory::MemorySearchTool
        .execute(json!({"scope":"user","query":"explicit user"}), context())
        .await
        .unwrap();
    assert!(found.contains(global_id));
    assert!(crate::tools::memory::MemoryFetchTool
        .execute(json!({"scope":"user","ids":[global_id]}), context())
        .await
        .unwrap()
        .contains("explicit user preference"));

    // A directory move keeps canonical repository identity and previously stored IDs.
    let moved = base.join("moved-repo");
    std::fs::rename(&repo, &moved).unwrap();
    git(&moved, &["worktree", "repair"]);
    let reopened = binding(&base, &moved, &exe);
    assert_eq!(reopened.scope().unwrap().key(), main.scope().unwrap().key());
    assert_eq!(reopened.fetch(&[id]).await.unwrap()[0], saved);
    // Reusing an old pathname must NEVER reuse a repository's durable identity.
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "--quiet"]);
    let replacement = binding(&base, &repo, &exe);
    assert_ne!(
        replacement.scope().unwrap().key(),
        reopened.scope().unwrap().key()
    );
    assert!(replacement
        .search(Default::default())
        .await
        .unwrap()
        .is_empty());
    assert!(replacement.fetch(&[id]).await.is_err());
    assert!(replacement.forget(id).await.is_err());
    assert_eq!(reopened.fetch(&[id]).await.unwrap()[0], saved);
    // A reused legacy alias path must not resolve into its former linked group.
    git(&moved, &["worktree", "remove", worktree.to_str().unwrap()]);
    std::fs::create_dir(&worktree).unwrap();
    git(&worktree, &["init", "--quiet"]);
    let replacement_worker = binding(&base, &worktree, &exe);
    assert_ne!(
        replacement_worker.scope().unwrap().key(),
        reopened.scope().unwrap().key()
    );
    assert!(replacement_worker.fetch(&[id]).await.is_err());
    assert!(replacement_worker.forget(id).await.is_err());
    assert!(!base.join("context-archives").exists());
}

#[test]
fn default_shared_paths_and_user_scope_are_host_bound() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let scope = ProjectScope::for_root(&base).unwrap();
    let disabled = MemoryBinding::new(
        base.clone(),
        Ok(scope.clone()),
        &MemoryBackendConfig {
            kind: MemoryBackendKind::Axel,
            ..Default::default()
        },
    );
    assert_eq!(
        disabled.brain_path().unwrap(),
        base.join("memory/axel/brain.r8")
    );
    assert!(disabled.for_user_notes().is_err());
    assert!(!base.join("memory").exists());
    let legacy = MemoryBinding::new(
        base.clone(),
        Ok(scope),
        &MemoryBackendConfig {
            user_scope: true,
            ..Default::default()
        },
    );
    assert!(legacy.for_user_notes().is_err());
}
