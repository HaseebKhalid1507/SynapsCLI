//! Anthropic-only outgoing image projection. The side rule is independent of
//! image count. Original-batch count/byte retirement lives in image_retention.
use super::attachments::MAX_IMAGE_BYTES;
use crate::SharedMessage;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use image::{io::Reader, GenericImageView, ImageEncoder, ImageFormat};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    io::{Cursor, Write},
    sync::{Arc, Mutex, OnceLock},
};

const MAX_SIDE: u32 = 2000;
const MAX_PIXELS: u64 = 16_000_000;
// image 0.24 partial library guard, NOT a total JPEG decoder memory ceiling:
// limits are not propagated to JPEG internals (e.g. progressive buffers).
// The 16 MP cap and one worker bound workload, not decoder memory to 128 MiB.
const MAX_ALLOC: u64 = 128 * 1024 * 1024;
const CACHE_BYTES: usize = 16 * 1024 * 1024;
const CACHE_COUNT: usize = 16;
const INVALID: &str = "Image projection failed: corrupt or unsupported image; export a static PNG/JPEG at most 2000 pixels per side and reattach it";
const RESOURCE: &str = "Image projection exceeds resource limits; export a static PNG/JPEG at most 2000 pixels per side and reattach it";

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

#[derive(Clone, Debug, PartialEq)]
struct Projection {
    data: String,
    mime: &'static str,
    note: String,
}

#[derive(Default)]
struct Cache {
    entries: VecDeque<([u8; 32], Arc<Projection>)>,
    bytes: usize,
}
impl Cache {
    fn get(&self, key: &[u8; 32]) -> Option<Arc<Projection>> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }
    fn insert(&mut self, key: [u8; 32], value: Arc<Projection>) {
        let size = value.data.len() + value.note.len();
        if size > CACHE_BYTES {
            return;
        }
        while self.entries.len() >= CACHE_COUNT || self.bytes + size > CACHE_BYTES {
            if let Some((_, old)) = self.entries.pop_front() {
                self.bytes -= old.data.len() + old.note.len();
            }
        }
        self.bytes += size;
        self.entries.push_back((key, value));
    }
}

/// A Write-only encoder sink: fail before extending past the raw image budget.
struct BoundedOutput {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for BoundedOutput {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other(RESOURCE));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn encode(image: &image::DynamicImage, jpeg: bool, limit: usize) -> Result<Vec<u8>, String> {
    let mut out = BoundedOutput {
        bytes: Vec::new(),
        limit,
    };
    if jpeg {
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85)
            .encode_image(image)
            .map_err(|_| RESOURCE)?;
    } else {
        let rgba = image.to_rgba8();
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(
                rgba.as_raw(),
                rgba.width(),
                rgba.height(),
                image::ColorType::Rgba8,
            )
            .map_err(|_| RESOURCE)?;
    }
    Ok(out.bytes)
}

// Walk PNG chunks (not arbitrary compressed bytes) to reject APNG when resizing.
fn animated_png(bytes: &[u8]) -> bool {
    let mut pos = 8usize;
    while let Some(header) = bytes.get(pos..pos.saturating_add(8)) {
        if &header[4..8] == b"acTL" {
            return true;
        }
        let len = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        let Some(next) = pos.checked_add(12).and_then(|n| n.checked_add(len)) else {
            break;
        };
        pos = next;
    }
    false
}

