//! The parent settings a worker inherits, resolved for the worker's model
//! and window (`docs/specs/subagent-context-rollover.md` §2, §7.2;
//! slice 7, dark). Pure: no runtime, no disk, no network.
use super::*;
use agent_core::compaction::{CompactionMode, ContentClass};
use agent_core::config::{ContextManagementConfig, ContextManagementMode};
use agent_core::core::context_policy::ContextThresholds;

const SONNET: &str = "claude-sonnet-4-6";

/// A parent with context management `auto` and `config`'s thresholds.
fn parent(config: ContextManagementConfig) -> WorkerContextPolicy {
    WorkerContextPolicy {
        context_management: ContextManagementConfig {
            mode: ContextManagementMode::Auto,
            ..config
        },
        ..Default::default()
    }
}

fn thresholds(pressure: u64, rollover: u64, target: u64, summary: u64) -> ContextManagementConfig {
    ContextManagementConfig {
        pressure_tokens: Some(pressure),
        rollover_tokens: Some(rollover),
        target_tokens: Some(target),
        summary_tokens: Some(summary),
        ..Default::default()
    }
}

fn window(tokens: u64, model: &str) -> Option<InheritedWindow> {
    Some(InheritedWindow {
        tokens,
        model: model.into(),
    })
}

/// m9: the parent's window override reaches a worker on the same model
/// (however its id is spelled), never one on another model or endpoint.
#[test]
fn m9_a_window_override_is_inherited_only_for_the_same_model_identity() {
    let policy = WorkerContextPolicy {
        window_override: window(150_000, SONNET),
        ..parent(Default::default())
    };
    let inherited = |model: &str| policy.resolve(model).expect("resolves").context_window;
    assert_eq!(inherited(SONNET), Some(150_000));
    assert_eq!(inherited("anthropic/claude-sonnet-4-6"), Some(150_000), "same model, other spelling");
    assert_eq!(inherited("claude-opus-4-6"), None, "another model");
    assert_eq!(inherited("local/test-model"), None, "another provider");
    assert_eq!(policy.resolve("plugin:provider:claude-sonnet-4-6").unwrap().context_window, None);
    let none = parent(Default::default());
    assert_eq!(none.resolve(SONNET).unwrap().context_window, None);
}

#[test]
fn m9_model_identity_is_provider_endpoint_and_model() {
    assert!(same_model_identity(SONNET, SONNET));
    assert!(same_model_identity(SONNET, "anthropic/claude-sonnet-4-6"));
    assert!(same_model_identity(" claude-sonnet-4-6", "claude-sonnet-4-6 "));
    assert!(!same_model_identity(SONNET, "claude-opus-4-6"));
    assert!(!same_model_identity("local/test-model", "local/other-model"));
    assert!(!same_model_identity("groq/llama-3.3-70b-versatile", "local/llama-3.3-70b-versatile"));
    assert!(same_model_identity("plugin:provider:model", "plugin:provider:model"));
    assert!(!same_model_identity("plugin:provider:model", "plugin:other:model"));
}

/// m10: thresholds that fit the worker's window are the parent's, with no
/// notice.
#[test]
fn m10_thresholds_that_fit_the_workers_window_are_kept() {
    let config = ContextManagementConfig {
        reserve_tokens: 20_000,
        finish_rounds: 3,
        summary_timeout_secs: 60,
        ..thresholds(60_000, 80_000, 25_000, 4_000)
    };
    let resolved = parent(config).resolve(SONNET).expect("fits 200k");
    assert_eq!(
        resolved.context_management,
        ContextManagementConfig {
            mode: ContextManagementMode::Auto,
            ..config
        }
    );
    assert_eq!(resolved.notice, None);
}

/// m10: thresholds set for a larger window (the parent's 1M override on
/// another model) are reset to the worker's defaults; the reserve, finish
/// rounds and summary timeout stay; the caller is told why.
#[test]
fn m10_thresholds_that_dont_fit_are_reset_to_the_workers_defaults_with_a_notice() {
    let config = ContextManagementConfig {
        reserve_tokens: 20_000,
        finish_rounds: 3,
        summary_timeout_secs: 60,
        ..thresholds(600_000, 800_000, 300_000, 20_000)
    };
    let policy = WorkerContextPolicy {
        window_override: window(1_000_000, "claude-opus-4-6"),
        ..parent(config)
    };
    let resolved = policy.resolve(SONNET).expect("the defaults fit");
    assert_eq!(resolved.context_window, None, "another model's override");
    assert_eq!(
        resolved.context_management,
        ContextManagementConfig {
            mode: ContextManagementMode::Auto,
            pressure_tokens: None,
            rollover_tokens: None,
            target_tokens: None,
            summary_tokens: None,
            reserve_tokens: 20_000,
            finish_rounds: 3,
            summary_timeout_secs: 60,
        }
    );
    assert_eq!(
        resolved.notice.as_deref(),
        Some(
            "rollover thresholds reset to this worker's defaults: \
             pressure_tokens must be below the context window (window 200000 tokens)"
        )
    );
}

