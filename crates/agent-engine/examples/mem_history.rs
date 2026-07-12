/// mem_history.rs — Slice 0 measurement harness for task #128.
///
/// Builds a synthetic 272-message session whose serialised size lands near 664 KB
/// (assert ±10%), then re-plays TODAY's per-turn copy pipeline while measuring
/// RssAnon (from /proc/self/status) before/after each stage, keeping each stage's
/// output alive simultaneously — that is the entire point.
///
/// Run:
///   cargo run -p synaps-engine --example mem_history --release
///
/// No real session data, no fixture files, no new dependencies.
use serde_json::{json, Value};
use std::hint::black_box;

// ── /proc/self/status RssAnon probe ─────────────────────────────────────────

/// Returns RssAnon in kilobytes, or 0 on non-Linux.
fn rss_anon_kb() -> i64 {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("RssAnon:") {
                let kb: i64 = rest
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                return kb;
            }
        }
        0
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

// ── Synthetic fixture ────────────────────────────────────────────────────────
//
// Distribution target (by bytes):
//   ~65%  assistant tool_use blocks
//   ~25%  user tool_result blocks
//   ~7%   assistant thinking blocks
//   rest  small text messages
//
// Total serialised ≈ 664 KB.  Verified with a ±10% assert.

/// Minimal seeded LCG — no external dependency.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }
    /// ASCII printable char in [0x21, 0x7e].
    fn next_char(&mut self) -> char {
        (0x21u8 + (self.next() % 94) as u8) as char
    }
    /// String of `n` printable ASCII chars.
    fn string(&mut self, n: usize) -> String {
        (0..n).map(|_| self.next_char()).collect()
    }
}

