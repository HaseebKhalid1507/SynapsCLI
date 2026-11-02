//! The daemon writes the saved usage readings (p1-r1 B1/B2, p1-r2 B3/SF2).
//! `Daemon::start` takes their lease once it holds the daemon lock, before
//! any restored turn can read usage; a start that finds the lock held never
//! does. Broker selections in the started daemon process (on the
//! process-wide usage-read table), all at once, then run on fresh saved
//! readings with no usage request, and a reported limit drops the account's
//! reading from the file. The daemon's shutdown frees the lease.
//!
//! Starts a daemon: run it on the remote test host only. Loopback usage
//! mock, scratch HOME and credential store, tokens that expire in 2100 (no
//! refresh), no session (no model request): no real login, refresh or usage
//! request can happen.

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_core::auth::usage_reads::{process_table, UsageReads, SAVED_READINGS_FILE};
use agent_core::auth::{
    Account, AccountPolicy, AccountSelector, CredentialBroker, CredentialRef, LocalBroker,
    OAuthProviderId,
};
use agent_engine::daemon::registry::DaemonLock;
use agent_engine::daemon::{Daemon, DaemonOpts};
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::{EngineHost, HostOpts};
use axum::routing::get;
use axum::{Json, Router};
use phase2::HomeGuard;
use serde_json::json;
use serial_test::serial;

/// Year 2100: no token is ever refreshed.
const FAR_FUTURE: u64 = 4_102_444_800_000;
const LABELS: [&str; 3] = ["a", "b", "c"];

/// One Anthropic login per label, each with an account id (a reading of a
/// login without one is never saved).
fn install_accounts(base: &Path) {
    let mut root = serde_json::Map::new();
    for label in LABELS {
        root.insert(
            format!("anthropic@{label}"),
            json!({
                "type": "oauth",
                "access": format!("anthropic-{label}-access"),
                "refresh": format!("refresh-anth-{label}-SECRET"),
                "expires": FAR_FUTURE,
                "accountId": format!("acct-{label}"),
            }),
        );
    }
    std::fs::write(
        base.join("auth.json"),
        serde_json::Value::Object(root).to_string(),
    )
    .unwrap();
}

/// Loopback usage mock: room in every window for every account, and the
/// number of requests it has answered.
async fn spawn_usage() -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let app = Router::new().route(
        "/usage",
        get(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async move {
                let reset = "2099-01-01T00:00:00Z";
                Json(json!({
                    "five_hour": {"utilization": 10.0, "resets_at": reset},
                    "seven_day": {"utilization": 10.0, "resets_at": reset}
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/usage"), hits)
}

fn auto_broker(usage_url: &str) -> LocalBroker {
    LocalBroker::new(reqwest::Client::new())
        .with_account_policy(
            AccountPolicy::new().with(OAuthProviderId::Anthropic, AccountSelector::Auto),
        )
        .with_usage_endpoint_override(usage_url.to_string())
}

fn anthropic(label: &str) -> CredentialRef {
    CredentialRef::new(OAuthProviderId::Anthropic, Account::named(label).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_started_daemon_writes_the_readings_and_its_broker_selections_read_no_usage() {
    let guard = HomeGuard::new();
    let base = guard.base_dir();
    install_accounts(&base);
    let saved = base.join(SAVED_READINGS_FILE);
    let (usage_url, reads) = spawn_usage().await;

    // A previous daemon's readings: its table (a writer) read every account
    // in a first selection and saved them.
    let before = auto_broker(&usage_url).with_new_saving_usage_table();
    assert!(before
        .access_token_pinned(OAuthProviderId::Anthropic)
        .await
        .is_ok());
    assert_eq!(reads.load(Ordering::SeqCst), 3, "a first selection reads every account");
    let file = std::fs::read_to_string(&saved).expect("the readings were saved");
    for label in LABELS {
        assert!(file.contains(&format!("|anthropic@{label}|")), "{file}");
    }
    for secret in ["-access", "SECRET", "acct-"] {
        assert!(!file.contains(secret), "the saved readings hold {secret}");
    }

    // This process has started no daemon: it only loads the readings. The
    // previous daemon has stopped: its lease is free.
    assert!(!process_table().saves_readings(), "writing was on before a start");
    before.stop_saving_usage_readings();

    // A start that finds the daemon lock held fails, and never turns
    // writing on.
    let opts = DaemonOpts {
        runtime_dir: Some(base.join("run")),
        ..Default::default()
    };
    let paths = opts.paths();
    std::fs::create_dir_all(&paths.dir).unwrap();
    let held = DaemonLock::try_acquire(&paths)
        .unwrap()
        .expect("the daemon lock is free");
    let host: Arc<EngineHost> = EngineHost::boot_and_install(HostOpts {
        profile: None,
        no_extensions: true,
        mcp_discovery: agent_engine::McpDiscovery::Inline,
    })
    .await
    .expect("host boot");
    assert!(
        Daemon::start(host.clone(), opts.clone()).await.is_err(),
        "a start won the held lock"
    );
    assert!(!process_table().saves_readings(), "a refused start turned writing on");
    drop(held);

    let daemon = Daemon::start(host.clone(), opts.clone())
        .await
        .expect("daemon start");
    assert!(process_table().saves_readings(), "the started daemon does not write");

    // Broker selections in the started daemon process, all at once, each on
    // a broker of its own over the process-wide table: the saved readings
    // load first, and they are fresh, so no usage request goes out.
    let selections: Vec<_> = (0..4)
        .map(|_| {
            let broker = auto_broker(&usage_url);
            tokio::spawn(async move {
                broker
                    .access_token_pinned(OAuthProviderId::Anthropic)
                    .await
                    .is_ok()
            })
        })
        .collect();
    for selection in selections {
        assert!(selection.await.unwrap(), "a selection found no account");
    }
    assert_eq!(
        reads.load(Ordering::SeqCst),
        3,
        "the selections read usage despite fresh saved readings"
    );

    // The daemon writes: a reported limit drops the account's reading from
    // the file, and keeps the others.
    auto_broker(&usage_url)
        .report_cooldown(&anthropic("a"), None, "anthropic_quota_exhausted")
        .await
        .unwrap();
    let file = std::fs::read_to_string(&saved).unwrap();
    assert!(!file.contains("|anthropic@a|"), "a's reading is still saved: {file}");
    for label in ["b", "c"] {
        assert!(file.contains(&format!("|anthropic@{label}|")), "{file}");
    }
    assert_eq!(reads.load(Ordering::SeqCst), 3);

    SocketTransport::shutdown(&daemon.paths.sock, false)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), daemon.wait())
        .await
        .expect("daemon wait");
    // The shutdown freed the lease: this process writes nothing more, and
    // another table (the next daemon's) takes it.
    assert!(!process_table().saves_readings(), "the stopped daemon still writes");
    assert!(
        UsageReads::default().write_saved_readings(&saved),
        "the stopped daemon kept the lease"
    );
    drop(guard);
}
