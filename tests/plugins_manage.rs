//! End-to-end: add marketplace → install → uninstall, with a local HTTP
//! server for metadata and a local bare git repo as the plugin source.

use std::process::Command;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

fn mk_plugin_repo(tmp: &std::path::Path) -> std::path::PathBuf {
    let work = tmp.join("work");
    std::fs::create_dir_all(&work).unwrap();
    Command::new("git").args(["init", "-q"]).current_dir(&work).status().unwrap();
    Command::new("git").args(["config", "user.email", "t@t"]).current_dir(&work).status().unwrap();
    Command::new("git").args(["config", "user.name", "t"]).current_dir(&work).status().unwrap();
    std::fs::write(work.join("SKILL.md"),
        "---\nname: web\ndescription: Web tools\n---\nbody").unwrap();
    // required plugin.json so the loader picks it up
    std::fs::create_dir_all(work.join(".synaps-plugin")).unwrap();
    std::fs::write(
        work.join(".synaps-plugin").join("plugin.json"),
        r#"{"name":"web"}"#,
    ).unwrap();
    // Move SKILL.md under a skills/ subdir as the loader expects.
    std::fs::create_dir_all(work.join("skills").join("search")).unwrap();
    std::fs::rename(work.join("SKILL.md"),
        work.join("skills").join("search").join("SKILL.md")).unwrap();
    Command::new("git").args(["add", "."]).current_dir(&work).status().unwrap();
    Command::new("git").args(["commit", "-q", "-m", "init"]).current_dir(&work).status().unwrap();
    let bare = tmp.join("bare.git");
    Command::new("git").args(["clone", "--bare", "-q",
        work.to_str().unwrap(), bare.to_str().unwrap()]).status().unwrap();
    bare
}

async fn serve_json_once(body: String) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(), body
        );
        sock.write_all(resp.as_bytes()).await.unwrap();
    });
    port
}

#[tokio::test]
async fn end_to_end_add_install_uninstall() {
    use synaps_cli::skills::{state::*, install, marketplace};

    let tmp = tempfile::tempdir().unwrap();
    let bare = mk_plugin_repo(tmp.path());
    let file_url = format!("file://{}", bare.display());

    // Serve a marketplace.json pointing at the bare repo.
    let body = format!(
        r#"{{"name":"mk","plugins":[{{"name":"web","source":"{}"}}]}}"#,
        file_url
    );
    let port = serve_json_once(body).await;
    let metadata_url = format!("http://127.0.0.1:{}/mk", port);

    // Step 1: fetch marketplace manifest.
    let manifest = marketplace::fetch_raw(&metadata_url).await.unwrap();
    let m: synaps_cli::skills::manifest::MarketplaceManifest =
        serde_json::from_str(&manifest).unwrap();

    let mut state = PluginsState::default();
    state.marketplaces.push(Marketplace {
        name: m.name.clone(),
        url: metadata_url.clone(),
        description: None,
        last_refreshed: Some("now".into()),
        cached_plugins: m.plugins.iter().map(|p| CachedPlugin {
            name: p.name.clone(),
            source: p.source.clone(),
            version: None,
            description: None,
        }).collect(),
        repo_url: None,
    });

    // Step 2: install.
    let dest = tmp.path().join("plugins").join("web");
    let sha = install::install_plugin(&file_url, &dest).unwrap();
    assert!(dest.join(".synaps-plugin").join("plugin.json").exists());
    state.installed.push(InstalledPlugin {
        name: "web".into(),
        marketplace: Some("mk".into()),
        source_url: file_url.clone(),
        installed_commit: sha,
        latest_commit: None,
        installed_at: "now".into(),
        source_subdir: None,
    });

    let state_path = tmp.path().join("plugins.json");
    state.save_to(&state_path).unwrap();
    let reloaded = PluginsState::load_from(&state_path).unwrap();
    assert_eq!(reloaded.marketplaces.len(), 1);
    assert_eq!(reloaded.installed.len(), 1);
    assert_eq!(reloaded.installed[0].installed_commit, state.installed[0].installed_commit);
    assert_eq!(reloaded.marketplaces[0].cached_plugins[0].source, file_url);

    // Step 3: uninstall.
    install::uninstall_plugin(&dest).unwrap();
    assert!(!dest.exists());
}