/// Build the 272-message synthetic history (deterministic, seeded).
///
/// Targeting ~664 KB serialised total:
///   tool_use   : 127 msgs × ~3 358 B ≈ 427 KB  (65%)
///   tool_result:  68 msgs × ~2 415 B ≈ 160 KB  (25%)
///   thinking   :  18 msgs × ~2 666 B ≈  47 KB  ( 7%)
///   text       :  59 msgs × ~  548 B ≈  31 KB  ( 5%)
///   total      : 272 msgs              ≈ 665 KB  (within ±10% of 664 KB)
fn build_fixture() -> Vec<Value> {
    let mut rng = Lcg(0xdead_beef_cafe_babe);
    let mut msgs: Vec<Value> = Vec::with_capacity(272);

    // tool_use (assistant) blocks: 127 messages (~65% of bytes)
    // Each: ~3 200 B payload string + ~158 B JSON overhead ≈ 3 358 B/msg → 427 KB total
    for i in 0..127usize {
        let tool_name = format!("tool_{}", i % 12);
        let input_str = rng.string(3_200);
        msgs.push(json!({
            "role": "assistant",
            "content": [
                {
                    "type": "tool_use",
                    "id": format!("toolu_{i:04}"),
                    "name": tool_name,
                    "input": {
                        "command": input_str,
                        "cwd": "/home/user/projects/synaps",
                        "timeout": 30
                    }
                }
            ]
        }));
    }

    // tool_result (user) blocks: 68 messages (~25% of bytes)
    // Each: ~2 300 B payload string + ~115 B JSON overhead ≈ 2 415 B/msg → 160 KB total
    for i in 0..68usize {
        let output = rng.string(2_300);
        msgs.push(json!({
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": format!("toolu_{i:04}"),
                    "content": [{ "type": "text", "text": output }]
                }
            ]
        }));
    }

    // thinking blocks (assistant): 18 messages (~7% of bytes)
    // Each: ~2 600 B payload string + ~66 B JSON overhead ≈ 2 666 B/msg → 47 KB total
    for _ in 0..18usize {
        let thinking = rng.string(2_600);
        msgs.push(json!({
            "role": "assistant",
            "content": [{ "type": "thinking", "thinking": thinking }]
        }));
    }

    // small text messages: 59 messages (rest)
    // Each: ~520 B payload string + ~28 B JSON overhead ≈ 548 B/msg → 31 KB total
    for i in 0..59usize {
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        let text = rng.string(520);
        msgs.push(json!({ "role": role, "content": text }));
    }

    msgs
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║  mem_history — Slice 0 copy-pipeline harness (task #128)    ║");
    println!("╚══════════════════════════════════════════════════════════════╝\n");

    // Build fixture — deterministic, seeded, no real session data.
    let history: Vec<Value> = build_fixture();
    assert_eq!(history.len(), 272, "fixture must be exactly 272 messages");

    let serialised = serde_json::to_vec(&history).expect("serialise fixture");
    let serial_bytes = serialised.len();
    let serial_kb = serial_bytes / 1024;

    println!(
        "Fixture: {} messages, {} bytes ({} KB) serialised\n",
        history.len(),
        serial_bytes,
        serial_kb
    );

    // Assert ±10% of 664 KB target.
    let target_kb: usize = 664;
    let lo = target_kb * 9 / 10; // 597
    let hi = target_kb * 11 / 10; // 730
    assert!(
        serial_kb >= lo && serial_kb <= hi,
        "serialised size {} KB outside ±10% of {} KB target ({}–{} KB)",
        serial_kb,
        target_kb,
        lo,
        hi
    );
    println!(
        "✔  Size assert passed: {} KB ∈ [{}–{}] KB\n",
        serial_kb, lo, hi
    );

    // Drop the serialised buffer — it was just for the size check.
    drop(serialised);

    // Settle allocator, then capture baseline RSS.
    let _ = black_box(history.len());
    let baseline_kb = rss_anon_kb();
    println!("Baseline RssAnon: {} KB\n", baseline_kb);

    // ── Stage 1 ─────────────────────────────────────────────────────────────
    // Mirrors: app.api_messages.clone() → run_stream_with_messages
    //          (stream_handler.rs:264/343/357, dispatch.rs:277/996)
    //          Deep clone of the full history passed into the stream task (C6).
    let before1 = rss_anon_kb();
    let stage1: Vec<Value> = black_box(history.clone());
    let after1 = rss_anon_kb();
    let delta1 = after1 - before1;
    let cum1 = after1 - baseline_kb;

    // ── Stage 2 ─────────────────────────────────────────────────────────────
    // Mirrors: cleaned_messages = messages.to_vec()  (api.rs:709, C7)
    //          Per-API-round copy; alive through the entire streamed response.
    let before2 = rss_anon_kb();
    let stage2: Vec<Value> = black_box(stage1.to_vec());
    let after2 = rss_anon_kb();
    let delta2 = after2 - before2;
    let cum2 = after2 - baseline_kb;

    // ── Stage 3 ─────────────────────────────────────────────────────────────
    // Mirrors: body = json!({ ..., "messages": cleaned_messages, ... })
    //          (api.rs:719-721, C8). json! interpolation runs Serialize →
    //          rebuilds the whole message tree inside a new Value object.
    let before3 = rss_anon_kb();
    let stage3: Value = black_box(json!({
        "model": "claude-opus-4-5",
        "max_tokens": 16_000,
        "stream": true,
        "messages": stage2,
        "system": [{"type": "text", "text": "You are a helpful assistant."}]
    }));
    let after3 = rss_anon_kb();
    let delta3 = after3 - before3;
    let cum3 = after3 - baseline_kb;

    // ── Stage 4 ─────────────────────────────────────────────────────────────
    // Mirrors: messages.to_vec() into ProviderCompleteParams.messages
    //          (openai/mod.rs:204, C11). OpenAI path clone of the full history.
    let before4 = rss_anon_kb();
    let stage4: Vec<Value> = black_box(
        stage3["messages"]
            .as_array()
            .expect("messages array in body")
            .to_vec(),
    );
    let after4 = rss_anon_kb();
    let delta4 = after4 - before4;
    let cum4 = after4 - baseline_kb;

    // ── Table ────────────────────────────────────────────────────────────────
    println!(
        "{:<6}  {:<42}  {:>12}  {:>14}",
        "Stage", "What it mirrors", "Delta KB", "Cumulative KB"
    );
    println!("{}", "─".repeat(80));
    let rows = [
        ("1", "history.clone() → stream task (C6)", delta1, cum1),
        ("2", "messages.to_vec() → cleaned_msgs (C7)", delta2, cum2),
        ("3", "json!({messages:…}) body embed (C8)", delta3, cum3),
        ("4", "body[messages].to_vec() OAI path (C11)", delta4, cum4),
    ];
    for (stage, label, delta, cum) in &rows {
        println!("{:<6}  {:<42}  {:>12}  {:>14}", stage, label, delta, cum);
    }
    println!("{}", "─".repeat(80));

    // ── Inflation analysis ───────────────────────────────────────────────────
    println!("\nSerialized bytes (on-disk / wire):    {} KB", serial_kb);
    if cum4 > 0 {
        let copies: f64 = 4.0;
        let per_copy_kb = cum4 as f64 / copies;
        let inflation = per_copy_kb / serial_kb as f64;
        println!("RSS delta across all 4 live copies:   {} KB", cum4);
        println!(
            "Per-copy RSS cost (cum4 / 4):         {:.0} KB",
            per_copy_kb
        );
        println!("Inflation factor (per-copy / serial): {:.2}×", inflation);
        println!("(scope doc measured 1.43 MB per copy = 2.1× — expect similar)");
    } else {
        println!("(RssAnon probing requires Linux /proc — not available on this platform)");
    }

    // Keep all copies alive until here — that's the measurement contract.
    drop(black_box(stage1));
    drop(black_box(stage2));
    drop(black_box(stage3));
    drop(black_box(stage4));

    // ── SHARED pipeline (post-#128 world) ───────────────────────────────────
    // Same four stages, but the history is Vec<Arc<Value>> and every "copy"
    // is what the Arc-share migration actually does: refcount bumps for
    // stages 1/2/4, and NO body tree rebuild for stage 3 (RequestBody borrows
    // the slice; the wire-byte buffer exists in both worlds and is counted in
    // neither).
    println!("\n── SHARED pipeline (Arc<Value>, slices 2-5) ──\n");
    let shared: Vec<std::sync::Arc<Value>> = history.into_iter().map(std::sync::Arc::new).collect();
    let _ = black_box(shared.len());
    let s_base = rss_anon_kb();

    let sb1 = rss_anon_kb();
    let s1: Vec<std::sync::Arc<Value>> = black_box(shared.clone());
    let sa1 = rss_anon_kb();

    let sb2 = rss_anon_kb();
    let s2: Vec<std::sync::Arc<Value>> = black_box(s1.to_vec());
    let sa2 = rss_anon_kb();

    // Stage 3 equivalent: borrowing serializer — no Value tree is rebuilt.
    let sb3 = rss_anon_kb();
    let s3: &[std::sync::Arc<Value>] = black_box(&s2[..]);
    let sa3 = rss_anon_kb();

    let sb4 = rss_anon_kb();
    let s4: Vec<std::sync::Arc<Value>> = black_box(s3.to_vec());
    let sa4 = rss_anon_kb();

    let s_rows = [
        (
            "1'",
            "shared.clone() → stream task",
            sa1 - sb1,
            sa1 - s_base,
        ),
        (
            "2'",
            "to_vec() → cleaned_msgs (ptr bumps)",
            sa2 - sb2,
            sa2 - s_base,
        ),
        (
            "3'",
            "RequestBody borrow (no tree rebuild)",
            sa3 - sb3,
            sa3 - s_base,
        ),
        (
            "4'",
            "to_vec() OAI path (ptr bumps)",
            sa4 - sb4,
            sa4 - s_base,
        ),
    ];
    println!(
        "{:<6}  {:<42}  {:>12}  {:>14}",
        "Stage", "What it mirrors", "Delta KB", "Cumulative KB"
    );
    println!("{}", "─".repeat(80));
    for (stage, label, delta, cum) in &s_rows {
        println!("{:<6}  {:<42}  {:>12}  {:>14}", stage, label, delta, cum);
    }
    println!("{}", "─".repeat(80));
    let shared_cum = sa4 - s_base;
    println!("\n══ BEFORE/AFTER (this machine, same fixture) ══");
    println!("legacy pipeline, 4 live copies:  {} KB", cum4);
    println!("shared pipeline, 4 live handles: {} KB", shared_cum);
    if cum4 > 0 {
        println!(
            "reduction: {} KB ({:.1}% of the copy overhead eliminated)",
            cum4 - shared_cum,
            100.0 * (cum4 - shared_cum) as f64 / cum4 as f64
        );
    }

    drop(black_box(s4));
    drop(black_box(s2));
    drop(black_box(s1));
    drop(black_box(shared));

    println!("\nDone.");
}
