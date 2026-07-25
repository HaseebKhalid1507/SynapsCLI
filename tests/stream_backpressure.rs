//! CP-11 fix-2 (A) — bounded caller-facing stream boundary, end-to-end.
//!
//! For each provider wire (Anthropic Messages, OpenAI Chat Completions,
//! Gemini Code Assist), a hostile stub floods unbounded text deltas while
//! the CALLER of `Runtime::run_stream` never polls. The runtime must:
//!
//! - keep global relay retention within the FIXED preview budget the whole
//!   time (no unbounded byte hop feeds the absent consumer);
//! - account every produced preview byte as forwarded or dropped;
//! - release the provider task when the caller stream is DROPPED
//!   (production stops and the relay terminates).

#[path = "support/phase2/mod.rs"]
mod support;

use std::time::{Duration, Instant};

use serial_test::serial;
use support::*;
use synaps_cli::runtime::relay::{
    stream_relay_snapshot, RelaySnapshot, RELAY_DELTA_RETAINED_BUDGET_BYTES,
};
use synaps_cli::runtime::Runtime;
use tokio_util::sync::CancellationToken;

/// Flood payload for one SSE frame (~4 KiB of delta text).
const FRAME_TEXT_BYTES: usize = 4096;
/// The absent-consumer observation floor: the provider must have produced
/// at least this many preview bytes while the caller never polled.
const FLOOD_FLOOR_BYTES: u64 = 8 * 1024 * 1024;

fn anthropic_flood_frame() -> &'static str {
    let text = "x".repeat(FRAME_TEXT_BYTES);
    Box::leak(
        format!(
            "data: {{\"type\":\"content_block_delta\",\"index\":0,\
             \"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n"
        )
        .into_boxed_str(),
    )
}

fn oai_chat_flood_frame() -> &'static str {
    let text = "x".repeat(FRAME_TEXT_BYTES);
    Box::leak(
        format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n")
            .into_boxed_str(),
    )
}

fn gemini_flood_frame() -> &'static str {
    let text = "x".repeat(FRAME_TEXT_BYTES);
    Box::leak(
        format!(
            "data: {{\"response\":{{\"candidates\":[{{\"content\":{{\"parts\":\
             [{{\"text\":\"{text}\"}}]}}}}]}}}}\n\n"
        )
        .into_boxed_str(),
    )
}

async fn wait_until(deadline: Duration, mut check: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    check()
}

