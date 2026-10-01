//! Request-local enforcement of Anthropic's whole-request image limits.
//!
//! The Messages API rejects (HTTP 400) a request that carries MORE THAN 20
//! images if any one of them has a side over 2000 px ("many-image requests"),
//! and it caps a request at 100 images. Both limits depend on the whole
//! request, not on any single image. So a history that was valid one round
//! turns invalid the moment one more image arrives. Because the history is
//! re-sent every round, every later request in that session fails the same
//! way. The read tool's per-image guard (8000 px) and attachment preflight
//! cannot see this: each image is fine on its own.
//!
//! [`enforce_image_limits`] runs on the outgoing copy of the history, right
//! after `sanitize_thinking_blocks`, on the Anthropic transports only. The
//! durable session history is never rewritten (same contract as the stream
//! loop's `cap_history_image_bytes`).
//!
//! Prompt-cache behaviour:
//! - A request with 20 or fewer images is returned untouched (no clone, no
//!   decode). Every healthy session is byte-identical to before.
//! - The dimension rule depends only on each image's own bytes, and a session
//!   that has crossed 20 images stays across (history only grows). So the
//!   same blocks are replaced on every round and the cached prefix is stable
//!   after the single miss at the round that crosses the threshold.
//! - The 100-image cap drops the OLDEST images first. That one is a sliding
//!   window (each new image past 100 moves it), which is acceptable that deep
//!   into a session.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::SharedMessage;

/// More images than this in one request turns on the per-image side limit.
pub(crate) const MANY_IMAGE_THRESHOLD: usize = 20;
/// Per-side pixel limit for every image once the threshold is exceeded.
pub(crate) const MANY_IMAGE_MAX_SIDE_PX: u32 = 2000;
/// Hard cap on images in a single request. Conservative: keeping the newest
/// 100 never produces an invalid request, at worst it drops very old images.
pub(crate) const MAX_IMAGES_PER_REQUEST: usize = 100;

/// Label for images removed by the per-request image count cap.
pub(crate) const IMAGE_COUNT_CAP_LABEL: &str = "[image omitted from this request: the conversation holds more than 100 images, so the oldest ones are no longer sent. Re-read the file if you still need it.]";

/// Base64 chars decoded first when reading dimensions. PNG/GIF/WebP headers
/// sit in the first 30 bytes; a JPEG's SOF can sit behind a large EXIF
/// segment, so a miss falls back to decoding the whole payload. Multiple of
/// 4, so the prefix is a complete run of base64 quads.
const DIMENSION_PREFIX_CHARS: usize = 64 * 1024;

/// Where an image block sits: `content[block]` of message `msg`, or
/// `content[block].content[inner]` for media nested in a `tool_result`.
#[derive(Clone, Copy, Debug)]
struct Slot {
    msg: usize,
    block: usize,
    inner: Option<usize>,
}

fn is_image(block: &Value) -> bool {
    block["type"] == "image"
}

/// Every image block in send order, oldest first. Mirrors the shapes the
/// attachment preflight admits: top-level `content[]` and one level of
/// `tool_result.content[]`.
fn image_slots(messages: &[SharedMessage]) -> Vec<Slot> {
    let mut slots = Vec::new();
    for (msg, message) in messages.iter().enumerate() {
        let Some(blocks) = message["content"].as_array() else {
            continue;
        };
        for (block, b) in blocks.iter().enumerate() {
            if is_image(b) {
                slots.push(Slot {
                    msg,
                    block,
                    inner: None,
                });
            } else if let Some(nested) = b["content"].as_array() {
                for (inner, n) in nested.iter().enumerate() {
                    if is_image(n) {
                        slots.push(Slot {
                            msg,
                            block,
                            inner: Some(inner),
                        });
                    }
                }
            }
        }
    }
    slots
}

fn block_at(messages: &[SharedMessage], slot: Slot) -> &Value {
    let outer = &messages[slot.msg]["content"][slot.block];
    match slot.inner {
        Some(inner) => &outer["content"][inner],
        None => outer,
    }
}

