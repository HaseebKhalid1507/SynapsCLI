//! P12.4: non-loop helpers + P12.3 select! arm handlers — pure code-motion
//! out of `mod.rs` so the run() file stays setup-call + select! routing
//! table + teardown. Bodies are verbatim; only visibility annotations and
//! one `self::` → `super::` path changed for the new module location.
//! The animation-tick GUARD expression stays inline at the select! call
//! site in mod.rs (S206-regression knowledge) — only bodies live here.

use super::*;

fn handle_widget_event(
    app: &mut App,
    event: synaps_cli::extensions::widgets::ExtensionWidgetEvent,
) -> bool {
    use synaps_cli::extensions::widgets::WidgetEvent;
    let ext_id = event.extension_id.clone();
    match event.event {
        WidgetEvent::Upsert {
            id,
            lines,
            styled_lines,
            position,
            title,
            ttl_secs,
        } => {
            let pos = match position.as_str() {
                "top_left" => toast::ToastPosition::TOP_LEFT,
                "top_center" => toast::ToastPosition::TOP_CENTER,
                "top_right" => toast::ToastPosition::TOP_RIGHT,
                "middle_left" => toast::ToastPosition::MIDDLE_LEFT,
                "center" => toast::ToastPosition::CENTER,
                "middle_right" => toast::ToastPosition::MIDDLE_RIGHT,
                "bottom_left" => toast::ToastPosition::BOTTOM_LEFT,
                "bottom_center" => toast::ToastPosition::BOTTOM_CENTER,
                "bottom_right" => toast::ToastPosition::BOTTOM_RIGHT,
                _ => toast::ToastPosition::TOP_RIGHT,
            };
            let ttl = ttl_secs.map(std::time::Duration::from_secs);
            let mut t = toast::Toast::new(
                format!("widget:{}", id),
                lines.first().cloned().unwrap_or_default(),
            )
            .lines(lines)
            .at(pos)
            .ttl(ttl);
            // Convert styled_lines → rich ratatui Lines if present.
            if let Some(styled) = styled_lines {
                use ratatui::style::Style;
                use ratatui::text::{Line, Span};
                let rich: Vec<Line<'static>> = styled
                    .into_iter()
                    .map(|spans| {
                        Line::from(
                            spans
                                .into_iter()
                                .map(|s| {
                                    let mut style = Style::default();
                                    if let Some(ref fg) = s.fg {
                                        if let Some(c) = parse_hex_color(fg) {
                                            style = style.fg(c);
                                        }
                                    }
                                    if let Some(ref bg) = s.bg {
                                        if let Some(c) = parse_hex_color(bg) {
                                            style = style.bg(c);
                                        }
                                    }
                                    Span::styled(s.text, style)
                                })
                                .collect::<Vec<_>>(),
                        )
                    })
                    .collect();
                t = t.rich(rich);
            }
            if let Some(title) = title {
                t = t.titled(title);
            }
            // P19.2: extension-rendered surface resolves through the
            // namespaced token — border accent from `ext.<id>.accent`
            // (user TOML override > manifest declaration > default border).
            t = t.accent(theme::THEME.load().ext_token(&ext_id, "accent"));
            app.toasts.upsert(t)
        }
        WidgetEvent::Dismiss { id } => app.toasts.dismiss(&format!("widget:{}", id)),
    }
}

/// Parse a CSS-style hex color string (e.g. "#ff0000") into a ratatui Color.
fn parse_hex_color(s: &str) -> Option<ratatui::style::Color> {
    let s = s.strip_prefix('#')?;
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(ratatui::style::Color::Rgb(r, g, b))
}

fn handle_extension_loader_toast(app: &mut App, title: &str, lines: Vec<String>, persistent: bool) {
    app.toasts.upsert(
        toast::Toast::new("extension-loader", "")
            .titled(title)
            .lines(lines)
            .at(toast::ToastPosition::TOP_CENTER)
            .ttl(if persistent {
                None
            } else {
                Some(std::time::Duration::from_secs(5))
            }),
    );
    app.invalidate();
}