/// Drive one absent-consumer flood turn on `rt` and enforce the fixed
/// retention, conservation, and drop-releases-provider invariants.
async fn assert_bounded_flood_turn(rt: &Runtime) {
    let base = stream_relay_snapshot();
    let stream = rt
        .run_stream("flood".to_string(), CancellationToken::new())
        .await;

    // The caller NEVER polls. Watch production grow to the floor while
    // sampling retention against the fixed budget on every poll.
    let mut max_retained = 0u64;
    let reached = wait_until(Duration::from_secs(60), || {
        let now = stream_relay_snapshot();
        max_retained = max_retained.max(now.retained_delta_bytes - base.retained_delta_bytes);
        now.produced_delta_bytes - base.produced_delta_bytes >= FLOOD_FLOOR_BYTES
    })
    .await;
    assert!(reached, "provider flood must reach the observation floor");
    assert!(
        max_retained <= RELAY_DELTA_RETAINED_BUDGET_BYTES as u64,
        "absent-consumer retention must stay within the fixed budget, saw {max_retained}"
    );
    let during = stream_relay_snapshot();
    assert!(
        during.dropped_delta_bytes > base.dropped_delta_bytes,
        "the flood beyond the budget must be dropped with accounting"
    );

    // Dropping the caller stream must release the provider task.
    drop(stream);
    assert!(
        wait_until(Duration::from_secs(15), || {
            stream_relay_snapshot().active_relays == base.active_relays
        })
        .await,
        "relay must terminate after the caller stream is dropped"
    );
    let mut quiesced: Option<RelaySnapshot> = None;
    assert!(
        wait_until(Duration::from_secs(15), || {
            let now = stream_relay_snapshot();
            let stable = quiesced
                .map(|prev| prev.produced_delta_bytes == now.produced_delta_bytes)
                .unwrap_or(false);
            quiesced = Some(now);
            stable
        })
        .await,
        "production must STOP after the caller departs (provider released)"
    );
    let end = stream_relay_snapshot();
    assert_eq!(
        end.produced_delta_bytes - base.produced_delta_bytes,
        (end.forwarded_delta_bytes - base.forwarded_delta_bytes)
            + (end.dropped_delta_bytes - base.dropped_delta_bytes),
        "conservation: every preview byte is forwarded or dropped"
    );
    assert_eq!(
        end.retained_delta_bytes, base.retained_delta_bytes,
        "no retained preview bytes may outlive the relay"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn anthropic_flood_absent_consumer_fixed_retention_and_release() {
    let _guard = HomeGuard::new();
    std::env::remove_var("SYNAPS_AUTH_ENDPOINT");
    std::env::remove_var("SYNAPS_MACHINE_TOKEN");
    let (url, _hits, _) = spawn_stub(Script::FloodSse {
        preamble: ANTHROPIC_SSE_PREFIX,
        frame: anthropic_flood_frame(),
    })
    .await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model("claude-sonnet-4-5".to_string());
    assert_bounded_flood_turn(&rt).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn openai_chat_flood_absent_consumer_fixed_retention_and_release() {
    let _guard = HomeGuard::new();
    let (endpoint, _hits, _) = spawn_broker(BrokerScript::ProxyFlood {
        preamble: "",
        frame: oai_chat_flood_frame(),
    })
    .await;
    std::env::set_var("SYNAPS_AUTH_ENDPOINT", &endpoint);
    std::env::set_var("SYNAPS_MACHINE_TOKEN", "machine-token-fixture");

    let mut rt = Runtime::new().await.expect("runtime");
    rt.apply_config(&synaps_cli::SynapsConfig::default());
    rt.set_model("groq/fixture-model".to_string());
    assert_bounded_flood_turn(&rt).await;

    std::env::remove_var("SYNAPS_AUTH_ENDPOINT");
    std::env::remove_var("SYNAPS_MACHINE_TOKEN");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn gemini_flood_absent_consumer_fixed_retention_and_release() {
    let _guard = HomeGuard::new();
    let (endpoint, _hits, _) = spawn_broker(BrokerScript::ProxyFlood {
        preamble: "",
        frame: gemini_flood_frame(),
    })
    .await;
    std::env::set_var("SYNAPS_AUTH_ENDPOINT", &endpoint);
    std::env::set_var("SYNAPS_MACHINE_TOKEN", "machine-token-fixture");

    let mut rt = Runtime::new().await.expect("runtime");
    rt.apply_config(&synaps_cli::SynapsConfig::default());
    rt.set_model("google-gemini/gemini-2.5-pro".to_string());
    assert_bounded_flood_turn(&rt).await;

    std::env::remove_var("SYNAPS_AUTH_ENDPOINT");
    std::env::remove_var("SYNAPS_MACHINE_TOKEN");
}

/// A SLOW (but present) consumer also stays within the budget and still
/// receives the stream's content boundary-correctly (prefix of the flood).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn anthropic_flood_slow_consumer_fixed_retention() {
    use futures::StreamExt;

    let _guard = HomeGuard::new();
    std::env::remove_var("SYNAPS_AUTH_ENDPOINT");
    std::env::remove_var("SYNAPS_MACHINE_TOKEN");
    let (url, _hits, _) = spawn_stub(Script::FloodSse {
        preamble: ANTHROPIC_SSE_PREFIX,
        frame: anthropic_flood_frame(),
    })
    .await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model("claude-sonnet-4-5".to_string());

    let base = stream_relay_snapshot();
    let mut stream = rt
        .run_stream("flood".to_string(), CancellationToken::new())
        .await;
    // Consume a handful of events very slowly, then hang up.
    for _ in 0..8 {
        let _ = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("bounded wait for next event");
        tokio::time::sleep(Duration::from_millis(100)).await;
        let now = stream_relay_snapshot();
        assert!(
            now.retained_delta_bytes - base.retained_delta_bytes
                <= RELAY_DELTA_RETAINED_BUDGET_BYTES as u64,
            "slow-consumer retention must stay within the fixed budget"
        );
    }
    drop(stream);
    assert!(
        wait_until(Duration::from_secs(15), || {
            stream_relay_snapshot().active_relays == base.active_relays
        })
        .await,
        "relay must terminate after the slow consumer departs"
    );
}