/// Swap one image block for a text block. Clones only the owning message
/// (`Arc::make_mut`); a `tool_result`'s leading text summary is never an
/// image, so the text-first invariant of rich tool output holds.
fn replace_with_text(messages: &mut [SharedMessage], slot: Slot, text: String) {
    let message = Arc::make_mut(&mut messages[slot.msg]);
    let outer = &mut message["content"][slot.block];
    let target = match slot.inner {
        Some(inner) => &mut outer["content"][inner],
        None => outer,
    };
    *target = json!({ "type": "text", "text": text });
}

/// Pixel size of a base64 image, read from its header. `None` when the
/// source is not base64 or the header cannot be parsed.
fn image_size(block: &Value) -> Option<(u32, u32)> {
    let source = &block["source"];
    if source["type"] != "base64" {
        return None;
    }
    let data = source["data"].as_str()?;
    let mime = source["media_type"].as_str()?;
    let size_of = |encoded: &str| {
        STANDARD
            .decode(encoded)
            .ok()
            .and_then(|bytes| crate::tools::read::image_dimensions(mime, &bytes))
    };
    // `get` (not indexing): a non-ASCII payload must not panic on a char
    // boundary; it is corrupt anyway and simply falls through to `None`.
    let prefix = data.get(..DIMENSION_PREFIX_CHARS.min(data.len()))?;
    size_of(prefix).or_else(|| {
        if prefix.len() < data.len() {
            size_of(data)
        } else {
            None
        }
    })
}

fn oversized_label(size: Option<(u32, u32)>) -> String {
    let what = match size {
        Some((w, h)) => format!("{w}x{h} px"),
        None => "size could not be read".to_string(),
    };
    format!(
        "[image omitted from this request ({what}): the conversation holds more than \
         {MANY_IMAGE_THRESHOLD} images, and then Anthropic accepts only images up to \
         {MANY_IMAGE_MAX_SIDE_PX} px per side. If you still need it, downscale a copy \
         (e.g. `convert IN -resize 1568x1568\\> OUT`) and read that.]"
    )
}

