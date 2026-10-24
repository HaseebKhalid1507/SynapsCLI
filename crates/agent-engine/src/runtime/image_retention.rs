//! Request-local replay of canonical USER batches. No persisted/runtime state.
//! Editing, truncating or compacting history starts a new replay window: this
//! cannot guarantee non-resurrection across changes to the original history.
use crate::SharedMessage;
#[cfg(test)]
use serde_json::json;
use serde_json::Value;
#[cfg(test)]
use std::sync::Arc;

use super::attachments::{serialized_size, MAX_HISTORY_ENCODED_BYTES};

const MAX_IMAGES: usize = 100;
const CHUNK: usize = 40;
const LOW_WATER: usize = 12 * 1024 * 1024;
pub(super) const MAX_ORIGINAL_BYTES: usize = 256 * 1024 * 1024;
const MAX_ORIGINAL_IMAGES: usize = 4096;
pub(super) const OMITTED: &str =
    "[Image omitted from this request by history retention. Re-read the file if needed.]";
const PROTECTED: &str =
    "Attachment history cannot fit documents and the current image batch within the request budget";

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

/// Returns a monotonic image-occurrence prefix watermark. Call after original
/// structural validation and before same-role merging. Each user message is a
/// batch, including all sibling tool results. Documents accumulate forever.
pub(super) fn plan(messages: &[SharedMessage]) -> Result<usize, String> {
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

#[cfg(test)]
pub(super) fn apply(messages: &mut [SharedMessage]) -> Result<usize, String> {
    let watermark = plan(messages)?;
    let mut occurrence = 0;
    for message in messages {
        if occurrence >= watermark {
            break;
        }
        if message["role"] != "user" {
            continue;
        }
        // Avoid cloning text-only and assistant messages.
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
                    retire(block, &mut occurrence, watermark);
                } else if block["type"] == "tool_result" {
                    if let Some(nested) = block["content"].as_array_mut() {
                        for item in nested {
                            if item["type"] == "image" {
                                retire(item, &mut occurrence, watermark);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(watermark)
}

#[cfg(test)]
fn retire(block: &mut Value, occurrence: &mut usize, watermark: usize) {
    if *occurrence < watermark {
        *block = json!({"type":"text", "text":OMITTED});
    }
    *occurrence += 1;
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
}