/// m10: a window even the defaults don't fit turns rollover off, and says
/// why.
#[test]
fn m10_a_window_the_defaults_dont_fit_turns_rollover_off() {
    let policy = WorkerContextPolicy {
        window_override: window(1, SONNET),
        ..parent(thresholds(60_000, 80_000, 25_000, 4_000))
    };
    assert_eq!(
        policy.resolve(SONNET),
        Err("context window must contain at least two tokens (window 1 tokens)".to_owned())
    );
}

/// No worker context from a parent whose context management is not `auto`.
#[test]
fn a_parent_without_automatic_context_management_gives_no_worker_context() {
    let off = WorkerContextPolicy::default();
    assert_eq!(
        off.resolve(SONNET),
        Err("the parent's context management is not auto".to_owned())
    );
}

/// The summary and disclosure settings are the parent's, as configured: an
/// unset compaction model stays unset (the worker resolves the default as
/// the parent does), an explicit summary model stays explicit, a malformed
/// one stays malformed.
#[test]
fn m6_m8_the_summary_and_disclosure_settings_are_copied_as_configured() {
    let policy = WorkerContextPolicy {
        summary_model: Some("local/test-model".into()),
        compaction_mode: CompactionMode::LocalOnly,
        compaction_exclusions: vec![ContentClass::ToolResults],
        ..parent(Default::default())
    };
    let resolved = policy.resolve(SONNET).unwrap();
    assert_eq!(resolved.summary_model.as_deref(), Some("local/test-model"));
    assert!(!resolved.summary_model_invalid);
    assert_eq!(resolved.compaction_model, None, "unset stays unset");
    assert_eq!(resolved.compaction_mode, CompactionMode::LocalOnly);
    assert_eq!(resolved.compaction_exclusions, vec![ContentClass::ToolResults]);
    let invalid = WorkerContextPolicy {
        summary_model_invalid: true,
        compaction_model: Some(String::new()),
        ..parent(Default::default())
    };
    let resolved = invalid.resolve(SONNET).unwrap();
    assert!(resolved.summary_model_invalid);
    assert_eq!(resolved.compaction_model.as_deref(), Some(""), "an empty one too");
}

/// The old silent path is gone: no worker context leaves thresholds its
/// window can't resolve. Those made every assessment `InvalidConfig` (a
/// hard stop, so a rollover each round) and the rollover itself unbounded
/// with no summary (`runtime::stream`'s threshold resolution).
#[test]
fn no_worker_context_has_thresholds_its_window_cannot_resolve() {
    let configs = [
        Default::default(),
        thresholds(60_000, 80_000, 25_000, 4_000),
        thresholds(600_000, 800_000, 300_000, 20_000),
        thresholds(150_000, 190_000, 100_000, 0),
        ContextManagementConfig {
            target_tokens: Some(150_000),
            ..Default::default()
        },
        ContextManagementConfig {
            summary_tokens: Some(140_000),
            ..Default::default()
        },
        ContextManagementConfig {
            rollover_tokens: Some(250_000),
            ..Default::default()
        },
    ];
    let windows = [None, Some(2), Some(50_000), Some(150_000), Some(1_000_000)];
    for config in configs {
        for tokens in windows {
            let policy = WorkerContextPolicy {
                window_override: tokens.and_then(|t| window(t, SONNET)),
                ..parent(config)
            };
            match policy.resolve(SONNET) {
                Ok(resolved) => {
                    let window = resolved.context_window.unwrap_or(200_000);
                    assert!(
                        ContextThresholds::resolve(&resolved.context_management, window).is_ok(),
                        "{config:?} in {window}"
                    );
                    assert_eq!(resolved.context_management.mode, ContextManagementMode::Auto);
                }
                Err(why) => assert!(!why.is_empty()),
            }
        }
    }
}

