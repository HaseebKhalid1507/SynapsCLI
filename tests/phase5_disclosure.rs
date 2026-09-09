//! Phase 5 / Task 31 — compaction disclosure policy and local-only mode
//! (spec §9.4).
//!
//! Contract:
//!
//! 1. Every frontend surfaces the disclosure preview (provider, model,
//!    approximate bytes, content classes) BEFORE summarization dispatch.
//! 2. The disclosure/config vocabulary is the shared typed ContentClass set
//!    (spec §9.3 provenance and §9.4 policy speak the same language).
//! 3. Local-only mode and per-class exclusions are configurable.
//!
//! The socket-spy (zero network in local-only mode) and per-class sentinel
//! proofs live in the engine unit tests next to `runtime::compaction`.

use synaps_cli::core::compaction::{CompactionMode, ContentClass};

#[test]
fn every_frontend_surfaces_disclosure_before_dispatch() {
    let root = env!("CARGO_MANIFEST_DIR");
    let read = |rel: &str| {
        std::fs::read_to_string(format!("{root}/{rel}"))
            .unwrap_or_else(|e| panic!("read {rel}: {e}"))
    };

    for rel in [
        "src/cmd/chat.rs",
        "src/cmd/rpc.rs",
        "src/cmd/server.rs",
        // The TUI compacts on the SessionActor since phase 3 (A2): the
        // disclosure is computed there and shipped as `CompactionStarted`.
        "crates/agent-engine/src/session/actor.rs",
    ] {
        let src = read(rel);
        assert!(
            src.contains("preview_compaction_disclosure"),
            "{rel} must surface the §9.4 disclosure preview before dispatch"
        );
        // Compare CALL sites (the `(`-suffixed forms skip import lists).
        let preview_pos = src.find("preview_compaction_disclosure(").unwrap();
        let dispatch_pos = src
            .find("compact_conversation(")
            .expect("frontend dispatches compaction");
        assert!(
            preview_pos < dispatch_pos,
            "{rel}: disclosure must be computed BEFORE the summarization \
             dispatch site"
        );
    }

    // The RPC frontend must additionally EMIT the disclosure to the client
    // as a dedicated pre-dispatch frame (command "compact.disclosure"),
    // before the summarization dispatch; the wire shape is pinned by
    // tests/rpc_protocol.rs. TUI and server surface it client-visibly as a
    // pre-dispatch System message/broadcast (asserted above by position).
    let rpc = read("src/cmd/rpc.rs");
    let frame_pos = rpc
        .find("compact.disclosure")
        .expect("rpc must emit the pre-dispatch disclosure frame");
    let rpc_dispatch_pos = rpc.find("compact_conversation(").unwrap();
    assert!(
        frame_pos < rpc_dispatch_pos,
        "rpc: the compact.disclosure frame must be sent before dispatch"
    );
}

#[test]
fn disclosure_config_round_trips_typed_mode_and_classes() {
    let config = synaps_cli::config::load_config_from_str(
        "compaction_mode = local\ncompaction_exclude = thinking, tool_results, file_paths\n",
    );
    assert_eq!(config.compaction_mode, CompactionMode::LocalOnly);
    assert_eq!(
        config.compaction_exclude,
        vec![
            ContentClass::Thinking,
            ContentClass::ToolResults,
            ContentClass::FilePaths,
        ]
    );

    // The typed classes serialize with the same stable names the provenance
    // record uses — one vocabulary across §9.3 and §9.4.
    for class in ContentClass::ALL {
        assert_eq!(
            serde_json::to_value(class).unwrap(),
            serde_json::Value::String(class.as_str().to_string()),
            "config name and serde name must agree for {class:?}"
        );
        assert_eq!(ContentClass::parse(class.as_str()), Some(class));
    }
}

#[test]
fn compaction_mode_defaults_to_remote_and_warns_on_unknown() {
    let defaults = synaps_cli::config::load_config_from_str("");
    assert_eq!(defaults.compaction_mode, CompactionMode::Remote);
    assert!(defaults.compaction_exclude.is_empty());

    let bad = synaps_cli::config::load_config_from_str("compaction_exclude = passwords\n");
    assert!(bad.compaction_exclude.is_empty());
    assert!(
        bad.warnings.iter().any(|w| w.contains("passwords")),
        "unknown class names must warn loudly: {:?}",
        bad.warnings
    );
}
