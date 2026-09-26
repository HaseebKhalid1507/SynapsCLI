//! Synthetic real-Axel forum integration. Never touch configured user storage.
use super::*;
use agent_core::config::{MemoryBackendConfig, MemoryBackendKind};
use agent_core::memory::store::ProjectScope;
use serde_json::Value;
use std::{path::Path, sync::Arc};

fn binding(base: &Path, scope: ProjectScope, exe: &Path) -> MemoryBinding {
    MemoryBinding::new(
        base.into(),
        Ok(scope),
        &MemoryBackendConfig {
            kind: MemoryBackendKind::Axel,
            executable: Some(exe.into()),
            brain: Some(base.join("brain.r8")),
            user_scope: true,
        },
    )
}
fn post(key: &str, thread: Option<String>) -> Post {
    Post {
        request_key: key.into(),
        title: if thread.is_some() {
            String::new()
        } else {
            "Shared findings".into()
        },
        thread_id: thread,
        reply_to: None,
        body: format!("synthetic forum finding {key}"),
        retention_days: 30,
    }
}
fn private_write(path: &Path, bytes: &[u8]) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}
#[tokio::test]
#[ignore = "requires SYNAPS_AXEL_TEST_BIN; isolated synthetic repository/brain only"]
async fn peers_concurrent_posts_scope_paging_export_and_deletion() {
    let exe = std::path::PathBuf::from(
        std::env::var_os("SYNAPS_AXEL_TEST_BIN").expect("explicit service path"),
    );
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let repo = base.join("repo");
    std::fs::create_dir(&repo).unwrap();
    let scope = ProjectScope::for_root(&repo).unwrap();
    let root = binding(&base, scope.clone(), &exe);
    let first = root.forum_post(post("root", None)).await.unwrap();
    assert_eq!(first.status, Status::Created);
    assert_eq!(
        root.forum_post(post("root", None)).await.unwrap().status,
        Status::Duplicate
    );
    let mut jobs = Vec::new();
    for n in 0..8 {
        // Separate binding => separate process + lock, not shared in-memory mutex.
        let peer = binding(&base, scope.clone(), &exe).fork_for_worker();
        let thread = first.id.clone();
        jobs.push(tokio::spawn(async move {
            peer.forum_post(post(&format!("peer-{n}"), Some(thread)))
                .await
                .unwrap()
        }));
    }
    for job in jobs {
        assert_eq!(job.await.unwrap().status, Status::Created);
    }
    let query = Read {
        thread_id: Some(first.id.clone()),
        limit: 3,
        ..Default::default()
    };
    let mut q = query.clone();
    let mut ids = std::collections::BTreeSet::new();
    loop {
        let page = root.forum_read(q.clone()).await.unwrap();
        for e in &page.entries {
            assert!(ids.insert(e.id.clone()));
        }
        match page.next {
            Some(c) => q.after = Some(c),
            None => break,
        }
    }
    assert_eq!(ids.len(), 9);
    assert!(root.search(Default::default()).await.unwrap().is_empty());
    assert!(root.fetch(&[&first.id]).await.is_err());
    let foreign_path = base.join("foreign");
    std::fs::create_dir(&foreign_path).unwrap();
    let foreign = root.with_scope(ProjectScope::for_root(&foreign_path).unwrap());
    assert!(foreign
        .forum_read(query.clone())
        .await
        .unwrap()
        .entries
        .is_empty());
    assert!(foreign
        .forum_post(post("foreign", Some(first.id.clone())))
        .await
        .is_err());
    assert!(foreign.forum_forget(&first.id).await.is_err());
    assert!(root
        .for_user_notes()
        .unwrap()
        .forum_read(Read::default())
        .await
        .is_err());
    let export = root.rpc("export", json!({"full":true})).await.unwrap();
    assert_eq!(export["records"].as_array().unwrap().len(), 9);
    let export_path = base.join("export.json");
    private_write(&export_path, &serde_json::to_vec(&export).unwrap());
    let restored_dir = base.join("restored");
    std::fs::create_dir(&restored_dir).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&restored_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let restored = binding(&restored_dir, scope.clone(), &exe);
    let source = super::super::migration::MigrationSource::Export {
        path: export_path,
        project: scope.key().into(),
    };
    let preview = super::super::migration::preview_from(&restored, &source)
        .await
        .unwrap();
    super::super::migration::apply_from(&restored, &source, &preview.manifest_digest)
        .await
        .unwrap();
    let all = Read {
        thread_id: Some(first.id.clone()),
        limit: 16,
        ..Default::default()
    };
    assert_eq!(
        root.forum_read(all.clone()).await.unwrap(),
        restored.forum_read(all.clone()).await.unwrap()
    );
    root.forum_forget(&first.id).await.unwrap();
    assert_eq!(
        root.forum_post(post("root", None)).await.unwrap().status,
        Status::Tombstoned
    );
    assert!(root
        .forum_post(post("new", Some(first.id.clone())))
        .await
        .is_err());
    assert_eq!(root.forum_read(all.clone()).await.unwrap().entries.len(), 8);
    let deleted_export = root.rpc("export", json!({"full":true})).await.unwrap();
    let deletion_path = base.join("deletion-export.json");
    private_write(
        &deletion_path,
        &serde_json::to_vec(&deleted_export).unwrap(),
    );
    let deletion = super::super::migration::MigrationSource::Export {
        path: deletion_path,
        project: scope.key().into(),
    };
    let preview = super::super::migration::preview_from(&restored, &deletion)
        .await
        .unwrap();
    super::super::migration::apply_from(&restored, &deletion, &preview.manifest_digest)
        .await
        .unwrap();
    assert_eq!(restored.forum_read(all).await.unwrap().entries.len(), 8);
    let old_preview = super::super::migration::preview_from(&restored, &source)
        .await
        .unwrap();
    super::super::migration::apply_from(&restored, &source, &old_preview.manifest_digest)
        .await
        .unwrap();
    assert!(restored
        .forum_read(Read::default())
        .await
        .unwrap()
        .entries
        .is_empty());
}