/// The parent's `context_window = 1m` reaches a worker on the same
/// 200k-only model clamped to that model's ceiling: its thresholds are
/// resolved (here reset, with the notice) for the 200k it can send, never
/// the configured 1M. A 1M-capable worker model keeps the 1M window and the
/// thresholds that fit it.
#[test]
fn an_inherited_window_is_clamped_to_the_workers_model_ceiling() {
    const HAIKU: &str = "claude-haiku-4-5-20251001";
    let config = thresholds(300_000, 450_000, 100_000, 15_000);
    let on = |model: &str| {
        WorkerContextPolicy {
            window_override: window(1_000_000, model),
            ..parent(config)
        }
        .resolve(model)
        .expect("resolves")
    };
    let haiku = on(HAIKU);
    assert_eq!(
        haiku.context_window,
        Some(1_000_000),
        "the setting, as configured"
    );
    assert_eq!(
        crate::models::effective_context_window(HAIKU, haiku.context_window),
        200_000
    );
    assert_eq!(
        haiku.notice.as_deref(),
        Some(
            "rollover thresholds reset to this worker's defaults: \
             pressure_tokens must be below the context window (window 200000 tokens)"
        )
    );
    let at = ContextThresholds::resolve(&haiku.context_management, 200_000).expect("fits 200k");
    assert_eq!((at.pressure_tokens, at.rollover_tokens), (140_000, 180_000));
    let sonnet = on(SONNET);
    assert_eq!(sonnet.context_window, Some(1_000_000));
    assert_eq!(sonnet.notice, None, "fits 1M");
    assert_eq!(
        sonnet.context_management,
        ContextManagementConfig {
            mode: ContextManagementMode::Auto,
            ..config
        }
    );
}

// ── The trigger below the resume refusal (§12 item 9) ──────────────────────────

/// A parent with `config`'s thresholds, a 1M window on the worker's model
/// and the default resume refusal line (250k).
fn on_1m(config: ContextManagementConfig) -> ContextManagementConfig {
    WorkerContextPolicy {
        window_override: window(1_000_000, SONNET),
        resume_refuse_tokens: Some(250_000),
        ..parent(config)
    }
    .resolve(SONNET)
    .expect("resolves")
    .context_management
}

/// c1: a worker on a 1M window with the default thresholds would roll over
/// at 400k, past the 250k resume refusal. Its trigger is clamped to
/// 250k - (1 + 2) x 16k = 202,000 and its pressure to 7/9 of that
/// (157,111), so it keeps an advisory band; the target (100k) and summary
/// (15k) are the defaults; there is no notice (mutants: no clamp; the cap
/// at the refusal line; one round's headroom; pressure one below the cap).
#[test]
fn c1_a_1m_worker_rolls_over_below_the_resume_refusal() {
    let policy = WorkerContextPolicy {
        window_override: window(1_000_000, SONNET),
        resume_refuse_tokens: Some(250_000),
        ..parent(Default::default())
    };
    let resolved = policy.resolve(SONNET).expect("resolves");
    assert_eq!(resolved.context_window, Some(1_000_000));
    assert_eq!(resolved.notice, None, "a clamp gives no notice");
    let config = resolved.context_management;
    assert_eq!(
        (
            config.pressure_tokens,
            config.rollover_tokens,
            config.target_tokens,
            config.summary_tokens
        ),
        (Some(157_111), Some(202_000), None, None)
    );
    let at = ContextThresholds::resolve(&config, 1_000_000).expect("resolves for its window");
    assert_eq!((at.pressure_tokens, at.rollover_tokens), (157_111, 202_000));
    assert_eq!((at.target_tokens(&config), at.summary_tokens(&config)), (100_000, 15_000));
}

/// c2: an explicit trigger above the cap is clamped to it, and only it
/// moves while the pressure is below the cap; one under the cap is kept as
/// set. A pressure at or past the cap becomes 7/9 of it, and a target and
/// summary size that no longer fit go back to their defaults. The headroom
/// is the parent's reserve and finish rounds.
#[test]
fn c2_an_explicit_trigger_above_the_cap_is_clamped_and_one_below_it_kept() {
    let above = thresholds(150_000, 245_000, 90_000, 10_000);
    assert_eq!(
        on_1m(above),
        ContextManagementConfig {
            mode: ContextManagementMode::Auto,
            rollover_tokens: Some(202_000),
            ..above
        }
    );
    let below = thresholds(150_000, 190_000, 90_000, 10_000);
    assert_eq!(
        on_1m(below),
        ContextManagementConfig {
            mode: ContextManagementMode::Auto,
            ..below
        }
    );
    let past = on_1m(thresholds(210_000, 245_000, 160_000, 120_000));
    assert_eq!(
        (
            past.pressure_tokens,
            past.rollover_tokens,
            past.target_tokens,
            past.summary_tokens
        ),
        (Some(157_111), Some(202_000), None, None)
    );
    // 250k - (1 + 3) x 20k = 170k; 7/9 of it is 132,222.
    let wider = on_1m(ContextManagementConfig {
        reserve_tokens: 20_000,
        finish_rounds: 3,
        ..Default::default()
    });
    assert_eq!(
        (wider.pressure_tokens, wider.rollover_tokens),
        (Some(132_222), Some(170_000))
    );
}

