//! Daemon-mode phase 2 (C3): extension notification router → every live
//! session. One sidecar, two actor-hosted sessions, one `widget.upsert`
//! frame → both `LocalTransport`s receive `ExtensionNotification`.

use std::sync::Arc;
use std::time::Duration;

use agent_engine::config;
use agent_engine::extensions::hooks::events::HookEvent;
use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, LocalTransport, SessionConfig, SessionEventWire,
};
use agent_engine::{EngineHost, HostOpts};

fn install_widget_plugin(home: &std::path::Path) {
    let plugin_dir = home.join("plugins/widget-notify-test");
    std::fs::create_dir_all(plugin_dir.join(".synaps-plugin")).unwrap();
    std::fs::create_dir_all(plugin_dir.join("extensions")).unwrap();
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/widget_notify_extension.py"),
        plugin_dir.join("extensions/widget_notify_extension.py"),
    )
    .unwrap();
    std::fs::write(
        plugin_dir.join(".synaps-plugin/plugin.json"),
        r#"{
  "name": "widget-notify-test",
  "version": "0.1.0",
  "extension": {
    "protocol_version": 1,
    "runtime": "process",
    "command": "python3",
    "args": ["extensions/widget_notify_extension.py"],
    "permissions": ["privacy.llm_content"],
    "hooks": [{"hook": "before_message"}]
  }
}
"#,
    )
    .unwrap();
}

async fn next_notification(t: &mut LocalTransport) -> Option<(String, String, serde_json::Value)> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let ev = tokio::time::timeout_at(deadline, t.next_event()).await.ok()??;
        if let SessionEventWire::ExtensionNotification {
            extension_id,
            method,
            params,
        } = ev.event
        {
            return Some((extension_id, method, params));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_router_reaches_every_session() {
    let home = std::env::temp_dir().join(format!("synaps-notify-router-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    config::set_base_dir_for_tests(home.clone());
    install_widget_plugin(&home);

    let host = EngineHost::boot_and_install(HostOpts {
        profile: None,
        no_extensions: false,
    })
    .await
    .expect("host boot");

    // Process-level discovery, once.
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    agent_engine::extensions::loader::spawn_discover_and_load(
        Arc::clone(host.ext_manager()),
        tx,
        None,
    );
    host.extensions_ready().await;
    let (loaded, failed) = host.ext_manager().read().await.discovery_done().unwrap();
    assert_eq!(loaded, vec!["widget-notify-test".to_string()]);
    assert!(failed.is_empty(), "{failed:?}");
    agent_engine::extensions::notify_router::spawn_notification_router(Arc::clone(&host))
        .await
        .unwrap();

    // Two sessions on the same host, each with an attached local client.
    let mut transports = Vec::new();
    for _ in 0..2 {
        let handle = host
            .create_session(SessionConfig {
                persist: false,
                ..SessionConfig::default()
            })
            .await
            .expect("create_session");
        let (t, _snapshot) = LocalTransport::attach(handle, ClientMeta::new(ClientKind::Test))
            .await
            .expect("attach");
        transports.push(t);
    }
    assert_eq!(host.sessions().len(), 2);
    let ids: std::collections::HashSet<_> = transports
        .iter()
        .map(|t| t.session_id().clone())
        .collect();
    assert_eq!(ids.len(), 2, "distinct sessions");

    // Trigger the sidecar: one hook → one widget.upsert frame.
    let hook_bus = Arc::clone(host.ext_manager().read().await.hook_bus());
    let _ = hook_bus
        .emit(&HookEvent::before_message("ping").with_session(Some("sess-trigger")))
        .await;

    // Both transports receive it (broadcast: frames carry no session id).
    for t in transports.iter_mut() {
        let (ext, method, params) = next_notification(t)
            .await
            .unwrap_or_else(|| panic!("session {} never received the frame", t.session_id()));
        assert_eq!(ext, "widget-notify-test");
        assert_eq!(method, "widget.upsert");
        assert_eq!(params["id"], "c3-widget");
        assert_eq!(params["lines"][0], "beat 1 from sess-trigger");
    }

    // Teardown: end both sessions (each fires its own on_session_end).
    for t in transports.iter() {
        let _ = t
            .send(agent_engine::session::SessionCommand::End {
                reason: agent_engine::session::EndReason::ClientQuit,
            })
            .await;
    }
    host.ext_manager().write().await.shutdown_all().await;
    let _ = std::fs::remove_dir_all(&home);
}