fn project(block: &Value, cache: &Mutex<Cache>) -> Result<Option<Arc<Projection>>, String> {
    let source = &block["source"];
    let data = source["data"].as_str().ok_or(INVALID)?;
    let mime = source["media_type"].as_str().ok_or(INVALID)?;
    if source["type"] != "base64" {
        return Err(INVALID.into());
    }
    if data.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
        return Err(RESOURCE.into());
    }
    let mut hash = Sha256::new();
    hash.update(b"synaps-anthropic-image-v1:2000:triangle:jpeg85:rgba-png:raster-orientation\0");
    hash.update(mime.as_bytes());
    hash.update([0]);
    hash.update(data.as_bytes());
    let key: [u8; 32] = hash.finalize().into();
    if let Some(hit) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return Ok(Some(hit));
    }
    let bytes = STANDARD.decode(data).map_err(|_| INVALID)?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(RESOURCE.into());
    }
    let format = match mime {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::WebP,
        _ => return Err(INVALID.into()),
    };
    // Cheap bounded header inspection BEFORE constructing any decoder.
    let (w, h) = crate::tools::read::image_dimensions(mime, &bytes).ok_or(INVALID)?;
    if w == 0 || h == 0 || w.max(h) > 8000 {
        return Err(RESOURCE.into());
    }
    // Preserve preexisting structural validation for small originals; do not
    // decode their pixels, even if corrupt data may later be rejected upstream.
    if w.max(h) <= MAX_SIDE {
        return Ok(None);
    }
    if u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(RESOURCE.into());
    }
    // Conservative for GIF/WebP: require a static PNG/JPEG export rather than
    // silently flattening animation. Small originals remain byte-identical.
    if matches!(format, ImageFormat::Gif | ImageFormat::WebP)
        || (format == ImageFormat::Png && animated_png(&bytes))
    {
        return Err("Image projection requires a static PNG/JPEG export for oversized GIF/WebP or animated PNG; reattach an export at most 2000 pixels per side".into());
    }
    let mut reader = Reader::with_format(Cursor::new(&bytes), format);
    let mut limits = image::io::Limits::default();
    limits.max_image_width = Some(8000);
    limits.max_image_height = Some(8000);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|_| INVALID)?;
    if decoded.dimensions() != (w, h) {
        return Err(INVALID.into());
    }
    let longest = u64::from(w.max(h));
    let sent_w = (u64::from(w) * u64::from(MAX_SIDE) / longest).max(1) as u32;
    let sent_h = (u64::from(h) * u64::from(MAX_SIDE) / longest).max(1) as u32;
    let resized = decoded.resize_exact(sent_w, sent_h, image::imageops::FilterType::Triangle);
    let jpeg = format == ImageFormat::Jpeg;
    let encoded = encode(&resized, jpeg, MAX_IMAGE_BYTES)?;
    let value = Arc::new(Projection {
        data: STANDARD.encode(encoded),
        mime: if jpeg { "image/jpeg" } else { "image/png" },
        note: format!("[Image resized for this request: original {w}x{h} pixels; sent {sent_w}x{sent_h} pixels. Coordinates refer to sent pixels. Encoded raster orientation is used; EXIF orientation is not applied.]"),
    });
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, value.clone());
    Ok(Some(value))
}

#[cfg(test)]
fn enforce(messages: &mut [SharedMessage], cache: &Mutex<Cache>) -> Result<usize, String> {
    enforce_with_retirement(messages, 0, cache)
}

fn enforce_with_retirement(
    messages: &mut [SharedMessage],
    retired_count: usize,
    cache: &Mutex<Cache>,
) -> Result<usize, String> {
    let slots = image_slots(messages);
    // Reverse order keeps original slot indices valid when adding adjacent text.
    for (occurrence, &slot) in slots.iter().enumerate().rev() {
        let projected = project(block_at(messages, slot), cache)?;
        // Always project/check the original, including retired occurrences.
        // Discard retired output rather than letting later images hide errors.
        if occurrence < retired_count || projected.is_some() {
            let message = Arc::make_mut(&mut messages[slot.msg]);
            let (blocks, index) = match slot.inner {
                Some(inner) => (
                    message["content"][slot.block]["content"]
                        .as_array_mut()
                        .unwrap(),
                    inner,
                ),
                None => (message["content"].as_array_mut().unwrap(), slot.block),
            };
            if occurrence < retired_count {
                blocks[index] = json!({"type": "text", "text": super::image_retention::OMITTED});
                continue;
            }
            let projected = projected.unwrap();
            blocks[index]["source"]["data"] = Value::String(projected.data.clone());
            blocks[index]["source"]["media_type"] = Value::String(projected.mime.into());
            blocks.insert(index + 1, json!({"type": "text", "text": projected.note}));
        }
    }
    Ok(retired_count)
}

