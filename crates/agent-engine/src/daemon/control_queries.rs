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

/// Answer `Children`. With `members`, the `/proc` walk runs on a blocking
/// thread. Any query may also start a lazy sweep (at most once per minute).
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
        });
    }
    children::maybe_sweep();
    let mut listing = children::list(session_id.as_ref().map(SessionId::as_str), sid);
    if members && !listing.sessions.is_empty() {
        if let Ok(scan) = tokio::task::spawn_blocking(children::scan_proc).await {
            children::attach_members(&mut listing, &scan);
        }
    }
    DaemonFrame::ChildList(ChildListReply {
        id,
        supported: true,
        boot_id: children::boot_id().map(str::to_string),
        clk_tck: children::clk_tck(),
        sessions: listing.sessions,
        host: listing.host,
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
}