#[tokio::test]
#[ignore = "requires SYNAPS_AXEL_TEST_BIN; synthetic git worktree only"]
async fn repository_worktrees_share_forum_identity_after_reopen() {
    fn git(root: &Path, args: &[&str]) {
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap()
            .status
            .success());
    }
    let exe = std::path::PathBuf::from(std::env::var_os("SYNAPS_AXEL_TEST_BIN").unwrap());
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let repo = base.join("repo");
    let worktree = base.join("worktree");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "--quiet"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Synthetic",
            "-c",
            "user.email=syn@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "initial",
        ],
    );
    git(
        &repo,
        &["worktree", "add", "-qb", "peer", worktree.to_str().unwrap()],
    );
    let first = ProjectScope::discover_repository_with_override(&repo, None).unwrap();
    let second = ProjectScope::discover_repository_with_override(&worktree, None).unwrap();
    assert_eq!(first.scope.key(), second.scope.key());
    let root = binding(&base, first.scope.clone(), &exe);
    let peer = binding(&base, second.scope, &exe);
    let id = root.forum_post(post("root", None)).await.unwrap().id;
    assert_eq!(
        peer.forum_read(Read::default()).await.unwrap().entries[0].id,
        id
    );
    let reopened = binding(&base, first.scope, &exe);
    assert_ne!(root.forum_author(), reopened.forum_author());
    assert_eq!(
        reopened.forum_read(Read::default()).await.unwrap().entries[0].id,
        id
    );
    // State is captured; forking doesn't rediscover cwd or change shared lock.
    let fork = root.fork_for_worker();
    assert!(Arc::ptr_eq(&root.0.operation_lock, &fork.0.operation_lock));
}