async fn handle_extension_loader_event(
    app: &mut App,
    runtime: &Runtime,
    event: synaps_cli::extensions::loader::ExtensionLoaderEvent,
    ext_mgr: &std::sync::Arc<
        tokio::sync::RwLock<synaps_cli::extensions::manager::ExtensionManager>,
    >,
) {
    use synaps_cli::extensions::loader::ExtensionLoaderEvent;
    match event {
        ExtensionLoaderEvent::Started => {
            handle_extension_loader_toast(
                app,
                "Extensions",
                vec!["Discovering extensions…".into()],
                true,
            );
        }
        ExtensionLoaderEvent::Loaded {
            plugin,
            loaded,
            failed,
        } => {
            handle_extension_loader_toast(
                app,
                "Extensions",
                vec![
                    format!(
                        "Loaded {loaded} extension{}",
                        if loaded == 1 { "" } else { "s" }
                    ),
                    format!("Latest: {plugin}"),
                    format!("Failures: {failed}"),
                ],
                true,
            );
        }
        ExtensionLoaderEvent::Failed {
            failure,
            loaded,
            failed,
        } => {
            handle_extension_loader_toast(
                app,
                "Extensions",
                vec![
                    format!("Loaded {loaded}, failed {failed}"),
                    format!("⚠ {}", failure.plugin),
                ],
                true,
            );
            app.push_msg(ChatMessage::System(format!(
                "⚠ Extension '{}' failed: {}",
                failure.plugin,
                failure.concise_message()
            )));
        }
        ExtensionLoaderEvent::Finished { loaded, failed } => {
            app.extension_loader_running = false;
            let handler_count = runtime.hook_bus().handler_count().await;
            tracing::info!(
                extensions = loaded.len(),
                failures = failed.len(),
                handlers = handler_count,
                "Extension discovery complete"
            );
            let lines = if failed.is_empty() {
                vec![format!(
                    "✓ Loaded {} extension{}",
                    loaded.len(),
                    if loaded.len() == 1 { "" } else { "s" }
                )]
            } else {
                vec![
                    format!(
                        "Loaded {} extension{}",
                        loaded.len(),
                        if loaded.len() == 1 { "" } else { "s" }
                    ),
                    format!("{} failed — see transcript", failed.len()),
                ]
            };
            handle_extension_loader_toast(app, "Extensions", lines, false);

            // P19.2: merge manifest-declared theme tokens into the theme
            // registry under `ext.<plugin-id>.<token>`. User theme-TOML
            // `ext.<id>.<token>` overrides still win — they live inside the
            // Theme value and are checked first by `Theme::ext_token`.
            // Extensions with no `theme_tokens` contribute nothing here.
            let ext_theme_tokens = ext_mgr.read().await.theme_tokens();
            if !ext_theme_tokens.is_empty() {
                for (ext_id, tokens) in &ext_theme_tokens {
                    theme::register_ext_theme_tokens(
                        ext_id,
                        tokens.iter().map(|(k, v)| (k.as_str(), v.as_str())),
                    );
                }
                app.invalidate();
            }

            // Spawn a background notification watcher for each loaded extension.
            // The watcher forwards widget.* notifications to the TUI via widget_tx.
            //
            // CP-11 fix-3 sibling audit: widget_tx is BOUNDED and this
            // watcher must NEVER await capacity — an awaited send would
            // backpressure the extension's lossless notification fan-out
            // (stalling command.invoke / provider.stream subscribers on
            // the same queue). On overflow the event is DROPPED with a
            // warn instead: widget upserts are idempotent last-writer-wins
            // UI state, so the first event after the TUI loop resumes
            // consuming restores the display.
            let handlers = ext_mgr.read().await.handlers();
            for (ext_id, handler) in handlers {
                let widget_tx = app.widget_tx.clone();
                tokio::spawn(async move {
                    loop {
                        let (_sub_id, mut rx) = handler.subscribe_notifications().await;
                        while let Some(frame) = rx.recv().await {
                            if synaps_cli::extensions::widgets::is_widget_method(&frame.method) {
                                if let Ok(event) =
                                    synaps_cli::extensions::widgets::parse_widget_event(
                                        &frame.method,
                                        &frame.params,
                                    )
                                {
                                    if let Err(tokio::sync::mpsc::error::TrySendError::Full(_)) =
                                        widget_tx.try_send(
                                            synaps_cli::extensions::widgets::ExtensionWidgetEvent {
                                                extension_id: ext_id.clone(),
                                                event,
                                            },
                                        )
                                    {
                                        tracing::warn!(
                                            extension = %ext_id,
                                            "widget event queue full; dropping widget event",
                                        );
                                    }
                                }
                            }
                        }
                        // rx closed (EOF/restart) — resubscribe after a brief delay
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                });
            }
        }
    }
}

/// Phase 8 slice 8A.8: when a plugin has staked a lifecycle claim and
/// declared a `settings_category`, copy the legacy global
/// `sidecar_toggle_key` value into the plugin-namespaced equivalent
/// (`plugins.{plugin}.{cat}._lifecycle_toggle_key`) so the user's
/// toggle-key choice follows them across the rename. Idempotent: any
/// claim whose new key is already set is skipped, and a missing legacy
/// value is a no-op.
pub(crate) fn migrate_sidecar_toggle_key_to_claimed_plugins(
    claims: &[synaps_cli::skills::registry::LifecycleClaim],
) {
    const LEGACY: &str = "sidecar_toggle_key";
    let Some(legacy_value) = synaps_cli::config::read_config_value(LEGACY) else {
        return;
    };
    let trimmed = legacy_value.trim();
    if trimmed.is_empty() {
        return;
    }
    for claim in claims {
        let Some(ref cat) = claim.settings_category else {
            continue;
        };
        let new_key = format!("plugins.{}.{}._lifecycle_toggle_key", claim.plugin, cat);
        if synaps_cli::config::read_config_value(&new_key).is_some() {
            continue;
        }
        match synaps_cli::config::write_config_value(&new_key, trimmed) {
            Ok(()) => tracing::info!(
                "sidecar migration: copied global `{}` → `{}` for plugin `{}`",
                LEGACY,
                new_key,
                claim.plugin,
            ),
            Err(err) => tracing::warn!(
                "sidecar migration: failed to copy `{}` → `{}`: {}",
                LEGACY,
                new_key,
                err,
            ),
        }
    }
}

/// Look up the display name for a sidecar's owning plugin from the
/// lifecycle-claim snapshot. Returns `None` if no claim matches.
///
/// Phase 8 8A.5 follow-up: used post-spawn to populate
/// [`SidecarUiState::display_name`] from the registry claim.
pub(crate) fn pick_display_name_for_plugin(
    plugin_name: &str,
    claims: &[synaps_cli::skills::registry::LifecycleClaim],
) -> Option<String> {
    claims
        .iter()
        .find(|c| c.plugin == plugin_name)
        .map(|c| c.display_name.clone())
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    use synaps_cli::skills::registry::LifecycleClaim;

    // RAII guard: sets SYNAPS_BASE_DIR for the duration of the test, then
    // restores the previous value (or removes the var) on drop.  This is the
    // canonical override – base_dir() checks SYNAPS_BASE_DIR *before* HOME, so
    // setting it here completely shadows the real ~/.synaps-cli regardless of
    // what HOME is, and is immune to the HOME-vs-SYNAPS_BASE_DIR race that was
    // the root cause of T137 flakiness.
    struct BaseDir {
        _dir: tempfile::TempDir,
        old: Option<String>,
    }

    impl BaseDir {
        /// Create a fresh TempDir, point SYNAPS_BASE_DIR at it, write the
        /// given initial config content into `<tmpdir>/config`, and return the
        /// guard.  The directory is removed automatically when the guard drops.
        fn new(initial_config: &str) -> Self {
            let dir = tempfile::TempDir::new().expect("tempdir");
            let old = std::env::var("SYNAPS_BASE_DIR").ok();
            synaps_cli::config::set_base_dir_for_tests(dir.path().to_path_buf());
            std::fs::write(dir.path().join("config"), initial_config).expect("write test config");
            Self { _dir: dir, old }
        }

        /// Path to the config file that base_dir() resolves to.
        fn config_path(&self) -> std::path::PathBuf {
            self._dir.path().join("config")
        }
    }

    impl Drop for BaseDir {
        fn drop(&mut self) {
            match &self.old {
                Some(v) => std::env::set_var("SYNAPS_BASE_DIR", v),
                None => std::env::remove_var("SYNAPS_BASE_DIR"),
            }
        }
    }

    fn claim(plugin: &str, command: &str, cat: Option<&str>) -> LifecycleClaim {
        LifecycleClaim {
            plugin: plugin.to_string(),
            command: command.to_string(),
            settings_category: cat.map(str::to_string),
            display_name: command.to_string(),
            importance: 0,
        }
    }

    #[test]
    fn migrate_copies_legacy_into_namespaced_key() {
        let _lock = crate::tui::CONFIG_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _base = BaseDir::new("sidecar_toggle_key = F2\n");

        migrate_sidecar_toggle_key_to_claimed_plugins(&[claim(
            "sample-sidecar",
            "capture",
            Some("capture"),
        )]);
        let v = synaps_cli::config::read_config_value(
            "plugins.sample-sidecar.capture._lifecycle_toggle_key",
        );
        assert_eq!(v.as_deref(), Some("F2"));
    }

    #[test]
    fn migrate_skips_when_new_key_already_set() {
        let _lock = crate::tui::CONFIG_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _base = BaseDir::new(
            "sidecar_toggle_key = F2\nplugins.sample-sidecar.capture._lifecycle_toggle_key = F12\n",
        );

        migrate_sidecar_toggle_key_to_claimed_plugins(&[claim(
            "sample-sidecar",
            "capture",
            Some("capture"),
        )]);
        let v = synaps_cli::config::read_config_value(
            "plugins.sample-sidecar.capture._lifecycle_toggle_key",
        );
        assert_eq!(
            v.as_deref(),
            Some("F12"),
            "must not overwrite a user-set value"
        );
    }

    #[test]
    fn migrate_is_noop_when_legacy_unset() {
        let _lock = crate::tui::CONFIG_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _base = BaseDir::new("model = claude-sonnet-4-6\n");

        migrate_sidecar_toggle_key_to_claimed_plugins(&[claim(
            "sample-sidecar",
            "capture",
            Some("capture"),
        )]);
        assert!(synaps_cli::config::read_config_value(
            "plugins.sample-sidecar.capture._lifecycle_toggle_key"
        )
        .is_none());
    }

    #[test]
    fn migrate_skips_claim_without_settings_category() {
        let _lock = crate::tui::CONFIG_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let base = BaseDir::new("sidecar_toggle_key = F8\n");

        migrate_sidecar_toggle_key_to_claimed_plugins(&[claim("p", "ocr", None)]);
        // No namespaced key written for a claim with no category.
        let contents = std::fs::read_to_string(base.config_path()).unwrap();
        assert!(
            !contents.contains("_lifecycle_toggle_key"),
            "no namespaced key should be written when settings_category is None: {contents}"
        );
    }

    #[test]
    fn migrate_handles_multiple_claims_in_one_pass() {
        let _lock = crate::tui::CONFIG_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _base = BaseDir::new("sidecar_toggle_key = C-V\n");

        migrate_sidecar_toggle_key_to_claimed_plugins(&[
            claim("sample-sidecar", "capture", Some("capture")),
            claim("ocr-plugin", "ocr", Some("ocr")),
        ]);
        assert_eq!(
            synaps_cli::config::read_config_value(
                "plugins.sample-sidecar.capture._lifecycle_toggle_key"
            )
            .as_deref(),
            Some("C-V")
        );
        assert_eq!(
            synaps_cli::config::read_config_value("plugins.ocr-plugin.ocr._lifecycle_toggle_key")
                .as_deref(),
            Some("C-V")
        );
    }
}

#[cfg(test)]
mod display_name_helper_tests {
    use super::pick_display_name_for_plugin;
    use synaps_cli::skills::registry::LifecycleClaim;

    fn claim(plugin: &str, display: &str) -> LifecycleClaim {
        LifecycleClaim {
            plugin: plugin.into(),
            command: "capture".into(),
            settings_category: None,
            display_name: display.into(),
            importance: 0,
        }
    }

    #[test]
    fn pick_display_name_for_plugin_returns_match() {
        let claims = vec![claim("sample-sidecar", "Sample")];
        assert_eq!(
            pick_display_name_for_plugin("sample-sidecar", &claims),
            Some("Sample".to_string())
        );
    }

    #[test]
    fn pick_display_name_for_plugin_returns_none_for_unmatched() {
        let claims = vec![claim("sample-sidecar", "Sample")];
        assert_eq!(pick_display_name_for_plugin("unknown", &claims), None);
    }

    #[test]
    fn pick_display_name_for_plugin_returns_none_with_empty_claims() {
        assert_eq!(pick_display_name_for_plugin("sample-sidecar", &[]), None);
    }
}

// ── P12.3: select! arm handlers — pure code-motion from the run() loop.
// Each fn is the verbatim arm body; the select! arms are now a routing
// table. The animation-tick GUARD expression stays inline at the call
// site (S206-regression knowledge) — only the body moved here.

/// Ping-result arm: a model ping completed.
pub(crate) fn handle_ping_arm(
    app: &mut App,
    result: Option<(String, synaps_cli::runtime::openai::ping::PingStatus, u64)>,
) {
    match result {
        Some((key, status, ms)) => {
            if app.ping_print {
                let detail = match status {
                    synaps_cli::runtime::openai::ping::PingStatus::Online => format!("{}ms", ms),
                    synaps_cli::runtime::openai::ping::PingStatus::RateLimited => {
                        "429 rate limited".to_string()
                    }
                    synaps_cli::runtime::openai::ping::PingStatus::Unauthorized => {
                        "401 unauthorized".to_string()
                    }
                    synaps_cli::runtime::openai::ping::PingStatus::NotFound => {
                        "404 not found".to_string()
                    }
                    synaps_cli::runtime::openai::ping::PingStatus::Timeout => "timeout".to_string(),
                    synaps_cli::runtime::openai::ping::PingStatus::Error => "error".to_string(),
                };
                app.push_msg(ChatMessage::System(format!(
                    "  {} {:<50} — {}",
                    status.icon(),
                    key,
                    detail
                )));
                app.ping_pending = app.ping_pending.saturating_sub(1);
                if app.ping_pending == 0 {
                    app.ping_print = false;
                }
            }
            app.model_health.insert(key, (status, ms));
            app.request_redraw();
        }
        None => {
            // All ping tasks done (tx dropped) — stop printing
            app.ping_print = false;
        }
    }
}

/// Expanded model-list arm: async provider model enumeration returned.
pub(crate) fn handle_model_list_arm(
    app: &mut App,
    result: Option<(
        String,
        std::result::Result<Vec<models::ExpandedModelEntry>, String>,
    )>,
) {
    if let Some((provider_key, models_result)) = result {
        // App-level catalog cache: shared by the /models modal and
        // the /settings model picker (live/static reuse).
        if let Ok(models_list) = &models_result {
            let rows = models::catalog_override_rows(&provider_key, models_list);
            if !rows.is_empty() {
                app.catalog_overrides.insert(provider_key.clone(), rows);
            }
        }
        if let Some(state) = app.models.as_mut() {
            models::apply_model_list_result(state, &provider_key, models_result);
        }
        app.request_redraw();
    }
}

/// Async extension-loader progress arm.
pub(crate) async fn handle_extension_loader_arm(
    app: &mut App,
    runtime: &Runtime,
    event: Option<synaps_cli::extensions::loader::ExtensionLoaderEvent>,
    ext_mgr: &std::sync::Arc<
        tokio::sync::RwLock<synaps_cli::extensions::manager::ExtensionManager>,
    >,
) {
    if let Some(event) = event {
        handle_extension_loader_event(app, runtime, event, ext_mgr).await;
    } else {
        app.extension_loader_running = false;
        app.toasts.dismiss("extension-loader");
    }
    app.request_redraw();
}

/// Widget-event arm: background extension notification watcher pushed a widget.
pub(crate) fn handle_widget_arm(
    app: &mut App,
    widget_event: synaps_cli::extensions::widgets::ExtensionWidgetEvent,
) {
    // Only redraw when the widget's VISIBLE content actually changed.
    // Plugins (d20/jawz-widget/synaps-tasks) re-send unchanged widgets
    // on a poll loop; redrawing on every one pinned the render loop at
    // ~30% CPU at idle (#119). The dirty-check in upsert/dismiss makes an
    // idle session genuinely idle.
    if handle_widget_event(app, widget_event) {
        app.request_redraw();
    }
}

/// Animation/spinner tick arm body (~60fps when active). The GUARD that
/// gates this arm stays inline at the select! call site — it encodes the
/// S206 idle-logo regression fix and must not move. Returns `true` when the
/// render thread signalled exit-done and the loop should break.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_animation_tick(
    app: &mut App,
    runtime: &Runtime,
    config: &synaps_cli::SynapsConfig,
    registry: &std::sync::Arc<synaps_cli::skills::registry::CommandRegistry>,
    render_handle: &render_thread::RenderHandle,
    secret_prompt_rx: &std::sync::Arc<
        std::sync::Mutex<
            tokio::sync::mpsc::UnboundedReceiver<synaps_cli::tools::SecretPromptRequest>,
        >,
    >,
    boot_done: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    exit_done: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    boot_fx_sent: &mut bool,
    exit_fx_sent: bool,
    last_subagent_reconcile: &mut Option<std::time::Instant>,
) -> bool {
    // Active animations/effects always need a redraw each tick.
    // messages.is_empty() = idle logo screen — its color gradient
    // is time-based and needs ticking too (S206 regression: the
    // dirty-flag loop froze it until first keystroke).
    // Update local effect-sent flags from the render thread's done signals.
    if *boot_fx_sent && boot_done.load(Ordering::Acquire) {
        *boot_fx_sent = false;
    }
    if exit_fx_sent
        || *boot_fx_sent
        || app.streaming
        || app.logo_build_t.is_some()
        || app.logo_dismiss_t.is_some()
        || app.gamba_child.is_some()
        || app.transcript.is_empty()
        || !app.subagents.is_empty()
    {
        app.request_redraw();
    }
    app.secret_prompts.poll_requests(secret_prompt_rx);
    // Animated theme cross-fade: advance the active transition one frame
    // through the SAME set_theme path every other apply uses. On landing,
    // transition::advance clears app.theme_transition and hands back the
    // byte-exact target — so the tick GUARD's `theme_transition.is_some()`
    // term goes false and this arm stops firing (no permanent-60fps leak).
    if let Some(frame) =
        theme::transition::advance(&mut app.theme_transition, std::time::Instant::now())
    {
        theme::set_theme(frame);
        app.invalidate();
    }
    // P7.8: activation/deactivation happen OUTSIDE any input event
    // (async queue + auto-chaining); reconcile the stack to the
    // queue's is_active() so SecretPrompt is pushed/popped (§5).
    input::reconcile_secret_prompt(app);
    // Throttled subagent reconcile (~1s cadence) — keeps the mutex off
    // the 60fps path while still updating timers and catching idle-finish.
    if !app.subagents.is_empty() {
        let now = std::time::Instant::now();
        let should_reconcile =
            last_subagent_reconcile.map_or(true, |t| now.duration_since(t).as_secs_f64() >= 1.0);
        if should_reconcile {
            let rows = runtime
                .subagent_registry()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .display_rows();
            super::stream_handler::reconcile_subagents(&mut app.subagents, &rows, now);
            *last_subagent_reconcile = Some(now);
            app.request_redraw();
        }
    }
    if app.toasts.tick() {
        app.invalidate();
    }
    // Tick the in-flight plugin install spinner and reap the
    // background clone task once it finishes.
    let mut install_did_work = false;
    let mut install_finished = false;
    if let Some(plugins_state) = app.plugins.as_mut() {
        if plugins_state.is_install_active() {
            plugins_state.tick_install_spinner();
            install_did_work = true;
            if plugins_state.install_ready_to_reap() {
                install_finished = true;
            }
        }
    }
    if install_finished {
        if let Some(plugins_state) = app.plugins.as_mut() {
            super::plugins::actions::complete_pending_install_clone(
                plugins_state,
                registry,
                config,
            )
            .await;
        }
    }
    if install_did_work || install_finished {
        app.invalidate();
    }
    let message_animation_needs_clear = app.needs_clear_for_animation_redraw();
    if message_animation_needs_clear
        && crossterm::terminal::size().is_ok_and(|(w, h)| w > 0 && h > 0)
    {
        render_handle.send_clear();
    }
    if let Some(ref mut t) = app.logo_build_t {
        *t += 0.025;
        if *t >= 1.0 {
            app.logo_build_t = None;
        }
        app.request_redraw();
    }
    if let Some(ref mut t) = app.logo_dismiss_t {
        *t += 0.04;
        if *t >= 1.0 {
            app.logo_dismiss_t = None;
        }
        app.request_redraw();
    }
    if app.advance_animations() {
        // Spinner ticks only affect the tail message (THINKING_PLACEHOLDER,
        // active tool animation). Mark just the last slot dirty instead of
        // full invalidation — O(1) instead of O(n) per frame.
        app.invalidate_last();
    }
    if let Some(msg) = app.check_gamba_exited() {
        // check_gamba_exited() already called restore_terminal();
        // resume the render thread now that we own the terminal again.
        render_handle.resume();
        app.push_msg(ChatMessage::System(msg));
        app.invalidate(); // invalidate already sets needs_redraw
    }
    // Poll background compaction task
    if app.compact_task.as_ref().is_some_and(|t| t.is_finished()) {
        let handle = app.compact_task.take().unwrap();
        let msg_count = app.api_messages.len();
        match handle.await {
            Ok(Ok(outcome)) => {
                // T30 (spec §9.2): apply through the ONE engine
                // transition — successor policy, chain advancement,
                // provenance, pending events/queued message, hooks,
                // and save ordering all live in the engine now.
                let pending: Vec<String> = app.pending_events.clone();
                let queued = app.queued_message.clone();
                match synaps_cli::runtime::compaction::apply_compaction(
                    runtime,
                    &app.session,
                    &app.api_messages,
                    &outcome,
                    synaps_cli::runtime::compaction::CompactionTransition {
                        policy: synaps_cli::runtime::compaction::CompactionPolicy::LinkedSuccessor,
                        pending_events: pending,
                        queued_message: queued.clone(),
                        hook_source: "manual".to_string(),
                    },
                )
                .await
                {
                    Ok(applied) => {
                        let old_id = applied.previous_session_id.clone();
                        app.pending_events.clear();
                        app.queued_message = None;
                        app.session = applied.session;
                        app.api_messages = applied.api_messages;
                        app.total_input_tokens = 0;
                        app.total_output_tokens = 0;
                        app.session_cost = 0.0;
                        let msgs = app.api_messages.clone();
                        rebuild_display_messages(&msgs, app);
                        for name in &applied.chains_advanced {
                            app.push_msg(ChatMessage::System(format!(
                                "chain '{}' advanced: {} → {}",
                                name, old_id, app.session.id
                            )));
                        }
                        if let Some(q) = queued {
                            app.push_msg(ChatMessage::System(format!(
                                "queued message restored: {}",
                                q
                            )));
                        }
                        app.push_msg(ChatMessage::System(format!(
                            "✓ compacted {} messages → new session {} (from {})",
                            msg_count, app.session.id, old_id
                        )));
                    }
                    Err(e) => {
                        // Prior session state (including pending
                        // events and the queued message) is intact.
                        app.push_msg(ChatMessage::Error(format!("compaction failed: {}", e)));
                    }
                }
            }
            Ok(Err(e)) => {
                app.push_msg(ChatMessage::Error(format!("compaction failed: {}", e)));
            }
            Err(e) => {
                app.push_msg(ChatMessage::Error(format!(
                    "compaction task panicked: {}",
                    e
                )));
            }
        }
        app.status_text = None;
        app.invalidate();
    }
    if exit_done.load(Ordering::Acquire) {
        return true;
    }
    false
}