/// Acquire before spawn, then MOVE the permit inside blocking work. Cancellation
/// of the async waiter cannot admit another decoder while this one still runs.
/// Only projected bytes/metadata survive in the process-wide bounded memo cache.
pub(super) async fn enforce_image_limits(
    messages: &mut [SharedMessage],
    retired_count: usize,
) -> Result<usize, String> {
    static GATE: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    if image_slots(messages).is_empty() {
        return Ok(0);
    }
    let permit = GATE
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1)))
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| RESOURCE)?;
    let mut outgoing = messages.to_vec();
    let (outgoing, omitted) = blocking_with_permit(permit, move || {
        let omitted = enforce_with_retirement(
            &mut outgoing,
            retired_count,
            CACHE.get_or_init(|| Mutex::new(Cache::default())),
        )?;
        Ok::<_, String>((outgoing, omitted))
    })
    .await?;
    messages.clone_from_slice(&outgoing);
    Ok(omitted)
}

async fn blocking_with_permit<T: Send + 'static>(
    permit: tokio::sync::OwnedSemaphorePermit,
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .map_err(|_| "Image projection worker failed; export a smaller static image".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(w: u32, h: u32, jpeg: bool) -> Value {
        let raster = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            w,
            h,
            image::Rgba([60, 120, 180, 128]),
        ));
        json!({"type": "image", "source": {"type": "base64",
            "media_type": if jpeg { "image/jpeg" } else { "image/png" },
            "data": STANDARD.encode(encode(&raster, jpeg, MAX_IMAGE_BYTES).unwrap())}})
    }
    fn history(n: usize, img: &Value) -> Vec<SharedMessage> {
        (0..n)
            .map(|i| {
                Arc::new(json!({"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": format!("t{i}"), "is_error": false,
                     "content": [{"type": "text", "text": "original summary"}, img]}
                ]}))
            })
            .collect()
    }
    fn cache() -> Mutex<Cache> {
        Mutex::new(Cache::default())
    }

    #[tokio::test]
    async fn prefix_is_identical_at_1_20_21_22_and_original_arcs_survive() {
        let img = fixture(2001, 4, false);
        let original = history(22, &img);
        let saved = original.clone();
        let mut previous: Vec<SharedMessage> = Vec::new();
        for n in [1, 20, 21, 22] {
            let mut outgoing = original[..n].to_vec();
            assert_eq!(enforce_image_limits(&mut outgoing, 0).await.unwrap(), 0);
            assert_eq!(&outgoing[..previous.len()], previous.as_slice());
            assert_eq!(outgoing[0]["content"][0]["tool_use_id"], "t0");
            assert_eq!(outgoing[0]["content"][0]["is_error"], false);
            assert_eq!(
                outgoing[0]["content"][0]["content"][0]["text"],
                "original summary"
            );
            for (a, b) in original.iter().zip(&saved) {
                assert!(Arc::ptr_eq(a, b));
            }
            assert_eq!(original[0]["content"][0]["content"][1], img);
            previous = outgoing;
        }
    }

    #[test]
    fn boundary_top_level_nested_and_alpha() {
        let c = cache();
        let small = fixture(2000, 2, false);
        let large = fixture(2001, 2, false);
        let mut messages = vec![Arc::new(json!({"role": "user", "content": [small, large,
            {"type": "tool_result", "tool_use_id": "nested", "content": [large]}]}))];
        let original = messages.clone();
        enforce(&mut messages, &c).unwrap();
        assert_eq!(messages[0]["content"][0], original[0]["content"][0]);
        assert_eq!(messages[0]["content"][2]["type"], "text");
        assert_eq!(messages[0]["content"][3]["tool_use_id"], "nested");
        assert_eq!(
            messages[0]["content"][1],
            messages[0]["content"][3]["content"][0]
        );
        let bytes = STANDARD
            .decode(
                messages[0]["content"][1]["source"]["data"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
        let raster = image::load_from_memory(&bytes).unwrap();
        assert_eq!(raster.dimensions(), (2000, 1));
        assert_eq!(raster.to_rgba8().get_pixel(0, 0)[3], 128);
        let again = messages.clone();
        enforce(&mut messages, &c).unwrap();
        assert_eq!(messages, again); // no duplicate explanatory text
        let mut unchanged = history(1, &small);
        let saved = unchanged.clone();
        enforce(&mut unchanged, &c).unwrap();
        assert!(Arc::ptr_eq(&saved[0], &unchanged[0]));
    }

    #[test]
    fn jpeg_real_exif_6_8_preserves_encoded_raster() {
        let mut img = fixture(2001, 3, true);
        let baseline = project(&img, &cache()).unwrap().unwrap();
        let bytes = STANDARD
            .decode(img["source"]["data"].as_str().unwrap())
            .unwrap();
        for orientation in [6u8, 8] {
            // Valid Exif/TIFF little-endian IFD0: Orientation SHORT, count 1.
            let mut exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
            exif.extend_from_slice(&[orientation, 0, 0, 0, 0, 0, 0, 0]);
            // Also exercise header scanning beyond the old short prefix.
            exif.resize(59_998, 0);
            let mut with_exif = bytes[..2].to_vec();
            with_exif.extend_from_slice(&[0xff, 0xe1]);
            with_exif.extend_from_slice(&60_000u16.to_be_bytes());
            with_exif.extend_from_slice(&exif);
            with_exif.extend_from_slice(&bytes[2..]);
            img["source"]["data"] = STANDARD.encode(with_exif).into();
            let projected = project(&img, &cache()).unwrap().unwrap();
            // Both tags would swap axes if honored. Output must instead be
            // byte-identical to the untagged raster, with metadata stripped.
            assert_eq!(projected, baseline);
            let decoded =
                image::load_from_memory(&STANDARD.decode(&projected.data).unwrap()).unwrap();
            assert_eq!(decoded.dimensions(), (2000, 2));
            assert!(projected.note.contains("EXIF orientation is not applied"));
        }
    }

    #[test]
    fn mixed_multiple_images_and_nested_results_keep_indices() {
        let small = fixture(2, 2, false);
        let wide = fixture(2001, 3, false);
        let tall = fixture(3, 2001, false);
        let text = json!({"type": "text", "text": "keep"});
        let mut messages = vec![Arc::new(json!({"role": "user", "content": [
            wide, text,
            {"type": "tool_result", "tool_use_id": "a", "content": [text, tall, small, wide, text]},
            tall,
            {"type": "tool_result", "tool_use_id": "b", "is_error": false, "content": [wide, text, tall]},
            small, wide
        ]}))];
        let originals = messages.clone();
        let c = cache();
        let projected_block = |img: &Value| {
            let p = project(img, &c).unwrap().unwrap();
            let mut block = img.clone();
            block["source"]["data"] = p.data.clone().into();
            (block, json!({"type": "text", "text": p.note}))
        };
        let (w, wn) = projected_block(&wide);
        let (t, tn) = projected_block(&tall);
        enforce(&mut messages, &c).unwrap();
        assert_eq!(
            *messages[0],
            json!({"role": "user", "content": [
                w, wn, text,
                {"type": "tool_result", "tool_use_id": "a", "content": [text, t, tn, small, w, wn, text]},
                t, tn,
                {"type": "tool_result", "tool_use_id": "b", "is_error": false, "content": [w, wn, text, t, tn]},
                small, w, wn
            ]})
        );
        assert_eq!(originals[0]["content"][0], wide);
    }

    #[test]
    fn cache_hit_eviction_and_byte_count_bounds_are_deterministic() {
        let c = cache();
        let img = fixture(2001, 2, false);
        let first = project(&img, &c).unwrap().unwrap();
        let hit = project(&img, &c).unwrap().unwrap();
        assert!(Arc::ptr_eq(&first, &hit));
        for i in 0..CACHE_COUNT + 1 {
            c.lock().unwrap().insert(
                [i as u8; 32],
                Arc::new(Projection {
                    data: "x".repeat(CACHE_BYTES / 4),
                    mime: "image/png",
                    note: "note".into(),
                }),
            );
        }
        assert!(c.lock().unwrap().entries.len() <= CACHE_COUNT);
        assert!(c.lock().unwrap().bytes <= CACHE_BYTES);
        let miss = project(&img, &c).unwrap().unwrap();
        assert!(!Arc::ptr_eq(&first, &miss));
        assert_eq!(first, miss);
    }

    #[test]
    fn resizing_does_not_independently_evict() {
        let img = fixture(2, 2, false);
        let mut out = history(101, &img);
        assert_eq!(enforce(&mut out, &cache()).unwrap(), 0);
        assert_eq!(out[0]["content"][0]["content"][1], img);
    }

    #[test]
    fn malformed_resource_animation_and_encoder_limits() {
        let mut img = fixture(2001, 2, false);
        img["source"]["data"] = "!!!!".into();
        assert!(project(&img, &cache()).is_err());
        img["source"]["data"] = "A".repeat(MAX_IMAGE_BYTES.div_ceil(3) * 4 + 1).into();
        assert_eq!(project(&img, &cache()).unwrap_err(), RESOURCE);
        // Malformed oversized headers must error, never count-dependent omission.
        let valid = fixture(2001, 2, false);
        let mut bytes = STANDARD
            .decode(valid["source"]["data"].as_str().unwrap())
            .unwrap();
        bytes.truncate(33);
        img["source"]["data"] = STANDARD.encode(&bytes).into();
        for n in [1, 21, 101] {
            assert!(enforce(&mut history(n, &img), &cache()).is_err());
        }
        bytes[16..20].copy_from_slice(&8000u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&8000u32.to_be_bytes());
        img["source"]["data"] = STANDARD.encode(&bytes).into();
        assert_eq!(project(&img, &cache()).unwrap_err(), RESOURCE);
        let gif = json!({"type": "image", "source": {"type": "base64", "media_type": "image/gif",
            "data": STANDARD.encode(b"GIF89a\xd1\x07\x02\x00\x00\x00\x00")}});
        assert!(project(&gif, &cache())
            .unwrap_err()
            .contains("static PNG/JPEG"));
        assert!(encode(&image::DynamicImage::new_rgba8(2, 2), false, 1).is_err());
        assert!(encode(&image::DynamicImage::new_rgb8(2, 2), true, 1).is_err());
    }

    #[tokio::test]
    async fn retired_originals_still_fail_projection_at_1_and_101() {
        let mut png = fixture(2001, 2, false);
        let mut bytes = STANDARD
            .decode(png["source"]["data"].as_str().unwrap())
            .unwrap();
        bytes.truncate(33);
        bytes.extend_from_slice(b"\x00\x00\x00\x00IEND\xAE\x42\x60\x82");
        png["source"]["data"] = STANDARD.encode(bytes).into();
        let gif = json!({"type":"image", "source":{"type":"base64", "media_type":"image/gif",
            "data":STANDARD.encode(b"GIF89a\xd1\x07\x02\x00\x00\x00\x00\x3b")}});
        let small = fixture(2, 2, false);
        for bad in [gif, png] {
            for n in [1, 101] {
                let mut original = history(1, &bad);
                original.extend(history(n - 1, &small));
                super::super::attachments::validate_history_for_request(
                    "anthropic/claude-sonnet-4-6",
                    &original,
                )
                .unwrap();
                let retired = super::super::image_retention::plan(&original).unwrap();
                assert_eq!(retired, if n == 101 { 40 } else { 0 });
                let mut outgoing = original.clone();
                assert!(enforce_image_limits(&mut outgoing, retired).await.is_err());
                assert_eq!(outgoing, original); // failed worker never publishes partial output
                assert_eq!(original[0]["content"][0]["content"][1], bad);
            }
        }
    }

    #[tokio::test]
    async fn integrated_retirement_discards_projection_but_preserves_originals() {
        let large = fixture(2001, 2, false);
        let original = history(101, &large);
        let mut outgoing = original.clone();
        let retired = super::super::image_retention::plan(&original).unwrap();
        assert_eq!(
            enforce_image_limits(&mut outgoing, retired).await.unwrap(),
            40
        );
        for message in &outgoing[..40] {
            let content = message["content"][0]["content"].as_array().unwrap();
            assert_eq!(content.len(), 2); // no resize note for retired images
            assert_eq!(content[1]["text"], super::super::image_retention::OMITTED);
        }
        assert_eq!(outgoing[40]["content"][0]["content"][1]["type"], "image");
        assert_eq!(outgoing[40]["content"][0]["content"][2]["type"], "text");
        assert_eq!(original[0]["content"][0]["content"][1], large);
    }

    #[test]
    fn small_corrupt_pixels_retain_structural_only_validation() {
        let mut img = fixture(2, 2, false);
        let mut bytes = STANDARD
            .decode(img["source"]["data"].as_str().unwrap())
            .unwrap();
        // Keep structural header/trailer checks valid, but omit all IDAT pixels.
        bytes.truncate(33);
        bytes.extend_from_slice(b"\x00\x00\x00\x00IEND\xAE\x42\x60\x82");
        assert!(image::load_from_memory(&bytes).is_err());
        img["source"]["data"] = STANDARD.encode(bytes).into();
        let mut messages = history(1, &img);
        let original = messages.clone();
        super::super::attachments::validate_messages("anthropic/claude-sonnet-4-6", &messages)
            .unwrap();
        enforce(&mut messages, &cache()).unwrap();
        assert!(Arc::ptr_eq(&messages[0], &original[0]));
    }

    #[test]
    fn both_transports_replay_and_validate_before_merging_and_cache_marker() {
        for source in [include_str!("api.rs"), include_str!("api_sync.rs")] {
            let first = source
                .find("attachments::validate_history_for_request(model, messages)")
                .unwrap();
            let replay = source.find("image_retention::plan(messages)").unwrap();
            let sanitize = source
                .find("HelperMethods::sanitize_thinking_blocks(&mut cleaned_messages)")
                .unwrap();
            let wire = source
                .find("HelperMethods::wire_tool_references(&mut cleaned_messages")
                .unwrap();
            let project = source
                .find("image_limits::enforce_image_limits(&mut cleaned_messages, retired_count)")
                .unwrap();
            let check = source
                .find("attachments::validate_messages(model, &cleaned_messages)")
                .unwrap();
            let marker = source
                .find("HelperMethods::annotate_cache_breakpoint(&mut cleaned_messages")
                .unwrap();
            assert!(first < replay && replay < project && project < check);
            assert!(check < sanitize && sanitize < wire && wire < marker);
        }
    }
    #[tokio::test]
    async fn cancelled_waiter_keeps_blocking_permit_until_work_finishes() {
        let gate = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = gate.clone().acquire_owned().await.unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let waiter = tokio::spawn(blocking_with_permit(permit, move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        }));
        started_rx.await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert!(gate.clone().try_acquire_owned().is_err());
        release_tx.send(()).unwrap();
        let _permit = tokio::time::timeout(std::time::Duration::from_secs(5), gate.acquire_owned())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn streaming_token_cancel_drops_worker_waiter_but_keeps_permit() {
        let gate = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = gate.clone().acquire_owned().await.unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let token = cancel.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let waiter = tokio::spawn(async move {
            super::super::api::await_or_cancel(
                &token,
                blocking_with_permit(permit, move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                }),
            )
            .await
        });
        started_rx.await.unwrap();
        cancel.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), waiter).await;
        let held = gate.clone().try_acquire_owned().is_err();
        // Release even if an assertion fails, so the blocking pool can shut down.
        release_tx.send(()).unwrap();
        assert!(matches!(
            result.unwrap().unwrap(),
            Err(crate::RuntimeError::Canceled)
        ));
        assert!(held);
        let _permit = tokio::time::timeout(std::time::Duration::from_secs(5), gate.acquire_owned())
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn projected_output_passes_original_attachment_preflight() {
        let mut messages = history(1, &fixture(2001, 4, false));
        super::super::attachments::validate_messages("anthropic/claude-sonnet-4-6", &messages)
            .unwrap();
        enforce(&mut messages, &cache()).unwrap();
        super::super::attachments::validate_messages("anthropic/claude-sonnet-4-6", &messages)
            .unwrap();
    }
}