// Exercises the model-facing normalization through the actual installed service,
// not only the in-process shared contract. No real project or configured brain.
#[tokio::test]
#[ignore = "requires SYNAPS_AXEL_TEST_BIN; synthetic tool-boundary forum roundtrip"]
async fn nullable_tool_arguments_roundtrip_and_rejections_are_actionable() {
    use crate::tools::{
        forum::{ForumForgetTool, ForumPostTool, ForumReadTool},
        Tool, ToolContext,
    };
    fn context(binding: &MemoryBinding) -> ToolContext {
        let mut context = crate::tools::test_helpers::create_tool_context();
        context.capabilities.memory_backend = Some(binding.clone());
        context
    }
    fn result(output: String) -> Value {
        let (banner, body) = output.split_once('\n').unwrap();
        assert_eq!(banner, crate::tools::forum::LOWER_AUTHORITY_HEADER);
        serde_json::from_str::<Value>(body).unwrap()["result"].clone()
    }
    let exe = std::path::PathBuf::from(std::env::var_os("SYNAPS_AXEL_TEST_BIN").unwrap());
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let host = binding(&base, ProjectScope::for_root(&base).unwrap(), &exe);
    let worker = host.fork_for_worker();
    let input = json!({"request_key":"null-root","title":"Root","body":"Synthetic note", "thread_id":null,"reply_to":null,"retention_days":null,"project":null});
    let receipt = result(
        ForumPostTool
            .execute(input.clone(), context(&worker))
            .await
            .unwrap(),
    );
    assert_eq!(receipt["status"], "created");
    assert_eq!(
        result(
            ForumPostTool
                .execute(input, context(&worker))
                .await
                .unwrap()
        )["status"],
        "duplicate"
    );
    let root_id = receipt["thread_id"].as_str().unwrap();
    let list = json!({"thread_id":null,"after":null,"query":null,"limit":null,"project":null});
    let page = result(
        ForumReadTool
            .execute(list.clone(), context(&host))
            .await
            .unwrap(),
    );
    assert_eq!(page["entries"].as_array().unwrap().len(), 1);
    assert_eq!(page["entries"][0]["id"], root_id);
    let reply = result(ForumPostTool.execute(json!({"request_key":"reply","title":null,"body":"Synthetic reply", "thread_id":root_id,"reply_to":null,"retention_days":null,"project":null}), context(&host)).await.unwrap());
    assert_eq!(reply["status"], "created");
    let thread = json!({"thread_id":root_id,"after":null,"query":null,"limit":null,"project":null});
    assert_eq!(
        result(
            ForumReadTool
                .execute(thread.clone(), context(&worker))
                .await
                .unwrap()
        )["entries"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    // Omitted/null host confirmation is not permission to select foreign scope.
    for project in ["", "p0000000000000000", "p1234567890123456"] {
        let mut bad = list.clone();
        bad["project"] = json!(project);
        assert!(ForumReadTool
            .execute(bad, context(&worker))
            .await
            .unwrap_err()
            .to_string()
            .contains("project confirmation"));
    }
    for digit in ['0', 'f'] {
        let fake = format!("msg-{}", digit.to_string().repeat(64));
        let e = ForumReadTool
            .execute(json!({"thread_id":fake}), context(&worker))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("placeholder"));
    }
    // A plausible but nonexistent ID must be rejected by the service, not be
    // turned into a root; expose its vetted error instead of a generic failure.
    let missing = format!("msg-{}", "12".repeat(32));
    let e = ForumPostTool.execute(json!({"request_key":"missing-parent","body":"Synthetic rejected reply","thread_id":missing}), context(&worker)).await.unwrap_err().to_string();
    assert!(e.contains("[not_found]"), "{e}");
    assert!(e.contains("no success receipt"), "{e}");
    assert!(e.contains("exact live thread ID"), "{e}");
    assert_eq!(
        result(
            ForumReadTool
                .execute(list.clone(), context(&worker))
                .await
                .unwrap()
        )["entries"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let deleted = result(
        ForumForgetTool
            .execute(json!({"id":reply["id"],"project":null}), context(&host))
            .await
            .unwrap(),
    );
    assert_eq!(deleted["status"], "tombstoned");
    assert_eq!(
        result(
            ForumReadTool
                .execute(thread, context(&worker))
                .await
                .unwrap()
        )["entries"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // Reopening the same scope retains the root, independent of actor lifetime.
    let reopened = binding(&base, ProjectScope::for_root(&base).unwrap(), &exe);
    assert_eq!(
        result(
            ForumReadTool
                .execute(list, context(&reopened))
                .await
                .unwrap()
        )["entries"][0]["id"],
        root_id
    );
}