/// Bring the outgoing history within Anthropic's whole-request image limits.
/// Returns the number of image blocks replaced (0 = nothing touched, no
/// allocation, every `Arc` unchanged).
pub(super) fn enforce_image_limits(messages: &mut [SharedMessage]) -> usize {
    let slots = image_slots(messages);
    if slots.len() <= MANY_IMAGE_THRESHOLD {
        return 0;
    }

    // 1. Count cap: drop the oldest images beyond the newest 100.
    let over_cap = slots.len().saturating_sub(MAX_IMAGES_PER_REQUEST);
    for &slot in &slots[..over_cap] {
        replace_with_text(messages, slot, IMAGE_COUNT_CAP_LABEL.to_string());
    }

    // 2. Side limit: what remains is still more than 20 images (min(n, 100)),
    //    so every image must be at most 2000 px per side. An image whose size
    //    cannot be read is treated as oversized: sending it would risk the
    //    same 400 on every later round.
    let mut oversized = 0usize;
    for &slot in &slots[over_cap..] {
        let size = image_size(block_at(messages, slot));
        let fits = size.is_some_and(|(w, h)| w.max(h) <= MANY_IMAGE_MAX_SIDE_PX);
        if !fits {
            replace_with_text(messages, slot, oversized_label(size));
            oversized += 1;
        }
    }

    let replaced = over_cap + oversized;
    if replaced > 0 {
        tracing::info!(
            images = slots.len(),
            over_count_cap = over_cap,
            oversized,
            "request image limits: image blocks replaced with text labels in the outgoing copy"
        );
    }
    replaced
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal PNG header: signature + IHDR length/type + width + height.
    fn png(w: u32, h: u32) -> String {
        let mut b = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        b.extend_from_slice(&13u32.to_be_bytes());
        b.extend_from_slice(b"IHDR");
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&h.to_be_bytes());
        b.extend_from_slice(&[8, 6, 0, 0, 0]);
        STANDARD.encode(b)
    }

    /// JPEG whose SOF0 sits behind an APP1 segment larger than the decode
    /// prefix, so the size is only readable from the full payload.
    fn jpeg_with_big_exif(w: u16, h: u16) -> String {
        let mut b = vec![0xFF, 0xD8];
        let app1_len: u16 = 60_000; // > 48 KiB decoded prefix
        b.extend_from_slice(&[0xFF, 0xE1]);
        b.extend_from_slice(&app1_len.to_be_bytes());
        b.resize(b.len() + app1_len as usize - 2, 0);
        b.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        b.extend_from_slice(&h.to_be_bytes());
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&[0x03; 12]);
        b.extend_from_slice(&[0xFF, 0xD9]);
        STANDARD.encode(b)
    }

    fn image(mime: &str, data: String) -> Value {
        json!({"type": "image", "source": {"type": "base64", "media_type": mime, "data": data}})
    }

    fn png_image(w: u32, h: u32) -> Value {
        image("image/png", png(w, h))
    }

    /// A read-tool style result: text summary first, then the image.
    fn tool_result(id: &str, img: Value) -> Value {
        json!({"type": "tool_result", "tool_use_id": id,
               "content": [{"type": "text", "text": "Image: shot.png"}, img]})
    }

    fn user(content: Vec<Value>) -> SharedMessage {
        Arc::new(json!({"role": "user", "content": content}))
    }

    fn assistant() -> SharedMessage {
        Arc::new(json!({"role": "assistant", "content": [{"type": "text", "text": "ok"}]}))
    }

    /// `n` user messages, each one tool_result image, alternating with
    /// assistant turns. `size(i)` gives image i's dimensions.
    fn history(n: usize, size: impl Fn(usize) -> (u32, u32)) -> Vec<SharedMessage> {
        let mut out = Vec::new();
        for i in 0..n {
            let (w, h) = size(i);
            out.push(user(vec![tool_result(&format!("t{i}"), png_image(w, h))]));
            out.push(assistant());
        }
        out
    }

    fn remaining_images(messages: &[SharedMessage]) -> usize {
        image_slots(messages).len()
    }

    #[test]
    fn twenty_images_untouched_even_when_oversized() {
        let original = history(20, |_| (2400, 1771));
        let mut out = original.clone();
        assert_eq!(enforce_image_limits(&mut out), 0);
        for (a, b) in original.iter().zip(&out) {
            assert!(Arc::ptr_eq(a, b), "healthy path must not clone");
        }
    }

    #[test]
    fn twenty_first_image_drops_only_oversized_ones() {
        // The S344 session shape: 21 images, a handful of 2400 px screenshots.
        let big = [5usize, 9, 12];
        let original = history(21, |i| {
            if big.contains(&i) {
                (2400, 1771)
            } else {
                (1200, 760)
            }
        });
        let mut out = original.clone();
        assert_eq!(enforce_image_limits(&mut out), big.len());
        assert_eq!(remaining_images(&out), 21 - big.len());

        for (idx, (a, b)) in original.iter().zip(&out).enumerate() {
            let image_no = idx / 2;
            let touched = idx % 2 == 0 && big.contains(&image_no);
            assert_eq!(!Arc::ptr_eq(a, b), touched, "message {idx}");
        }
        let replaced = &out[10]["content"][0]["content"];
        assert_eq!(replaced[0]["type"], "text", "text summary stays first");
        assert_eq!(replaced[1]["type"], "text");
        let label = replaced[1]["text"].as_str().unwrap();
        assert!(label.contains("2400x1771 px"), "{label}");
        assert!(label.contains("2000 px"), "{label}");
    }

    /// The image sizes of the real session that hit the 400 (in order). The
    /// 21st image pushed it past 20 while five 2400 px screenshots were in
    /// history, so every later request failed until /compact.
    #[test]
    fn real_session_shape_is_brought_within_limits() {
        const SIZES: [(u32, u32); 21] = [
            (1200, 760),
            (1200, 760),
            (1200, 760),
            (1200, 760),
            (1200, 760),
            (1200, 760),
            (1200, 760),
            (1200, 760),
            (1200, 760),
            (1600, 1518),
            (1200, 760),
            (2000, 1518),
            (1200, 760),
            (1200, 760),
            (2400, 1771),
            (2400, 1771),
            (2400, 1210),
            (2400, 927),
            (2400, 990),
            (1200, 760),
            (1200, 760),
        ];
        let mut out = history(SIZES.len(), |i| SIZES[i]);
        assert_eq!(enforce_image_limits(&mut out), 5);
        let kept = image_slots(&out);
        assert_eq!(kept.len(), 16);
        for slot in kept {
            let (w, h) = image_size(block_at(&out, slot)).expect("readable");
            assert!(w.max(h) <= MANY_IMAGE_MAX_SIDE_PX, "{w}x{h} still sent");
        }
        // One image fewer (the last one was never read) needs no change.
        let mut before = history(SIZES.len() - 1, |i| SIZES[i]);
        assert_eq!(enforce_image_limits(&mut before), 0);
    }

    #[test]
    fn exactly_2000_px_is_kept() {
        let mut out = history(25, |_| (2000, 2000));
        assert_eq!(enforce_image_limits(&mut out), 0);
        assert_eq!(remaining_images(&out), 25);
    }

    #[test]
    fn top_level_user_images_are_covered() {
        let mut out = history(20, |_| (800, 600));
        out.push(user(vec![
            json!({"type": "text", "text": "look"}),
            png_image(3000, 100),
        ]));
        assert_eq!(enforce_image_limits(&mut out), 1);
        let last = out.last().unwrap();
        assert_eq!(last["content"][1]["type"], "text");
        assert!(last["content"][1]["text"]
            .as_str()
            .unwrap()
            .contains("3000x100 px"));
    }

    #[test]
    fn unreadable_size_is_treated_as_oversized_past_threshold() {
        let mut out = history(20, |_| (800, 600));
        out.push(user(vec![image(
            "image/png",
            STANDARD.encode(b"not a png"),
        )]));
        assert_eq!(enforce_image_limits(&mut out), 1);
        let text = out.last().unwrap()["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("size could not be read"), "{text}");
    }

    #[test]
    fn jpeg_size_found_past_decode_prefix() {
        let small = image("image/jpeg", jpeg_with_big_exif(1600, 900));
        let large = image("image/jpeg", jpeg_with_big_exif(2400, 900));
        assert!(small["source"]["data"].as_str().unwrap().len() > DIMENSION_PREFIX_CHARS);
        assert_eq!(image_size(&small), Some((1600, 900)));
        assert_eq!(image_size(&large), Some((2400, 900)));

        let mut out = history(19, |_| (800, 600));
        out.push(user(vec![small]));
        out.push(assistant());
        out.push(user(vec![large]));
        assert_eq!(enforce_image_limits(&mut out), 1);
        assert_eq!(remaining_images(&out), 20);
    }

    #[test]
    fn count_cap_drops_oldest_beyond_100() {
        let mut out = history(105, |_| (800, 600));
        assert_eq!(enforce_image_limits(&mut out), 5);
        assert_eq!(remaining_images(&out), MAX_IMAGES_PER_REQUEST);
        for i in 0..5 {
            let block = &out[i * 2]["content"][0]["content"][1];
            assert_eq!(block["text"], IMAGE_COUNT_CAP_LABEL, "image {i}");
        }
        assert_eq!(out[10]["content"][0]["content"][1]["type"], "image");
    }

    #[test]
    fn output_is_stable_round_to_round() {
        // Same history in, same bytes out: the cached prefix survives.
        let base = history(30, |i| if i % 4 == 0 { (2400, 900) } else { (900, 600) });
        let mut first = base.clone();
        let mut second = base.clone();
        enforce_image_limits(&mut first);
        enforce_image_limits(&mut second);
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );
        // And a later round (one more turn appended) keeps the earlier prefix.
        let mut later = base.clone();
        later.push(user(vec![tool_result("t30", png_image(900, 600))]));
        enforce_image_limits(&mut later);
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&later[..first.len()]).unwrap()
        );
    }

    #[test]
    fn non_image_content_is_ignored() {
        let mut out = vec![
            Arc::new(json!({"role": "user", "content": "plain string content"})),
            assistant(),
        ];
        out.extend(history(25, |_| (800, 600)));
        assert_eq!(enforce_image_limits(&mut out), 0);
    }
}
