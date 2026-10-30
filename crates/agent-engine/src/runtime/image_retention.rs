//! Request-local image retention: one deterministic replay of canonical USER
//! batches, shared by every route and role. No persisted/runtime state.
//! Editing, truncating or compacting history starts a new replay window: this
//! cannot guarantee non-resurrection across changes to the original history.
//!
//! # Why chunks, and why a frontier
//!
//! Retiring an image rewrites the request from that image onward. Anthropic
//! looks back at most 20 blocks from each cache breakpoint for an earlier
//! cache entry, so a retirement deep in the history (the oldest kept image is
//! usually far more than 20 blocks behind the tail) misses every message-level
//! entry and the whole conversation after the system prompt is written again.
//! Every budget therefore retires in chunks, down to a low-water mark, so the
//! history changes once per chunk instead of once per new image. The replay
//! also reports the retention frontier: the oldest image the PREVIOUS request
//! still sent. Nothing before it differs between the two requests, so the
//! native transport puts a cache breakpoint on the last whole message ahead
//! of the one holding it (`HelperMethods::annotate_retention_anchor`; never
//! an earlier block of that same message) and a retirement re-reads the
//! prefix up to there instead of rewriting it.
use crate::SharedMessage;
use serde_json::{json, Value};
use std::sync::Arc;

use super::attachments::{serialized_size, MAX_HISTORY_ENCODED_BYTES};
use super::openai::catalog::CodexRequestRole;

const MAX_IMAGES: usize = 100;
const CHUNK: usize = 40;
const LOW_WATER: usize = 12 * 1024 * 1024;
pub(super) const MAX_ORIGINAL_BYTES: usize = 256 * 1024 * 1024;
const MAX_ORIGINAL_IMAGES: usize = 4096;
/// Worker uploads must leave uplink capacity for foreground sessions: a
/// worker request carries at most this many images / base64 bytes.
pub(crate) const WORKER_IMAGE_COUNT: usize = 6;
pub(crate) const WORKER_IMAGE_BYTES: usize = 4 * 1024 * 1024;
/// Every route except the native one: total base64 image payload per request.
/// Anthropic-compatible gateways cap a request near 32 MB; leave text headroom.
pub(crate) const ROUTED_IMAGE_BYTES: usize = 20 * 1024 * 1024;
/// The one label a retired image leaves behind, on every route.
pub(super) const OMITTED: &str =
    "[Image omitted from this request by history retention. Re-read the file if needed.]";
const PROTECTED: &str =
    "Attachment history cannot fit documents and the current image batch within the request budget";

/// The image budget a request is replayed against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Budget {
    /// Native Anthropic history retention. Admission (`validate_history_for_request`)
    /// replays this budget too, so a batch that cannot fit is refused up front.
    Native,
    /// Worker upload budget, on every route.
    Worker,
    /// Byte budget for every non-native route.
    Routed,
}

/// High-water limits trigger a retirement; one retirement then goes down to
/// the low-water limits (soft: the batch being sent is never retired for them).
struct Limits {
    max_images: usize,
    max_bytes: usize,
    low_images: usize,
    low_bytes: usize,
    /// At least this many older occurrences go per retirement.
    min_chunk: usize,
    /// Native: serialized block size, documents count, and the current batch
    /// is protected (refused, never retired). Otherwise: base64 payload bytes,
    /// documents are ignored, and the current batch is trimmed oldest first
    /// only as far as the high-water limits require.
    native: bool,
}

const NATIVE_LIMITS: Limits = Limits {
    max_images: MAX_IMAGES,
    max_bytes: MAX_HISTORY_ENCODED_BYTES,
    low_images: MAX_IMAGES,
    low_bytes: LOW_WATER,
    min_chunk: CHUNK,
    native: true,
};
const WORKER_LIMITS: Limits = Limits {
    max_images: WORKER_IMAGE_COUNT,
    max_bytes: WORKER_IMAGE_BYTES,
    low_images: WORKER_IMAGE_COUNT / 2,
    low_bytes: WORKER_IMAGE_BYTES / 2,
    min_chunk: 0,
    native: false,
};
const ROUTED_LIMITS: Limits = Limits {
    max_images: usize::MAX,
    max_bytes: ROUTED_IMAGE_BYTES,
    low_images: usize::MAX,
    low_bytes: LOW_WATER,
    min_chunk: 0,
    native: false,
};

impl Budget {
    /// The budget for a request to `model` from `role`.
    pub(crate) fn for_request(model: &str, role: CodexRequestRole) -> Self {
        if role == CodexRequestRole::Worker {
            Budget::Worker
        } else if super::attachments::native_image_retention(model) {
            Budget::Native
        } else {
            Budget::Routed
        }
    }

    fn limits(self) -> &'static Limits {
        match self {
            Budget::Native => &NATIVE_LIMITS,
            Budget::Worker => &WORKER_LIMITS,
            Budget::Routed => &ROUTED_LIMITS,
        }
    }
}

