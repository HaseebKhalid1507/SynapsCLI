//! Control queries answered without a session (and on an attached
//! connection): they read daemon-held state only and never wait on a
//! session actor or a turn.

use crate::runtime::openai::catalog::validation::{default_level_for_model, thinking_options_with_basis, OptionsBasis};
use crate::session::children;
use crate::session::wire::{ChildListReply, DaemonFrame, ThinkingOptionsWire};
use crate::session::SessionId;
use crate::EngineHost;

/// Feature name for the `children` query (and the `tool_child` events).
pub const FEATURE_CHILDREN: &str = "children";
/// Feature name for the `thinking_options` query.
pub const FEATURE_THINKING_OPTIONS: &str = "thinking_options";
/// Longest model id the `thinking_options` query answers.
pub const MAX_MODEL_ID_BYTES: usize = 256;

/// What this daemon lists in `Welcome.features`.
pub fn daemon_features() -> Vec<String> {
    let mut features = Vec::new();
    if children::supported() {
        features.push(FEATURE_CHILDREN.to_string());
    }
    features.push(FEATURE_THINKING_OPTIONS.to_string());
    features
}

/// Answer `Children`. Each listed running leader is checked now, on a
/// blocking thread: one found gone, reused or a zombie is ended in the
/// ledger before the reply says anything. With `members`, one shared
/// `/proc` walk serves concurrent queries. Any query may also start a lazy
/// sweep (at most once per minute).
pub async fn children_reply(
    id: Option<u64>,
    session_id: Option<SessionId>,
    sid: Option<u32>,
    members: bool,
) -> DaemonFrame {
    if !children::supported() {
        return DaemonFrame::ChildList(ChildListReply {
            id,
            supported: false,
            boot_id: None,
            clk_tck: children::clk_tck(),
            sessions: Vec::new(),
            host: Vec::new(),
            refused_records: 0,
            ledger_full: false,
        });
    }
    children::maybe_sweep();
    let scan = if members { Some(children::shared_scan().await) } else { None };
    let session = session_id.as_ref().map(|s| s.as_str().to_string());
    let checked = tokio::task::spawn_blocking(move || {
        children::list_checked(session.as_deref(), sid, scan.as_ref().map(|s| s.as_slice()))
    })
    .await;
    // The check can only fail if the runtime is shutting down; then the
    // reply holds nothing it could not vouch for.
    let listing = checked.unwrap_or_default();
    DaemonFrame::ChildList(ChildListReply {
        id,
        supported: true,
        boot_id: children::boot_id().map(str::to_string),
        clk_tck: children::clk_tck(),
        sessions: listing.sessions,
        host: listing.host,
        refused_records: listing.refused_records,
        ledger_full: listing.ledger_full,
    })
}