/// c3: with the refusal line off nothing is clamped (mutant: a clamp
/// without a refusal line), nor is a trigger already below the cap (a
/// worker on the default 200k window: 140k/180k).
#[test]
fn c3_without_a_refusal_line_or_below_the_cap_nothing_is_clamped() {
    let off = WorkerContextPolicy {
        window_override: window(1_000_000, SONNET),
        resume_refuse_tokens: None,
        ..parent(Default::default())
    };
    let config = off.resolve(SONNET).unwrap().context_management;
    assert_eq!((config.pressure_tokens, config.rollover_tokens), (None, None));
    let at = ContextThresholds::resolve(&config, 1_000_000).unwrap();
    assert_eq!((at.pressure_tokens, at.rollover_tokens), (250_000, 400_000));
    let small = WorkerContextPolicy {
        resume_refuse_tokens: Some(250_000),
        ..parent(Default::default())
    };
    assert_eq!(
        small.resolve(SONNET).unwrap().context_management,
        ContextManagementConfig {
            mode: ContextManagementMode::Auto,
            ..Default::default()
        }
    );
}

/// c4: a refusal line that leaves no trigger below its headroom turns
/// rollover off, and says why.
#[test]
fn c4_a_refusal_line_with_no_room_below_it_turns_rollover_off() {
    let policy = WorkerContextPolicy {
        resume_refuse_tokens: Some(40_000),
        ..parent(Default::default())
    };
    assert_eq!(
        policy.resolve(SONNET),
        Err("the resume refusal line (40000 tokens) leaves no room for a rollover trigger \
             below its 48000 tokens of headroom (window 200000 tokens)"
            .to_owned())
    );
}

/// c5: whatever the parent's thresholds, window and refusal line, a
/// worker context resolves for its window, its trigger at least
/// (1 + finish rounds) x reserve below the refusal line.
#[test]
fn c5_a_clamped_context_resolves_for_its_window_below_the_refusal_line() {
    let configs = [
        Default::default(),
        thresholds(60_000, 80_000, 25_000, 4_000),
        thresholds(150_000, 190_000, 100_000, 0),
        thresholds(210_000, 245_000, 160_000, 120_000),
        thresholds(600_000, 800_000, 300_000, 20_000),
        ContextManagementConfig {
            reserve_tokens: 20_000,
            finish_rounds: 3,
            target_tokens: Some(150_000),
            ..Default::default()
        },
        ContextManagementConfig {
            rollover_tokens: Some(250_000),
            summary_tokens: Some(0),
            ..Default::default()
        },
    ];
    let windows = [None, Some(2), Some(50_000), Some(150_000), Some(400_000), Some(1_000_000)];
    let mut clamped = 0;
    for config in configs {
        for tokens in windows {
            for refuse in [250_000, 120_000, 60_000, 40_000] {
                let policy = WorkerContextPolicy {
                    window_override: tokens.and_then(|t| window(t, SONNET)),
                    resume_refuse_tokens: Some(refuse),
                    ..parent(config)
                };
                match policy.resolve(SONNET) {
                    Ok(resolved) => {
                        let window = resolved.context_window.unwrap_or(200_000);
                        let resolved = resolved.context_management;
                        let at = ContextThresholds::resolve(&resolved, window)
                            .unwrap_or_else(|e| panic!("{config:?} in {window}: {e}"));
                        let headroom =
                            (1 + u64::from(resolved.finish_rounds)) * resolved.reserve_tokens;
                        assert!(
                            at.rollover_tokens + headroom <= refuse,
                            "{config:?} in {window} below {refuse}: {at:?}"
                        );
                        clamped += usize::from(resolved.rollover_tokens == Some(refuse - headroom));
                    }
                    Err(why) => assert!(!why.is_empty()),
                }
            }
        }
    }
    assert!(clamped > 0, "some shapes were clamped");
}