/// One request's retention: a pure function of the history and the budget.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Retention {
    /// Image occurrences retired from this request, oldest first. Monotonic
    /// as the history grows.
    pub retired: usize,
    /// `(message, top-level block)` of the oldest image occurrence the
    /// previous request (the history before the last assistant message) still
    /// sent. `None` when no such occurrence is left in the history.
    pub frontier: Option<(usize, usize)>,
}

/// The earlier of two frontiers: an anchor ahead of both is ahead of every
/// block either retention may change.
pub(super) fn earliest(
    a: Option<(usize, usize)>,
    b: Option<(usize, usize)>,
) -> Option<(usize, usize)> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Original-batch watermark for the native budget (admission and tests).
pub(super) fn plan(messages: &[SharedMessage]) -> Result<usize, String> {
    plan_request(messages, Budget::Native).map(|r| r.retired)
}

/// Replays the history batch by batch. Each user message is a batch, including
/// all sibling tool results. The native budget runs after original structural
/// validation and before same-role merging.
pub(crate) fn plan_request(
    messages: &[SharedMessage],
    budget: Budget,
) -> Result<Retention, String> {
    let limits = budget.limits();
    if limits.native {
        resource_prepass(messages)?;
    }
    // The previous request ended with the user batch before the response that
    // closes it: the last assistant message.
    let previous_end = messages.iter().rposition(|m| m["role"] == "assistant");
    let mut sizes: Vec<usize> = Vec::new();
    let mut positions: Vec<(usize, usize)> = Vec::new();
    let mut retired = 0;
    let mut previous_retired = 0;
    let mut retained_bytes = 0usize;
    let mut documents = 0usize;
    for (index, message) in messages.iter().enumerate() {
        if Some(index) == previous_end {
            previous_retired = retired;
        }
        if message["role"] != "user" {
            continue;
        }
        let batch_start = sizes.len();
        if let Some(blocks) = message["content"].as_array() {
            for (block_index, block) in blocks.iter().enumerate() {
                media(std::slice::from_ref(block), &mut |item| {
                    let bytes = if limits.native {
                        serialized_size(item, MAX_ORIGINAL_BYTES)
                            .ok_or("Original attachment exceeds resource limit")?
                    } else {
                        base64_len(item)
                    };
                    if item["type"] == "image" {
                        sizes.push(bytes);
                        positions.push((index, block_index));
                        retained_bytes += bytes;
                    } else if limits.native {
                        documents += bytes;
                    }
                    Ok(())
                })?;
            }
        }
        let over = |retired: usize, retained_bytes: usize, images: usize, bytes: usize| {
            sizes.len() - retired > images || documents + retained_bytes > bytes
        };
        if !over(retired, retained_bytes, limits.max_images, limits.max_bytes) {
            continue;
        }
        let chunk_target = (retired + limits.min_chunk).min(batch_start);
        while retired < batch_start
            && (retired < chunk_target
                || over(retired, retained_bytes, limits.low_images, limits.low_bytes))
        {
            retained_bytes -= sizes[retired];
            retired += 1;
        }
        if over(retired, retained_bytes, limits.max_images, limits.max_bytes) {
            if limits.native {
                return Err(PROTECTED.into());
            }
            while retired < sizes.len()
                && over(retired, retained_bytes, limits.max_images, limits.max_bytes)
            {
                retained_bytes -= sizes[retired];
                retired += 1;
            }
        }
    }
    Ok(Retention {
        retired,
        frontier: positions.get(previous_retired).copied(),
    })
}

/// Wire payload of a non-native image: its base64 data. URL images count
/// toward the image limit but carry no embedded payload.
fn base64_len(block: &Value) -> usize {
    if block["source"]["type"] == "base64" {
        block["source"]["data"].as_str().map_or(0, str::len)
    } else {
        0
    }
}