/// Non-empty, at most [`MAX_MODEL_ID_BYTES`], no whitespace or control
/// characters. Anything else gets `options: null`.
pub fn well_formed_model_id(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= MAX_MODEL_ID_BYTES
        && !model.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// The answer for one model id: the settings-UI list, its basis and the
/// model-switch default. Reads the process-local capability cache only.
pub fn thinking_options_for(model: &str) -> Option<ThinkingOptionsWire> {
    if !well_formed_model_id(model) {
        return None;
    }
    let (levels, basis) = thinking_options_with_basis(model);
    Some(ThinkingOptionsWire {
        levels,
        exact: basis != OptionsBasis::Fallback,
        basis,
        default_level: default_level_for_model(model).map(|l| l.as_str().to_string()),
    })
}

pub fn thinking_options_reply(id: Option<u64>, model: String) -> DaemonFrame {
    let options = thinking_options_for(&model);
    DaemonFrame::ThinkingOptionsResult { id, model, options }
}

/// The `thinking_options` answer. With `warm`, first the existing bounded
/// Muse warm-up (8 s, one attempt per 300 s per daemon) for a well-formed
/// `muse/` id with no live row, through `broker` (the host's own; no
/// credential crosses the wire). Without `warm`, no network call.
pub async fn thinking_options_answer(
    id: Option<u64>,
    model: String,
    warm: bool,
    broker: impl FnOnce() -> std::sync::Arc<dyn crate::auth::CredentialBroker>,
) -> DaemonFrame {
    if warm && well_formed_model_id(&model) && model.starts_with("muse/") {
        crate::runtime::openai::catalog::ensure_muse_catalog_for(&model, &broker()).await;
    }
    thinking_options_reply(id, model)
}

/// The host's credential broker (the one sessions use).
pub fn host_broker(host: &EngineHost) -> std::sync::Arc<dyn crate::auth::CredentialBroker> {
    let parts = host.parts();
    crate::auth::broker_from_source(&parts.credential_source, &parts.token_cache, parts.client.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::openai::catalog::muse_warmup_tests::{broker, CATALOG};
    use crate::runtime::openai::catalog::{capability_cache, muse, reset_muse_warmup_backoff};

    fn options(frame: DaemonFrame) -> (String, Option<ThinkingOptionsWire>) {
        match frame {
            DaemonFrame::ThinkingOptionsResult { model, options, .. } => (model, options),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn malformed_ids_get_no_options_and_any_other_id_an_answer() {
        let long = format!("x/{}", "a".repeat(MAX_MODEL_ID_BYTES - 2));
        assert_eq!(long.len(), MAX_MODEL_ID_BYTES);
        for bad in ["", "muse/a b", "a\tb", "a\u{7}b", "a\u{a0}b", &format!("{long}z")] {
            assert!(thinking_options_for(bad).is_none(), "{bad:?}");
        }
        let unknown = thinking_options_for(&long).expect("well formed");
        assert_eq!((unknown.basis, unknown.exact, unknown.default_level), (OptionsBasis::Fallback, false, None));
        let xai = thinking_options_for("xai-auth/grok-4.5").unwrap();
        assert_eq!((xai.basis, xai.exact, xai.default_level.as_deref()), (OptionsBasis::Static, true, Some("high")));
        assert_eq!(xai.levels, crate::runtime::openai::catalog::validation::thinking_options_for_model("xai-auth/grok-4.5"));
    }

    // The guard serializes the process-global Muse cache across tests; this
    // test's runtime is current-thread, so nothing else here waits on it.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn warm_is_opt_in_and_goes_through_the_given_broker_once() {
        let _guard = muse::test_cache_guard();
        capability_cache::replace_provider("muse", &[]);
        reset_muse_warmup_backoff();
        let model = "muse/muse-warmup-probe-zz7".to_string();
        let b = broker(Ok(CATALOG));
        let dyn_b = || -> std::sync::Arc<dyn crate::auth::CredentialBroker> { b.clone() };

        let (echo, cold) = options(thinking_options_answer(Some(1), model.clone(), false, dyn_b).await);
        assert_eq!(echo, model);
        assert_eq!(b.seen.lock().unwrap().len(), 0, "warm:false makes no call");
        assert_eq!((cold.unwrap().basis, b.seen.lock().unwrap().len()), (OptionsBasis::Fallback, 0));

        let (_, warm) = options(thinking_options_answer(Some(2), model.clone(), true, dyn_b).await);
        let warm = warm.unwrap();
        assert_eq!(b.seen.lock().unwrap().len(), 1);
        assert_eq!((warm.basis, warm.exact, warm.levels.clone()), (OptionsBasis::Live, true, vec!["adaptive".to_string(), "high".to_string()]));

        // A live row now exists: a second warm makes no call.
        let _ = thinking_options_answer(Some(3), model, true, dyn_b).await;
        assert_eq!(b.seen.lock().unwrap().len(), 1);
        // Not a muse id: warm is a no-op.
        let _ = thinking_options_answer(Some(4), "xai-auth/grok-4.5".into(), true, dyn_b).await;
        assert_eq!(b.seen.lock().unwrap().len(), 1);
        capability_cache::replace_provider("muse", &[]);
        reset_muse_warmup_backoff();
    }

    /// An extension process is in the host list from its spawn, labelled
    /// with its id, and leaves the reply as soon as it is gone.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_host_list_names_a_live_extension_process_only() {
        use crate::extensions::runtime::ExtensionHandler;
        const ID: &str = "host-list-fixture";
        let host_of = |frame: DaemonFrame| match frame {
            DaemonFrame::ChildList(reply) => reply.host.into_iter().filter(|h| h.owner_id == ID).collect::<Vec<_>>(),
            other => panic!("{other:?}"),
        };
        let ext = crate::extensions::runtime::process::ProcessExtension::spawn(ID, "/bin/cat", &[]).await.expect("spawn /bin/cat");
        let listed = host_of(children_reply(Some(1), None, None, false).await);
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].owner_kind, children::HostOwnerKind::Extension);
        let pid = listed[0].child.pid;
        assert_eq!(children::identify(pid).as_ref(), Some(&listed[0].child), "the spawned process itself");
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
        assert!(cmdline.starts_with(b"/bin/cat"), "{cmdline:?}");

        ext.shutdown().await;
        let t0 = std::time::Instant::now();
        while !host_of(children_reply(Some(2), None, None, false).await).is_empty() {
            assert!(t0.elapsed() < std::time::Duration::from_secs(10), "still listed after shutdown");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// The reply reads every listed leader now, and carries the ledger's
    /// refusal count. The records name pids that never exist, in this
    /// process's own (live) sid, so a sweep may end them but never drops them.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_reply_checks_each_leader_now_and_counts_refusals() {
        let Some(boot) = children::boot_id() else { return };
        let sid = children::read_stat(std::process::id()).expect("own stat").sid;
        let session = format!("cq-fresh-{}", std::process::id());
        let before = {
            let mut ledger = children::ledger().lock().unwrap();
            let before = ledger.list(None, None).refused_records;
            for i in 0..children::MAX_PER_SESSION as u32 + 1 {
                let pid = u32::MAX - 1 - i; // above any pid_max
                let child = children::ChildIdentity { pid, start_ticks: 1, boot_id: boot.to_string(), sid, pgid: sid };
                let spawn = children::ChildSpawn {
                    tool_id: format!("t{i}"),
                    tool_name: "bash".into(),
                    handle_id: None,
                    shell_session_id: None,
                    child,
                };
                assert_eq!(ledger.insert(&session, spawn), i < children::MAX_PER_SESSION as u32);
            }
            before
        };
        let DaemonFrame::ChildList(reply) = children_reply(Some(3), Some(SessionId::from(session.as_str())), None, false).await
        else {
            panic!("not a child_list")
        };
        assert!(reply.refused_records > before, "{} > {before}", reply.refused_records);
        assert!(!reply.ledger_full);
        let group = &reply.sessions[0];
        assert!(group.truncated);
        assert_eq!(group.children.len(), children::MAX_PER_SESSION);
        assert!(group.children.iter().all(|c| c.state == children::ChildState::Ended), "no leader runs");
        let stored = children::list(Some(&session), None);
        assert!(stored.sessions[0].children.iter().all(|c| c.state == children::ChildState::Ended), "the ledger agrees");
    }
}