/// Worker uploads must leave uplink capacity for foreground sessions. Count
/// base64 bytes (the actual wire payload), not decoded bytes. URL images count
/// toward the count limit but carry no embedded image payload.
pub(crate) const WORKER_IMAGE_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const WORKER_IMAGE_COUNT: usize = 6;
const WORKER_IMAGE_LABEL: &str = "[image omitted from this worker request: newest 6 images / 4 MiB upload budget; re-read a smaller image if needed]";

pub(super) fn cap_worker_images(messages: &[SharedMessage]) -> Option<Vec<SharedMessage>> {
    cap_images(messages, WORKER_IMAGE_COUNT, WORKER_IMAGE_BYTES)
}

fn cap_images(
    messages: &[SharedMessage],
    count_cap: usize,
    byte_cap: usize,
) -> Option<Vec<SharedMessage>> {
    let slots = image_slots(messages);
    let bytes = |slot| {
        let block = block_at(messages, slot);
        if block["source"]["type"] == "base64" {
            block["source"]["data"].as_str().map_or(0, str::len)
        } else {
            0
        }
    };
    let mut count = slots.len();
    let mut total = slots.iter().map(|&slot| bytes(slot)).sum::<usize>();
    if count <= count_cap && total <= byte_cap {
        return None;
    }
    let mut out = messages.to_vec();
    for slot in slots {
        if count <= count_cap && total <= byte_cap {
            break;
        }
        total -= bytes(slot);
        count -= 1;
        replace_with_text(&mut out, slot, WORKER_IMAGE_LABEL.to_owned());
    }
    tracing::info!(
        remaining_images = count,
        remaining_bytes = total,
        "worker image upload budget applied"
    );
    Some(out)
}