/// Live-MXC (myx theme) boot: start the subscriber iff the configured theme
/// is "myx". Later /theme switches reconcile via `App::sync_myx_live`.
pub(crate) fn boot_myx_live(app: &mut App) {
    if theme::configured_theme_name().as_deref() == Some("myx") {
        app.sync_myx_live("myx");
    }
}

/// Live-MXC arm body: apply a palette on the UI thread through the exact
/// `set_theme` + `invalidate` path `/theme` uses — animated, honoring the
/// wire's advisory `fade_ms` (clamped to 0..=2000ms; 0 = intentional snap;
/// absent = 350ms fallback). The subscriber task only ever sends; it never
/// touches theme state. Rapid track changes retarget the in-flight fade
/// from its current frame — never queued, never a jump.
pub(crate) fn handle_myx_theme_arm(app: &mut App, msg: (theme::Theme, Option<u64>)) {
    let (palette, fade_ms) = msg;
    app.apply_theme_animated(palette, theme::transition::wire_fade_duration(fade_ms));
}

/// Live-MXC teardown: after this the app is tearing down and no background
/// task may write into it. `abort()` is safe mid-await — the task holds no
/// locks and owns no external state; queued palettes die with the receiver.
pub(crate) fn abort_myx_live(app: &mut App) {
    if let Some(h) = app.myx_task.take() {
        h.abort();
    }
}