/// Replace the first `retired` image occurrences (the replay's order) with the
/// [`OMITTED`] label. Clones only the messages it edits.
pub(super) fn retire(messages: &mut [SharedMessage], retired: usize) {
    let mut occurrence = 0;
    for message in messages {
        if occurrence >= retired {
            break;
        }
        if message["role"] != "user" {
            continue;
        }
        let has_image = message["content"].as_array().is_some_and(|blocks| {
            let mut found = false;
            let _ = media(blocks, &mut |b| {
                found |= b["type"] == "image";
                Ok(())
            });
            found
        });
        if !has_image {
            continue;
        }
        if let Some(blocks) = Arc::make_mut(message)["content"].as_array_mut() {
            for block in blocks {
                if block["type"] == "image" {
                    retire_one(block, &mut occurrence, retired);
                } else if block["type"] == "tool_result" {
                    if let Some(nested) = block["content"].as_array_mut() {
                        for item in nested {
                            if item["type"] == "image" {
                                retire_one(item, &mut occurrence, retired);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
pub(super) fn apply(messages: &mut [SharedMessage]) -> Result<usize, String> {
    let watermark = plan(messages)?;
    retire(messages, watermark);
    Ok(watermark)
}

fn retire_one(block: &mut Value, occurrence: &mut usize, retired: usize) {
    if *occurrence < retired {
        *block = json!({"type": "text", "text": OMITTED});
    }
    *occurrence += 1;
}

/// The image view of one request round, shared by the stream loop and the
/// keep-warm ping so the two can never drift.
#[derive(Debug)]
pub(super) enum RoundImages {
    /// Nothing retired: send and estimate the history as is.
    Unchanged,
    /// Native budget: the native Anthropic transport replays it from the
    /// ORIGINAL history itself (projection checks every original, retired
    /// ones included), so the history is sent as is and only the context
    /// estimate counts this view.
    EstimateOnly(Vec<SharedMessage>),
    /// Worker and routed budgets: send and estimate this view. `frontier` is
    /// the retention frontier, a position that is the same in the view (images
    /// are retired in place), for the transport's cache anchor
    /// ([`super::api::ApiOptions::retention_frontier`]).
    Retained {
        view: Vec<SharedMessage>,
        frontier: Option<(usize, usize)>,
    },
}

impl RoundImages {
    /// `(sent, estimated)` messages for `history`.
    pub(super) fn views<'a>(
        &'a self,
        history: &'a [SharedMessage],
    ) -> (&'a [SharedMessage], &'a [SharedMessage]) {
        match self {
            RoundImages::Unchanged => (history, history),
            RoundImages::EstimateOnly(view) => (history, view),
            RoundImages::Retained { view, .. } => (view, view),
        }
    }

    /// The frontier of a retention already applied to the sent messages.
    pub(super) fn applied_frontier(&self) -> Option<(usize, usize)> {
        match self {
            RoundImages::Retained { frontier, .. } => *frontier,
            _ => None,
        }
    }
}

/// Request-local retention of one round to `model` from `role`. Durable
/// history is never touched.
pub(super) fn round_images(
    model: &str,
    role: CodexRequestRole,
    messages: &[SharedMessage],
) -> Result<RoundImages, String> {
    let budget = Budget::for_request(model, role);
    let retention = plan_request(messages, budget)?;
    if retention.retired == 0 {
        return Ok(RoundImages::Unchanged);
    }
    let mut view = messages.to_vec();
    retire(&mut view, retention.retired);
    tracing::debug!(
        ?budget,
        retired = retention.retired,
        "request image retention applied"
    );
    Ok(match budget {
        Budget::Native => RoundImages::EstimateOnly(view),
        Budget::Worker | Budget::Routed => RoundImages::Retained {
            view,
            frontier: retention.frontier,
        },
    })
}

/// Walk only canonical content positions, never tool inputs or opaque metadata.
fn media(
    blocks: &[Value],
    visit: &mut impl FnMut(&Value) -> Result<(), String>,
) -> Result<(), String> {
    for block in blocks {
        match block["type"].as_str() {
            Some("image" | "document") => visit(block)?,
            Some("tool_result") => {
                if let Some(nested) = block["content"].as_array() {
                    // One canonical level only; structural validation rejects deeper media.
                    for item in nested {
                        if matches!(item["type"].as_str(), Some("image" | "document")) {
                            visit(item)?;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Allocation-free, bounded serialization before base64 decode or occurrence
/// vectors. Count occurrences, not unique hashes. Malformed shapes still go
/// through the independent structural validator (which rejects them).
pub(super) fn resource_prepass(messages: &[SharedMessage]) -> Result<(), String> {
    let mut bytes = 0;
    let mut images = 0;
    for message in messages {
        if let Some(blocks) = message["content"].as_array() {
            media(blocks, &mut |block| {
                bytes += serialized_size(block, MAX_ORIGINAL_BYTES - bytes)
                    .ok_or("Original attachment history exceeds the 256 MiB resource limit")?;
                if block["type"] == "image" {
                    images += 1;
                    if images > MAX_ORIGINAL_IMAGES {
                        return Err(
                            "Original attachment history exceeds 4096 image occurrences".into()
                        );
                    }
                }
                Ok(())
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn batch(images: usize, bytes: usize) -> SharedMessage {
        Arc::new(json!({"role":"user", "content": (0..images).map(|_| json!({
            "type":"image", "source":{"data":"a".repeat(bytes)}
        })).collect::<Vec<_>>()}))
    }
    #[test]
    fn count_chunks_identical_occurrences_and_reload() {
        let mut history = vec![batch(1, 1); 100];
        assert_eq!(plan(&history).unwrap(), 0);
        history.push(batch(1, 1));
        assert_eq!(plan(&history).unwrap(), 40);
        for _ in 102..=140 {
            history.push(batch(1, 1));
            assert_eq!(plan(&history).unwrap(), 40);
        }
        let original = serde_json::to_string(&history).unwrap();
        let reloaded: Vec<SharedMessage> = serde_json::from_str(&original).unwrap();
        let mut outgoing = reloaded.clone();
        assert_eq!(apply(&mut outgoing).unwrap(), 40);
        assert_eq!(outgoing[0]["content"][0]["text"], OMITTED);
        assert_eq!(serde_json::to_string(&history).unwrap(), original);
        history.push(batch(1, 1));
        assert_eq!(plan(&history).unwrap(), 80);
    }
    #[test]
    fn byte_only_combined_and_headroom() {
        let h = vec![batch(1, 1024 * 1024); 21];
        // At batch 20 the serialized blocks overflow: all 19 older images go.
        assert_eq!(plan(&h).unwrap(), 19);
        let h = vec![batch(1, 210_000); 101];
        assert_eq!(plan(&h).unwrap(), 41); // 60 retained fit the 12 MiB target.
        let h = vec![batch(1, 300_000); 100];
        assert!(plan(&h).unwrap() >= 40);
    }
    #[test]
    fn low_water_is_soft_and_only_applies_after_hard_overflow() {
        // Synthetic serialized sizes: intentionally not valid per-item media.
        let protected = batch(1, 13 * 1024 * 1024);
        assert_eq!(plan(std::slice::from_ref(&protected)).unwrap(), 0);
        let older = batch(1, 8 * 1024 * 1024);
        assert_eq!(plan(&[older, protected]).unwrap(), 1);
        // The protected 13 MiB batch cannot meet 12 MiB or a 40-image chunk;
        // both soft targets yield once all available older images are retired.
    }

    #[test]
    fn documents_accumulate_and_bad_protected_batch_stays_bad() {
        let doc = Arc::new(json!({"role":"user", "content":[{
            "type":"document", "source":{"data":"d".repeat(11 * 1024 * 1024)}
        }]}));
        let h = vec![
            batch(1, 1024 * 1024),
            doc.clone(),
            batch(1, 9 * 1024 * 1024),
        ];
        assert!(plan(&h).is_err());
        let mut h = vec![batch(101, 1)];
        assert!(plan(&h).is_err());
        h.push(Arc::new(json!({"role":"user","content":"later text"})));
        assert!(plan(&h).is_err());
        assert!(plan(&[doc.clone(), doc]).is_err());
    }
    #[test]
    fn nested_batches_and_resource_cap() {
        let mut h = vec![batch(1, 1); 100];
        h.push(Arc::new(json!({"role":"user","content":[{
            "type":"tool_result","tool_use_id":"a","content":[{"type":"image"}]
        }]})));
        assert_eq!(plan(&h).unwrap(), 40);
        assert!(resource_prepass(&vec![batch(1, 1); 4097]).is_err());
    }

    // ─── One replay, three budgets ───────────────────────────────────────

    /// The native replay as it was before the budgets were generalized,
    /// verbatim: the oracle the native budget must match exactly, so a
    /// session's retired prefix (and with it its cache) survives the upgrade.
    fn legacy_native_plan(messages: &[SharedMessage]) -> Result<usize, String> {
        resource_prepass(messages)?;
        let mut sizes = Vec::new();
        let mut retired = 0;
        let mut retained_bytes = 0usize;
        let mut documents = 0usize;
        for message in messages {
            if message["role"] != "user" {
                continue;
            }
            let batch_start = sizes.len();
            if let Some(blocks) = message["content"].as_array() {
                media(blocks, &mut |block| {
                    let bytes = serialized_size(block, MAX_ORIGINAL_BYTES)
                        .ok_or("Original attachment exceeds resource limit")?;
                    if block["type"] == "image" {
                        sizes.push(bytes);
                        retained_bytes += bytes;
                    } else {
                        documents += bytes;
                    }
                    Ok(())
                })?;
            }
            if sizes.len() - retired <= MAX_IMAGES
                && documents + retained_bytes <= MAX_HISTORY_ENCODED_BYTES
            {
                continue;
            }
            let chunk_target = (retired + CHUNK).min(batch_start);
            while retired < batch_start
                && (retired < chunk_target
                    || documents + retained_bytes > LOW_WATER
                    || sizes.len() - retired > MAX_IMAGES)
            {
                retained_bytes -= sizes[retired];
                retired += 1;
            }
            if sizes.len() - retired > MAX_IMAGES
                || documents + retained_bytes > MAX_HISTORY_ENCODED_BYTES
            {
                return Err(PROTECTED.into());
            }
        }
        Ok(retired)
    }

    /// Deterministic pseudo-random source (no extra dev-dependency).
    struct Lcg(u64);
    impl Lcg {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % n
        }
    }

    /// Random user batches (top-level and nested images, the odd document)
    /// with assistant replies between some of them, drawn from `pool`.
    fn random_history(rng: &mut Lcg, pool: &[Value], max_batches: u64) -> Vec<SharedMessage> {
        let mut history = Vec::new();
        for _ in 0..1 + rng.below(max_batches) {
            let mut content = Vec::new();
            for _ in 0..rng.below(4) {
                let item = pool[rng.below(pool.len() as u64) as usize].clone();
                if rng.below(2) == 0 {
                    content.push(item);
                } else {
                    content.push(json!({"type":"tool_result","tool_use_id":"t",
                        "content":[{"type":"text","text":"shot"}, item]}));
                }
            }
            if rng.below(30) == 0 {
                content.push(json!({"type":"document","source":{"type":"base64",
                    "media_type":"application/pdf","data":"d".repeat(1 + rng.below(2 << 20) as usize)}}));
            }
            history.push(Arc::new(json!({"role":"user","content":content})));
            if rng.below(3) != 0 {
                history.push(Arc::new(json!({"role":"assistant","content":"ok"})));
            }
        }
        history
    }

    fn image_pool(sizes: &[usize]) -> Vec<Value> {
        sizes
            .iter()
            .map(|n| json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"a".repeat(*n)}}))
            .collect()
    }

    #[test]
    fn native_budget_matches_the_previous_replay_exactly() {
        // Mostly small images (the count limit and its 40-image chunk), some
        // large ones and documents (the byte limit, low water, refusals).
        let pool = image_pool(&[10, 10, 500, 500, 30_000, 400_000]);
        let mut rng = Lcg(0x5eed);
        let (mut retiring, mut refused) = (0, 0);
        let user = |content: Vec<Value>| Arc::new(json!({"role":"user","content":content}));
        let doc = |bytes: usize| {
            json!({"type":"document","source":{"type":"base64",
            "media_type":"application/pdf","data":"d".repeat(bytes)}})
        };
        // Refusals the random draw rarely reaches: one batch over the image
        // count, documents leaving no room for the current batch, and a
        // retiring history that ends in such a batch.
        let mut crafted = vec![
            vec![user((0..101).map(|_| pool[0].clone()).collect())],
            vec![user(vec![doc(19 << 20)]), user(vec![pool[5].clone(); 4])],
            vec![user(vec![doc(21 << 20), pool[0].clone()])],
        ];
        let mut tail = random_history(&mut Lcg(7), &pool, 150);
        tail.push(user((0..101).map(|_| pool[1].clone()).collect()));
        crafted.push(tail);
        let random = (0..40).map(|_| random_history(&mut rng, &pool, 150));
        for history in crafted.into_iter().chain(random) {
            let expected = legacy_native_plan(&history);
            retiring += usize::from(expected.as_ref().is_ok_and(|r| *r > 0));
            refused += usize::from(expected.is_err());
            assert_eq!(plan(&history), expected);
        }
        assert!(
            retiring >= 8 && refused >= 1,
            "oracle coverage: {retiring} retiring, {refused} refused"
        );
    }

    #[test]
    fn retirement_is_monotonic_and_a_pure_function_of_the_history() {
        for (budget, sizes) in [
            (Budget::Native, &[10, 500][..]),
            (Budget::Worker, &[10, 500, 300_000][..]),
            (Budget::Routed, &[200_000, 400_000][..]),
        ] {
            let pool = image_pool(sizes);
            // The first seed whose history retires (deterministic).
            let history = (1..50)
                .map(|seed| random_history(&mut Lcg(seed), &pool, 200))
                .find(|h| plan_request(h, budget).is_ok_and(|r| r.retired > 0))
                .expect("a retiring history");
            let mut last = 0;
            for end in 1..=history.len() {
                let Ok(retention) = plan_request(&history[..end], budget) else {
                    // A refused batch stays refused: every longer history is too.
                    assert!(plan_request(&history, budget).is_err());
                    break;
                };
                assert!(retention.retired >= last, "{budget:?} at {end}");
                last = retention.retired;
            }
            let reloaded: Vec<SharedMessage> =
                serde_json::from_str(&serde_json::to_string(&history).unwrap()).unwrap();
            assert_eq!(
                plan_request(&reloaded, budget),
                plan_request(&history, budget)
            );
        }
    }

    fn shot(bytes: usize) -> Value {
        json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"a".repeat(bytes)}})
    }

    /// A worker's tool loop: one screenshot per round. Screenshot `i` sits in
    /// message `2 + 2i`, block 0 (a tool_result).
    fn worker_loop(rounds: usize, bytes: usize) -> Vec<SharedMessage> {
        let mut history = vec![Arc::new(json!({"role":"user","content":"go"}))];
        for i in 0..rounds {
            let id = format!("t{i}");
            history.push(Arc::new(json!({"role":"assistant","content":[
                {"type":"tool_use","id":id,"name":"screenshot","input":{}}
            ]})));
            history.push(Arc::new(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":id,
                 "content":[{"type":"text","text":format!("shot {i}")}, shot(bytes)]}
            ]})));
        }
        history
    }

    /// A tool loop whose round `i` takes `layout[i]` screenshots in parallel:
    /// one assistant message with that many tool_uses, then one user batch
    /// with as many tool_results, screenshot `j` of the round in block `j`.
    fn screenshot_loop(layout: &[usize], bytes: usize) -> Vec<SharedMessage> {
        let mut history = vec![Arc::new(json!({"role":"user","content":"go"}))];
        for (i, &shots) in layout.iter().enumerate() {
            let ids: Vec<String> = (0..shots).map(|j| format!("t{i}.{j}")).collect();
            history.push(Arc::new(
                json!({"role":"assistant","content":ids.iter().map(|id| json!(
                {"type":"tool_use","id":id,"name":"screenshot","input":{}}
            )).collect::<Vec<_>>()}),
            ));
            history.push(Arc::new(
                json!({"role":"user","content":ids.iter().map(|id| json!(
                {"type":"tool_result","tool_use_id":id,
                 "content":[{"type":"text","text":format!("shot {id}")}, shot(bytes)]}
            )).collect::<Vec<_>>()}),
            ));
        }
        history
    }

    /// The screenshot rounds of the worker behind the 2026-10-30 cache-break
    /// report (thinking-ui, sa_13): 41 screenshots in 34 tool rounds, with
    /// the parallel pairs where they fell (text-only rounds in between do not
    /// move the replay). Taken from its saved history.
    const SA13: [usize; 34] = [
        1, 1, 2, 1, 2, 1, 2, 2, 1, 1, 1, 2, 1, 1, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 2, 1,
    ];

    /// The report read that worker's retirements as one image per new
    /// screenshot, because it logged only the first block each one changed.
    /// The replay (which reproduces every anchor index the telemetry logged)
    /// retires in chunks: nine rewrites for 41 screenshots, four or five
    /// images each, on the native and the routed worker route alike. A
    /// budget without hysteresis (retire down to the cap) would rewrite on
    /// every screenshot past the sixth.
    #[test]
    fn sa13_screenshot_session_rewrites_once_per_chunk_on_every_worker_route() {
        for model in ["claude-opus-4-6", "groq/llama-3.3-70b-versatile"] {
            let mut previous: Option<(usize, Vec<SharedMessage>)> = None;
            let mut chunks = Vec::new();
            for n in 1..=SA13.len() {
                let history = screenshot_loop(&SA13[..n], 1_000);
                let round = round_images(model, CodexRequestRole::Worker, &history).unwrap();
                let (sent, _) = round.views(&history);
                let retired = plan_request(&history, Budget::Worker).unwrap().retired;
                if let Some((before, previous)) = &previous {
                    // A rewrite: a message the previous request sent changed.
                    let rewrote = previous.iter().zip(sent).any(|(a, b)| a != b);
                    assert_eq!(rewrote, retired > *before, "{model} round {n}");
                    if rewrote {
                        chunks.push(retired - before);
                    }
                }
                previous = Some((retired, sent.to_vec()));
            }
            assert_eq!(chunks, [4, 5, 5, 4, 4, 4, 4, 4, 4], "{model}");
        }
    }

    fn retired_by_round(budget: Budget, rounds: usize, bytes: usize) -> Vec<usize> {
        (1..=rounds)
            .map(|n| {
                plan_request(&worker_loop(n, bytes), budget)
                    .unwrap()
                    .retired
            })
            .collect()
    }

    #[test]
    fn worker_budget_retires_in_chunks_down_to_half() {
        // Count: the 7th screenshot crosses 6 and four go at once (down to
        // 3); the next three append without touching the history.
        assert_eq!(
            retired_by_round(Budget::Worker, 15, 1_000),
            [0, 0, 0, 0, 0, 0, 4, 4, 4, 4, 8, 8, 8, 8, 12]
        );
        // Bytes: 1 MiB screenshots cross 4 MiB on the 5th, down to 2 MiB.
        assert_eq!(
            retired_by_round(Budget::Worker, 8, 1024 * 1024),
            [0, 0, 0, 0, 3, 3, 3, 6]
        );
        // The routed budget has no count limit: small screenshots never go.
        assert!(retired_by_round(Budget::Routed, 120, 1_000)
            .iter()
            .all(|r| *r == 0));
    }

    #[test]
    fn worker_budget_trims_an_oversized_newest_batch_only_as_far_as_needed() {
        let batch = |n: usize| {
            Arc::new(json!({"role":"user","content":(0..n).map(|_| shot(10)).collect::<Vec<_>>()}))
        };
        // Never-sent images of the newest batch go only for the high limits.
        assert_eq!(
            plan_request(&[batch(8)], Budget::Worker).unwrap().retired,
            2
        );
        assert_eq!(
            plan_request(&[batch(2), batch(8)], Budget::Worker)
                .unwrap()
                .retired,
            4
        );
        // A screenshot over the whole byte budget is never sent.
        let huge = Arc::new(json!({"role":"user","content":[shot(5 * 1024 * 1024)]}));
        assert_eq!(plan_request(&[huge], Budget::Worker).unwrap().retired, 1);
        // URL images count toward the image limit; documents are not images.
        let url = Arc::new(json!({"role":"user","content":[
            {"type":"image","source":{"type":"url","url":"https://example.invalid/a.png"}}
        ]}));
        assert_eq!(
            plan_request(&vec![url; 7], Budget::Worker).unwrap().retired,
            4
        );
        let pdf = Arc::new(json!({"role":"user","content":[{"type":"document",
            "source":{"type":"base64","media_type":"application/pdf","data":"d".repeat(30 << 20)}}]}));
        assert_eq!(
            plan_request(&[pdf, batch(1)], Budget::Worker)
                .unwrap()
                .retired,
            0
        );
    }

    #[test]
    fn frontier_is_the_oldest_image_the_previous_request_sent() {
        let first = Some((2, 0));
        for n in 1..=6 {
            let r = plan_request(&worker_loop(n, 1_000), Budget::Worker).unwrap();
            assert_eq!((r.retired, r.frontier), (0, first), "round {n}");
        }
        // Round 7 retires four. Its previous request sent all six, so the
        // frontier stays on the first screenshot: the anchor ahead of it is
        // the entry rounds 1-6 kept refreshing.
        let r = plan_request(&worker_loop(7, 1_000), Budget::Worker).unwrap();
        assert_eq!((r.retired, r.frontier), (4, first));
        // Round 8: the previous request sent screenshots 4.. (message 10).
        for n in 8..=10 {
            let r = plan_request(&worker_loop(n, 1_000), Budget::Worker).unwrap();
            assert_eq!((r.retired, r.frontier), (4, Some((10, 0))), "round {n}");
        }
        // A first request has no previous one: the frontier is its first image.
        let opening = [Arc::new(json!({"role":"user","content":[
            {"type":"text","text":"look"}, shot(10)
        ]}))];
        assert_eq!(
            plan_request(&opening, Budget::Native).unwrap().frontier,
            Some((0, 1))
        );
        // No image left that the previous request sent: no frontier.
        assert_eq!(
            plan_request(&worker_loop(0, 0), Budget::Worker)
                .unwrap()
                .frontier,
            None
        );
    }

    #[test]
    fn round_images_sends_native_history_as_is_and_retains_elsewhere() {
        let native = "claude-sonnet-4-6";
        let routed = "groq/llama-3.3-70b-versatile";
        assert_eq!(
            Budget::for_request(native, CodexRequestRole::Foreground),
            Budget::Native
        );
        assert_eq!(
            Budget::for_request(native, CodexRequestRole::Worker),
            Budget::Worker
        );
        assert_eq!(
            Budget::for_request(routed, CodexRequestRole::Worker),
            Budget::Worker
        );
        assert_eq!(
            Budget::for_request(routed, CodexRequestRole::Foreground),
            Budget::Routed
        );

        let history = worker_loop(7, 1_000);
        let round = round_images(native, CodexRequestRole::Foreground, &history).unwrap();
        assert!(matches!(round, RoundImages::Unchanged));
        let (sent, estimated) = round.views(&history);
        assert!(std::ptr::eq(sent, history.as_slice()) && std::ptr::eq(estimated, sent));

        // A worker on the native route: the retained view is sent and
        // estimated, and its frontier rides along for the transport.
        for model in [native, routed] {
            let round = round_images(model, CodexRequestRole::Worker, &history).unwrap();
            let (sent, estimated) = round.views(&history);
            assert!(std::ptr::eq(sent, estimated));
            assert_eq!(round.applied_frontier(), Some((2, 0)));
            for i in 0..7 {
                let block = &sent[2 + 2 * i]["content"][0]["content"][1];
                if i < 4 {
                    assert_eq!(block, &json!({"type":"text","text":OMITTED}), "{i}");
                } else {
                    assert_eq!(block["type"], "image", "{i}");
                }
            }
            assert!(Arc::ptr_eq(&sent[0], &history[0]));
            assert_eq!(history[2]["content"][0]["content"][1]["type"], "image");
        }

        // Over the native budget: the native transport retires from the
        // original itself, so only the estimate is projected.
        let big = worker_loop(101, 10);
        let round = round_images(native, CodexRequestRole::Foreground, &big).unwrap();
        let (sent, estimated) = round.views(&big);
        assert!(std::ptr::eq(sent, big.as_slice()));
        // 101 crosses 100: one 40-image chunk, screenshots 0..=39 (message
        // 2 + 2i), and the 41st onward is sent.
        assert_eq!(estimated[2]["content"][0]["content"][1]["text"], OMITTED);
        assert_eq!(estimated[80]["content"][0]["content"][1]["text"], OMITTED);
        assert_eq!(estimated[82]["content"][0]["content"][1]["type"], "image");
        assert_eq!(round.applied_frontier(), None);
    }

    /// Canonical bytes of the prefix ending at `(message, block)`, markers
    /// stripped (they are not part of the cache key).
    fn prefix_at(messages: &[SharedMessage], (message, block): (usize, usize)) -> String {
        fn strip(value: &Value) -> Value {
            let mut value = value.clone();
            if let Some(blocks) = value["content"].as_array_mut() {
                for b in blocks {
                    if let Some(obj) = b.as_object_mut() {
                        obj.remove("cache_control");
                    }
                }
            }
            value
        }
        let mut out: Vec<Value> = messages[..message].iter().map(|m| strip(m)).collect();
        let mut last = strip(&messages[message]);
        last["content"].as_array_mut().unwrap().truncate(block + 1);
        out.push(last);
        serde_json::to_string(&out).unwrap()
    }

    fn marked(messages: &[SharedMessage]) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for (m, message) in messages.iter().enumerate() {
            for (b, block) in message["content"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                if block.get("cache_control").is_some() {
                    out.push((m, b));
                }
            }
        }
        out
    }

    /// The cache property the anchor exists for, over a long screenshot loop
    /// through the real pipeline order: stream-loop retention, the native
    /// transport's own replay, the anchor, the tail marker. Every request
    /// either extends the previous tail (no retirement) or, when it retires,
    /// carries the previous request's anchor at the same place over the same
    /// bytes, so it re-reads that entry instead of writing the history again.
    #[test]
    fn every_retiring_round_reads_through_the_previous_anchor() {
        use super::super::helpers::HelperMethods;
        use crate::core::config::CacheTtl;
        let outgoing = |history: &[SharedMessage], role: CodexRequestRole| {
            let round = round_images("claude-sonnet-4-6", role, history).unwrap();
            let (sent, _) = round.views(history);
            let native = plan_request(sent, Budget::Native).unwrap();
            let mut out = sent.to_vec();
            retire(&mut out, native.retired);
            let frontier = earliest(round.applied_frontier(), native.frontier);
            HelperMethods::annotate_retention_anchor(&mut out, frontier, CacheTtl::FiveMinutes);
            HelperMethods::annotate_cache_breakpoint(&mut out, CacheTtl::FiveMinutes);
            (out, frontier)
        };
        /// Markers stripped: the messages up to and including `through`.
        fn messages_through(messages: &[SharedMessage], through: usize) -> String {
            let stripped: Vec<Value> = messages[..=through]
                .iter()
                .map(|m| {
                    let mut m = (**m).clone();
                    for b in m["content"].as_array_mut().into_iter().flatten() {
                        if let Some(obj) = b.as_object_mut() {
                            obj.remove("cache_control");
                        }
                    }
                    m
                })
                .collect();
            serde_json::to_string(&stripped).unwrap()
        }
        let pairs = [2; 30];
        let mixed: Vec<usize> = (0..260).map(|i| 2 - i % 2).collect();
        for (role, layout, bytes) in [
            (CodexRequestRole::Worker, &[1; 40][..], 300_000),
            (CodexRequestRole::Worker, &[1; 25][..], 900_000),
            (CodexRequestRole::Worker, &SA13[..], 300_000),
            (CodexRequestRole::Worker, &pairs[..], 10),
            (CodexRequestRole::Foreground, &[1; 260][..], 10),
            (CodexRequestRole::Foreground, &mixed[..], 10),
        ] {
            /// What one round sent: its retirement, marker positions, body.
            struct Sent {
                retired: usize,
                marks: Vec<(usize, usize)>,
                out: Vec<SharedMessage>,
            }
            let mut previous: Option<Sent> = None;
            let mut retirements = 0;
            let mut mid_message_frontiers = 0;
            for n in 1..=layout.len() {
                let history = screenshot_loop(&layout[..n], bytes);
                let (out, frontier) = outgoing(&history, role);
                let marks = marked(&out);
                assert_eq!(marks.len(), 2, "anchor + tail, round {n}");
                let budget = Budget::for_request("claude-sonnet-4-6", role);
                let retired = plan_request(&history, budget).unwrap().retired;
                if let Some(previous) = &previous {
                    if retired > previous.retired {
                        retirements += 1;
                        assert_eq!(marks[0], previous.marks[0], "{role:?} round {n}");
                        // The anchor closes a whole message ahead of the
                        // first one the retirement rewrote, never a block
                        // inside it.
                        let rewritten = (0..previous.out.len())
                            .find(|&m| {
                                messages_through(&out, m) != messages_through(&previous.out, m)
                            })
                            .expect("a retirement rewrites a message");
                        mid_message_frontiers += usize::from(frontier.unwrap().1 > 0);
                        assert!(
                            marks[0].0 < rewritten,
                            "{role:?} round {n}: anchor {:?} must precede rewritten message {rewritten}",
                            marks[0]
                        );
                        assert_eq!(
                            messages_through(&out, marks[0].0),
                            messages_through(&previous.out, marks[0].0),
                            "{role:?} round {n}: the anchored message must not change"
                        );
                        assert_eq!(
                            prefix_at(&out, marks[0]),
                            prefix_at(&previous.out, previous.marks[0]),
                            "{role:?} round {n}: the anchor prefix must not change"
                        );
                    } else {
                        let tail = previous.marks[1];
                        assert_eq!(
                            prefix_at(&out, tail),
                            prefix_at(&previous.out, tail),
                            "{role:?} round {n}: the previous tail must be a prefix"
                        );
                    }
                }
                previous = Some(Sent {
                    retired,
                    marks,
                    out,
                });
            }
            assert!(retirements >= 4, "{role:?}: {retirements} retirements");
            if layout.contains(&2) {
                assert!(
                    mid_message_frontiers > 0,
                    "{role:?}: the parallel layout must put a frontier past block 0"
                );
            }
        }
    }
}