#[cfg(test)]
mod worker_image_tests {
    use super::*;
    fn image(data: &str) -> Value {
        json!({"type":"image", "source":{"type":"base64","data":data,"media_type":"image/png"}})
    }
    fn message(content: Value) -> SharedMessage {
        Arc::new(json!({"role":"user","content":content}))
    }
    #[test]
    fn newest_images_survive_both_budgets_without_mutating_history() {
        let history = vec![message(
            json!([image("old"), {"type":"tool_result","tool_use_id":"t1",
            "content":[{"type":"text","text":"summary"},image("middle"),image("new")]}]),
        )];
        let original = history[0].clone();
        let out = cap_images(&history, 2, 9).unwrap();
        assert_eq!(out[0]["content"][0]["type"], "text");
        assert_eq!(out[0]["content"][1]["tool_use_id"], "t1");
        assert_eq!(out[0]["content"][1]["content"][0]["text"], "summary");
        assert_eq!(out[0]["content"][1]["content"][1], image("middle"));
        assert_eq!(out[0]["content"][1]["content"][2], image("new"));
        assert!(Arc::ptr_eq(&original, &history[0]));
        assert_eq!(history[0]["content"][0], image("old"));
        assert_eq!(out, cap_images(&history, 2, 9).unwrap());
        let byte_only = cap_images(&history, 99, 3).unwrap();
        assert_eq!(image_slots(&byte_only).len(), 1);
        assert_eq!(byte_only[0]["content"][1]["content"][2], image("new"));
    }
    #[test]
    fn count_url_and_oversize_images_and_keep_noop_allocation_free() {
        let history = vec![message(json!([image("too big")]))];
        assert!(cap_images(&history, 6, 7).is_none());
        assert_eq!(image_slots(&cap_images(&history, 6, 6).unwrap()).len(), 0);
        assert!(cap_worker_images(&[]).is_none());
        let urls = vec![message(json!((0..7).map(|_| json!({"type":"image","source":{"type":"url","url":"https://example.invalid/a"}})).collect::<Vec<_>>()))];
        let out = cap_worker_images(&urls).unwrap();
        assert_eq!(image_slots(&out).len(), 6);
        assert_eq!(image_slots(&urls).len(), 7);
        assert_eq!(WORKER_IMAGE_BYTES, 4 * 1024 * 1024);
    }
}
