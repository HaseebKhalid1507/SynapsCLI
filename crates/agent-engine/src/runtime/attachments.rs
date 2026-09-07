//! Offline preflight for canonical user attachments and rich tool results.
//!
//! This is a model **and** transport gate, not a model-name heuristic. Nothing
//! here fetches a catalog, reads an attachment path, or contacts a provider.
//! Call again on every outgoing history (including resume/model switches).
//! Errors are static: neither payloads, titles, MIME strings nor model IDs from
//! untrusted messages are interpolated into diagnostics.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::Serialize;
use serde_json::Value;
use std::io::{self, Write};

use super::openai::{self, catalog, AuthPolicy, WireProtocol};
use catalog::{capability_cache, CatalogModel, CatalogSource, Modality};

/// Shared with attachment loaders; nested tool media counts toward this limit.
pub const MAX_ATTACHMENTS_PER_MESSAGE: usize = 8;
/// 3.5 MiB, matching the read tool's raw image bound.
pub const MAX_IMAGE_BYTES: usize = 3_670_016;
pub const MAX_PDF_BYTES: usize = 10 * 1024 * 1024;
/// UTF-8 bytes, not Unicode scalar values.
pub const MAX_TEXT_DOCUMENT_BYTES: usize = 256 * 1024;
/// Encoded attachment blocks across the complete outgoing history.
pub const MAX_HISTORY_ENCODED_BYTES: usize = 20 * 1024 * 1024;
/// Keep the broker's existing bound authoritative; never raise it for media.
pub const MAX_BROKER_PROXY_BYTES: usize = crate::auth::broker::MAX_PROXY_REQUEST_BYTES;

// Preflight does not receive the system prompt/tools/options. Leave headroom
// for that envelope and for wire lowering; the broker still checks the actual
// complete serialized request. This is deliberately stricter than its cap.
const BROKER_ENVELOPE_RESERVE_BYTES: usize = 64 * 1024;
const WIRE_OVERHEAD_PER_ATTACHMENT: usize = 1024;
const MAX_TITLE_BYTES: usize = 255;

const BAD_ATTACHMENT: &str = "Attachment has an invalid canonical source or media type";
const BAD_BASE64: &str = "Attachment has invalid base64 data";
const TOO_LARGE: &str = "Attachment exceeds its byte limit";
const HISTORY_LIMIT: &str = "Attachments exceed the encoded history byte limit";
const BROKER_LIMIT: &str = "Attachment history exceeds the conservative 2 MiB broker proxy budget";
const UNSUPPORTED_TRANSPORT: &str =
    "Selected transport does not support image or document attachments";

/// Validate canonical top-level user image/document blocks and media inside
/// user `tool_result.content` arrays. Non-media tool inputs are not traversed:
/// a JSON object supplied as tool arguments is data, not a content block.
///
/// Text-only histories remain unaffected, even on unsupported transports.
pub fn validate_messages(model: &str, messages: &[crate::SharedMessage]) -> Result<(), String> {
    let mut validator = Validator::new(model);
    for message in messages {
        let mut count = 0;
        if let Some(blocks) = message.get("content").and_then(Value::as_array) {
            validator.blocks(
                blocks,
                message.get("role").and_then(Value::as_str) == Some("user"),
                false,
                &mut count,
            )?;
        } else if message.get("content").is_some_and(contains_media) {
            // A single media object is not canonical message content. Ordinary
            // opaque/provider content is outside this attachment validator.
            return Err("Attachments require an array of canonical content blocks".into());
        }
    }
    validator.broker_budget(messages)
}

/// Validate one tool's rich output before committing it to a tool_result.
/// Callers can replace Err with a plain-text tool error; no offending bytes
/// need enter history. The outgoing full-history check remains mandatory,
/// since sibling tool results share one message count/broker/history budget.
pub fn validate_tool_blocks(model: &str, blocks: &[Value]) -> Result<(), String> {
    let mut validator = Validator::new(model);
    validator.blocks(blocks, true, true, &mut 0)?;
    validator.broker_budget(blocks)
}

/// Assemble a sibling tool batch without admitting newly over-budget media.
/// Strip only rich results introduced by this batch, newest first, preserving
/// every correlation ID/order and returning explicit tool errors. Previously
/// committed history is never rewritten. Its validity is checked before tools.
pub(crate) fn bounded_tool_results(
    model: &str,
    history: &[crate::SharedMessage],
    mut results: Vec<Value>,
) -> Value {
    let mut candidate = history.to_vec();
    candidate.push(std::sync::Arc::new(
        serde_json::json!({"role":"user","content":results}),
    ));
    for index in (0..results.len()).rev() {
        let Err(error) = validate_messages(model, &candidate) else {
            break;
        };
        if results[index].get("content").is_some_and(contains_media) {
            results[index]["content"] = Value::String(format!("Attachment not sent: {error}"));
            results[index]["is_error"] = Value::Bool(true);
            *candidate.last_mut().expect("candidate tail") =
                std::sync::Arc::new(serde_json::json!({"role":"user","content":results}));
        }
    }
    serde_json::json!({"role":"user","content":results})
}

struct Transport {
    wire: WireProtocol,
    broker_proxy: bool,
    image: bool,
    pdf: bool,
}

impl Transport {
    fn for_model(model: &str) -> Result<Self, String> {
        // Explicit cloud invokes and extension providers precede ordinary
        // runtime routing. Never borrow a same-named native model's evidence.
        let (cloud_model, context) = crate::auth::cloud::split_model_route(model);
        if context.is_some()
            || cloud_model.split_once('/').is_some_and(|(provider, _)| {
                provider.parse::<crate::auth::CloudProviderId>().is_ok()
            })
            || crate::extensions::providers::ProviderRegistry::parse_model_id(model).is_some()
        {
            return Err(UNSUPPORTED_TRANSPORT.into());
        }
        let (provider, id) = model.split_once('/').unwrap_or(("anthropic", model));
        if provider.is_empty()
            || id.is_empty()
            || model.trim() != model
            || model.chars().any(char::is_control)
            || provider == "google-gemini"
        {
            return Err(UNSUPPORTED_TRANSPORT.into());
        }

        // `local` routing otherwise reads endpoint configuration. Its wire and
        // proxy policy are fixed, and validation needs no endpoint/credentials.
        let (wire, broker_proxy) = if provider == "local" {
            (WireProtocol::OpenAiChatCompletions, true)
        } else {
            let route = openai::resolve_route(model).ok_or(UNSUPPORTED_TRANSPORT)?;
            if route.provider != provider || route.model != id {
                return Err(UNSUPPORTED_TRANSPORT.into());
            }
            (route.wire, route.auth == AuthPolicy::BrokerProxy)
        };
        if wire == WireProtocol::GoogleGeminiCodeAssist {
            return Err(UNSUPPORTED_TRANSPORT.into());
        }

        let qualified = format!("{provider}/{id}");
        let known_native = provider == "anthropic"
            && agent_core::models::KNOWN_MODELS
                .iter()
                .any(|(known, _)| *known == id);
        // A cache hit is authoritative, including a text-only/empty/unknown
        // list: NEVER fall through to static image evidence after a hit.
        let cached = capability_cache::get(&qualified);
        let fallback = if cached.is_none() && provider == "openai-codex" {
            catalog::codex_static_catalog_models()
                .into_iter()
                .find(|row| row.runtime_id() == qualified)
        } else {
            None
        };
        Ok(Self::with_evidence(
            wire,
            broker_proxy,
            provider,
            id,
            known_native,
            cached.as_ref(),
            fallback.as_ref(),
        ))
    }

    fn with_evidence(
        wire: WireProtocol,
        broker_proxy: bool,
        provider: &str,
        id: &str,
        known_native: bool,
        cached: Option<&CatalogModel>,
        fallback: Option<&CatalogModel>,
    ) -> Self {
        let advertised = |modality: Modality| {
            let row = cached.or(fallback);
            row.is_some_and(|row| {
                // Generic compatible routes require exact LIVE advertising.
                // Codex alone has an evidence-backed static modality table.
                row.provider_key == provider
                    && row.id == id
                    && (row.source == CatalogSource::Live
                        || (provider == "openai-codex"
                            && row.source == CatalogSource::StaticFallback))
                    && row.input_modalities.contains(&modality)
            })
        };
        // The native catalog parser currently defaults every row to Text and
        // does not parse modality metadata. Its cache cannot revoke known
        // native vision/PDF evidence. Only the exact KNOWN_MODELS list grants
        // native binary support; unknown native rows never gain it via cache.
        let image = if wire == WireProtocol::AnthropicMessages {
            known_native
        } else {
            advertised(Modality::Image)
        };
        let pdf = match wire {
            WireProtocol::AnthropicMessages => known_native,
            WireProtocol::OpenAiChatCompletions
            | WireProtocol::OpenAiResponses
            | WireProtocol::CodexResponses => advertised(Modality::File),
            WireProtocol::GoogleGeminiCodeAssist => false,
        };
        Self {
            wire,
            broker_proxy,
            image,
            pdf,
        }
    }

    fn permits(&self, kind: AttachmentKind) -> Result<(), String> {
        if !matches!(
            self.wire,
            WireProtocol::AnthropicMessages
                | WireProtocol::OpenAiChatCompletions
                | WireProtocol::OpenAiResponses
                | WireProtocol::CodexResponses
        ) {
            return Err(UNSUPPORTED_TRANSPORT.into());
        }
        match kind {
            AttachmentKind::Image if !self.image => {
                Err("Selected model lacks exact image input capability metadata".into())
            }
            AttachmentKind::Pdf if !self.pdf => {
                Err("Selected model and transport lack supported PDF file input capability".into())
            }
            // Text documents are deliberately kept as documents until the
            // supported wire adapter lowers them to lower-authority text.
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Copy)]
enum AttachmentKind {
    Image,
    Pdf,
    Text,
}

struct Validator<'a> {
    model: &'a str,
    transport: Option<Transport>,
    encoded_bytes: usize,
    attachments: usize,
}

impl<'a> Validator<'a> {
    fn new(model: &'a str) -> Self {
        Self {
            model,
            transport: None,
            encoded_bytes: 0,
            attachments: 0,
        }
    }

    fn blocks(
        &mut self,
        blocks: &[Value],
        user: bool,
        in_tool_result: bool,
        count: &mut usize,
    ) -> Result<(), String> {
        for block in blocks {
            let kind = block.get("type").and_then(Value::as_str);
            match kind {
                Some("image" | "document") => {
                    if !user {
                        return Err(
                            "Attachments are only supported in user content or tool results".into(),
                        );
                    }
                    *count += 1;
                    if *count > MAX_ATTACHMENTS_PER_MESSAGE {
                        return Err(
                            "A message may contain at most 8 attachments, including tool media"
                                .into(),
                        );
                    }
                    self.attachment(block)?;
                }
                Some("tool_result") => {
                    if let Some(content) = block.get("content") {
                        match content {
                            Value::Array(nested) if user && !in_tool_result => {
                                self.blocks(nested, true, true, count)?;
                            }
                            _ if contains_media(content) => {
                                return Err(
                                    "Tool attachments require a canonical user tool_result array"
                                        .into(),
                                );
                            }
                            _ => {}
                        }
                    }
                    if block.get("source").is_some() {
                        return Err(BAD_ATTACHMENT.into());
                    }
                }
                // Preserve ordinary text/tool inputs/thinking/signatures and
                // opaque provider blocks. Do not walk tool_use.input: objects
                // inside arguments are data, not canonical media containers.
                _ if is_media_block(block) || (block.is_array() && contains_media(block)) => {
                    return Err("Unsupported or malformed canonical media block".into());
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn attachment(&mut self, block: &Value) -> Result<(), String> {
        if self.transport.is_none() {
            self.transport = Some(Transport::for_model(self.model)?);
        }
        let kind = validate_attachment(block)?;
        self.transport
            .as_ref()
            .expect("transport resolved")
            .permits(kind)?;
        let remaining = MAX_HISTORY_ENCODED_BYTES.saturating_sub(self.encoded_bytes);
        self.encoded_bytes += serialized_size(block, remaining).ok_or(HISTORY_LIMIT)?;
        self.attachments += 1;
        Ok(())
    }

    fn broker_budget<T: Serialize + ?Sized>(&self, content: &T) -> Result<(), String> {
        if !self.transport.as_ref().is_some_and(|t| t.broker_proxy) {
            return Ok(());
        }
        let overhead = self
            .attachments
            .saturating_mul(WIRE_OVERHEAD_PER_ATTACHMENT);
        let remaining = MAX_BROKER_PROXY_BYTES
            .saturating_sub(BROKER_ENVELOPE_RESERVE_BYTES)
            .saturating_sub(overhead);
        serialized_size(content, remaining).ok_or(BROKER_LIMIT)?;
        Ok(())
    }
}

fn validate_attachment(block: &Value) -> Result<AttachmentKind, String> {
    let object = block.as_object().ok_or(BAD_ATTACHMENT)?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "type" | "source" | "title" | "cache_control"))
    {
        return Err(BAD_ATTACHMENT.into());
    }
    let source = block
        .get("source")
        .and_then(Value::as_object)
        .ok_or(BAD_ATTACHMENT)?;
    // Exact canonical sources only. A base64 source with a second url/file_id
    // route is not accepted, even if its inline data happens to be valid.
    if source.len() != 3
        || source
            .keys()
            .any(|key| !matches!(key.as_str(), "type" | "media_type" | "data"))
    {
        return Err(BAD_ATTACHMENT.into());
    }
    let source_type = source
        .get("type")
        .and_then(Value::as_str)
        .ok_or(BAD_ATTACHMENT)?;
    let mime = source
        .get("media_type")
        .and_then(Value::as_str)
        .ok_or(BAD_ATTACHMENT)?;
    let data = source
        .get("data")
        .and_then(Value::as_str)
        .ok_or(BAD_ATTACHMENT)?;
    if let Some(title) = block.get("title") {
        validate_title(title)?;
    }
    match (block.get("type").and_then(Value::as_str), source_type, mime) {
        (Some("image"), "base64", "image/png" | "image/jpeg" | "image/gif" | "image/webp") => {
            if block.get("title").is_some() {
                return Err(BAD_ATTACHMENT.into());
            }
            let bytes = decode_bounded(data, MAX_IMAGE_BYTES)?;
            if crate::tools::read::sniff_image_mime(&bytes) != Some(mime) {
                return Err("Image attachment data does not match its declared media type".into());
            }
            // Call integrity only after the sniff has established the minimum
            // header length (notably WebP's RIFF size field).
            if crate::tools::read::image_integrity_error(mime, &bytes).is_some()
                || !crate::tools::read::image_dimensions(mime, &bytes)
                    .is_some_and(|(w, h)| (1..=8000).contains(&w) && (1..=8000).contains(&h))
            {
                return Err("Image attachment is corrupt or has unsupported dimensions".into());
            }
            Ok(AttachmentKind::Image)
        }
        (Some("document"), "base64", "application/pdf") => {
            let bytes = decode_bounded(data, MAX_PDF_BYTES)?;
            if !bytes.starts_with(b"%PDF-")
                || !bytes
                    .windows(5)
                    .rev()
                    .take(1024)
                    .any(|window| window == b"%%EOF")
            {
                return Err(
                    "PDF attachment is invalid or does not match its declared media type".into(),
                );
            }
            Ok(AttachmentKind::Pdf)
        }
        (Some("document"), "text", "text/plain") => {
            // A text document's title is the filename, not a filesystem path.
            validate_title(block.get("title").ok_or(BAD_ATTACHMENT)?)?;
            if data.len() > MAX_TEXT_DOCUMENT_BYTES {
                return Err(TOO_LARGE.into());
            }
            Ok(AttachmentKind::Text)
        }
        _ => Err(BAD_ATTACHMENT.into()),
    }
}

fn validate_title(title: &Value) -> Result<(), String> {
    let title = title.as_str().ok_or(BAD_ATTACHMENT)?;
    if title.is_empty()
        || title.len() > MAX_TITLE_BYTES
        || matches!(title, "." | "..")
        || title.contains(['/', '\\'])
        || title.chars().any(char::is_control)
    {
        return Err("Document attachment title must be a bounded filename".into());
    }
    Ok(())
}

fn decode_bounded(data: &str, max_bytes: usize) -> Result<Vec<u8>, String> {
    // Check before decoding/allocation. STANDARD also enforces canonical
    // padding/trailing bits and rejects whitespace, URL-safe data, and URIs.
    let max_encoded = max_bytes.div_ceil(3) * 4;
    if data.len() > max_encoded {
        return Err(TOO_LARGE.into());
    }
    let decoded = STANDARD.decode(data).map_err(|_| BAD_BASE64)?;
    if decoded.is_empty() {
        return Err(BAD_BASE64.into());
    }
    if decoded.len() > max_bytes {
        return Err(TOO_LARGE.into());
    }
    Ok(decoded)
}

// Media-shaped canonical/wire blocks must never bypass the source gate.
// Do not inspect arbitrary nested values (e.g. a tool's JSON arguments).
fn is_media_block(block: &Value) -> bool {
    block.get("source").is_some()
        || [
            "image_url",
            "input_image",
            "input_file",
            "file_id",
            "file_data",
            "mimeType",
        ]
        .iter()
        .any(|key| block.get(key).is_some())
        || matches!(
            block.get("type").and_then(Value::as_str),
            Some(
                "image"
                    | "document"
                    | "image_url"
                    | "input_image"
                    | "input_file"
                    | "file"
                    | "audio"
                    | "input_audio"
                    | "video"
                    | "resource"
                    | "resource_link"
            )
        )
}

// Iterative traversal of canonical containers only, so malformed nested tool
// results cannot hide media or trigger unbounded Rust recursion.
fn contains_media(content: &Value) -> bool {
    let mut pending = vec![content];
    while let Some(value) = pending.pop() {
        if is_media_block(value) {
            return true;
        }
        if let Some(blocks) = value.as_array() {
            pending.extend(blocks);
        } else if value.get("type").and_then(Value::as_str) == Some("tool_result") {
            if let Some(nested) = value.get("content") {
                pending.push(nested);
            }
        }
    }
    false
}

/// Count JSON bytes, including escaping, without allocating a payload copy.
fn serialized_size<T: Serialize + ?Sized>(value: &T, limit: usize) -> Option<usize> {
    struct Counter {
        bytes: usize,
        limit: usize,
    }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes) {
                return Err(io::Error::other("attachment serialization budget exceeded"));
            }
            self.bytes += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { bytes: 0, limit };
    serde_json::to_writer(&mut counter, value).ok()?;
    Some(counter.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    const NATIVE: &str = "anthropic/claude-sonnet-4-6";
    const ASTRA: &str = "openai-codex/gpt-6-astra";
    const SPARK: &str = "openai-codex/gpt-5.3-codex-spark";
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aX1sAAAAASUVORK5CYII=";

    fn image() -> Value {
        json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG}})
    }

    fn pdf() -> Value {
        // Exercises the validator's bounded header/trailer check, not a PDF renderer.
        json!({"type":"document","title":"sample.pdf","source":{
            "type":"base64","media_type":"application/pdf",
            "data":STANDARD.encode(b"%PDF-1.7\n%%EOF\n")
        }})
    }

    fn text(data: &str) -> Value {
        json!({"type":"document","title":"notes.txt","source":{
            "type":"text","media_type":"text/plain","data":data
        }})
    }

    fn message(blocks: Vec<Value>) -> crate::SharedMessage {
        Arc::new(json!({"role":"user","content":blocks}))
    }

    fn check(model: &str, block: Value) -> Result<(), String> {
        validate_messages(model, &[message(vec![block])])
    }

    fn row(
        provider: &str,
        id: &str,
        modalities: Vec<Modality>,
        source: CatalogSource,
    ) -> CatalogModel {
        let mut row = CatalogModel::new(provider, provider, id).unwrap();
        row.input_modalities = modalities;
        row.source = source;
        row
    }

    #[test]
    fn combined_tool_batch_rejects_only_overflowing_media_and_preserves_ids() {
        let results: Vec<_> = (0..9).map(|i| json!({"type":"tool_result", "tool_use_id":format!("tool_{i}"), "content":[image()]})).collect();
        let bounded = bounded_tool_results(NATIVE, &[], results);
        assert!(validate_messages(NATIVE, &[Arc::new(bounded.clone())]).is_ok());
        for i in 0..9 {
            assert_eq!(bounded["content"][i]["tool_use_id"], format!("tool_{i}"));
        }
        assert!(bounded["content"][7]["content"].is_array());
        assert_eq!(bounded["content"][8]["is_error"], true);
        assert!(bounded["content"][8]["content"]
            .as_str()
            .unwrap()
            .starts_with("Attachment not sent:"));
    }

    #[test]
    fn astra_image_and_text_work_but_static_pdf_and_spark_image_do_not() {
        assert!(check(ASTRA, image()).is_ok());
        assert!(validate_tool_blocks(ASTRA, &[image()]).is_ok());
        assert!(check(ASTRA, text("attached text")).is_ok());
        assert!(check(ASTRA, pdf()).is_err());
        assert!(check(SPARK, image()).is_err());
        assert!(check(SPARK, text("still a text model")).is_ok());
        assert!(check("openai-codex/gpt-5.4", image()).is_err());
        assert!(check("openai-codex/gpt-6-astra-unobserved", image()).is_err());
    }

    #[test]
    fn exact_native_anthropic_accepts_image_pdf_and_text() {
        for model in [NATIVE, "claude-sonnet-4-6"] {
            assert!(
                validate_messages(model, &[message(vec![image(), pdf(), text("hello")])]).is_ok()
            );
            assert!(validate_tool_blocks(model, &[image(), pdf(), text("hello")]).is_ok());
        }
        for model in [
            "anthropic/claude-unobserved",
            "claude-sonnet-4-6-unobserved",
        ] {
            assert!(check(model, image()).is_err());
            assert!(check(model, pdf()).is_err());
        }
        // Native catalog rows default to Text; they cannot accidentally revoke
        // the independently maintained exact native vision/PDF evidence.
        let cached = row(
            "anthropic",
            "claude-sonnet-4-6",
            vec![Modality::Text],
            CatalogSource::Live,
        );
        let transport = Transport::with_evidence(
            WireProtocol::AnthropicMessages,
            false,
            "anthropic",
            "claude-sonnet-4-6",
            true,
            Some(&cached),
            None,
        );
        assert!(transport.permits(AttachmentKind::Image).is_ok());
        assert!(transport.permits(AttachmentKind::Pdf).is_ok());
    }

    #[test]
    fn exact_live_cache_support_and_revocation_are_rechecked_for_generic_and_codex() {
        // Unique IDs avoid modifying real catalog rows or clearing shared cache
        // state while the suite runs in parallel.
        for provider in ["local", "openai-codex"] {
            let id = "attachment-validator-live-revocation-sa26";
            let model = format!("{provider}/{id}");
            let history = [message(vec![image(), pdf()])];
            assert!(validate_messages(&model, &history).is_err());
            capability_cache::insert(row(
                provider,
                id,
                vec![Modality::Image, Modality::File],
                CatalogSource::Live,
            ));
            assert!(validate_messages(&model, &history).is_ok());
            assert!(validate_tool_blocks(&model, &[image(), pdf()]).is_ok());
            for modalities in [vec![Modality::Text], vec![]] {
                capability_cache::insert(row(provider, id, modalities, CatalogSource::Live));
                assert!(validate_messages(&model, &history).is_err());
                assert!(validate_tool_blocks(&model, &[image()]).is_err());
                assert!(check(&model, pdf()).is_err());
                assert!(check(&model, text("text needs no binary capability")).is_ok());
            }
        }
    }

    #[test]
    fn generic_cache_requires_exact_provider_id_and_live_source() {
        let id = "attachment-validator-provider-scope-sa26";
        capability_cache::insert(row(
            "groq",
            id,
            vec![Modality::Image, Modality::File],
            CatalogSource::Live,
        ));
        assert!(check(&format!("local/{id}"), image()).is_err());
        assert!(check(&format!("local/{id}"), pdf()).is_err());
        for source in [
            CatalogSource::StaticFallback,
            CatalogSource::StaticWithLive,
            CatalogSource::Inferred,
        ] {
            capability_cache::insert(row(
                "local",
                id,
                vec![Modality::Image, Modality::File],
                source,
            ));
            assert!(check(&format!("local/{id}"), image()).is_err());
            assert!(check(&format!("local/{id}"), pdf()).is_err());
        }
        let wrong = row(
            "local",
            "different-model",
            vec![Modality::Image],
            CatalogSource::Live,
        );
        let transport = Transport::with_evidence(
            WireProtocol::OpenAiChatCompletions,
            true,
            "local",
            id,
            false,
            Some(&wrong),
            None,
        );
        assert!(transport.permits(AttachmentKind::Image).is_err());
    }

    #[test]
    fn cached_codex_revocation_overrides_astra_static_evidence_without_global_mutation() {
        let fallback = catalog::codex_static_catalog_models()
            .into_iter()
            .find(|row| row.runtime_id() == ASTRA)
            .unwrap();
        for modalities in [vec![Modality::Text], vec![]] {
            let cached = row(
                "openai-codex",
                "gpt-6-astra",
                modalities,
                CatalogSource::Live,
            );
            let transport = Transport::with_evidence(
                WireProtocol::CodexResponses,
                false,
                "openai-codex",
                "gpt-6-astra",
                false,
                Some(&cached),
                Some(&fallback),
            );
            assert!(transport.permits(AttachmentKind::Image).is_err());
            assert!(transport.permits(AttachmentKind::Pdf).is_err());
        }
    }

    #[test]
    fn supported_wire_protocols_require_separate_image_and_file_evidence() {
        for wire in [
            WireProtocol::OpenAiChatCompletions,
            WireProtocol::OpenAiResponses,
            WireProtocol::CodexResponses,
        ] {
            let cached = row(
                "local",
                "wire-evidence",
                vec![Modality::Image],
                CatalogSource::Live,
            );
            let transport = Transport::with_evidence(
                wire,
                false,
                "local",
                "wire-evidence",
                false,
                Some(&cached),
                None,
            );
            assert!(transport.permits(AttachmentKind::Image).is_ok());
            assert!(transport.permits(AttachmentKind::Pdf).is_err());
            assert!(transport.permits(AttachmentKind::Text).is_ok());
        }
    }

    #[test]
    fn unsupported_cloud_gemini_extensions_reject_all_attachment_kinds() {
        for model in [
            "aws-bedrock/anthropic.claude-3-haiku",
            "aws-bedrock/anthropic.claude-3-haiku#synaps-context=ctx-test",
            "google-vertex/claude-sonnet-4-6",
            "azure-openai/gpt-6-astra",
            "google-gemini/gemini-2.5-pro",
            "plugin:provider:model",
        ] {
            for block in [image(), pdf(), text("not even text documents")] {
                assert_eq!(
                    check(model, block.clone()).unwrap_err(),
                    UNSUPPORTED_TRANSPORT,
                    "{model}"
                );
                assert_eq!(
                    validate_tool_blocks(model, &[block]).unwrap_err(),
                    UNSUPPORTED_TRANSPORT,
                    "{model}"
                );
            }
            assert!(validate_messages(
                model,
                &[message(vec![json!({"type":"text","text":"ordinary text"})])]
            )
            .is_ok());
        }
    }

    #[test]
    fn invalid_base64_is_rejected_without_echoing_payload() {
        for data in [
            "",
            "private-payload%%%",
            "Zg",
            "Zh==",
            "Zg==\n",
            "____",
            "data:image/png;base64,Zg==",
        ] {
            let mut block = image();
            block["source"]["data"] = json!(data);
            assert_eq!(check(NATIVE, block).unwrap_err(), BAD_BASE64);
        }
    }

    #[test]
    fn corrupt_mismatched_and_out_of_bounds_images_are_rejected() {
        let bytes = STANDARD.decode(PNG).unwrap();
        let mut truncated = image();
        truncated["source"]["data"] = json!(STANDARD.encode(&bytes[..bytes.len() - 1]));
        assert!(check(NATIVE, truncated).unwrap_err().contains("corrupt"));
        let mut mismatch = image();
        mismatch["source"]["media_type"] = json!("image/jpeg");
        assert!(check(NATIVE, mismatch)
            .unwrap_err()
            .contains("declared media type"));
        for width in [0_u32, 8001] {
            let mut invalid = bytes.clone();
            invalid[16..20].copy_from_slice(&width.to_be_bytes());
            let mut block = image();
            block["source"]["data"] = json!(STANDARD.encode(invalid));
            assert!(check(NATIVE, block).unwrap_err().contains("dimensions"));
        }
        for bytes in [
            b"\x89PNG\r\n\x1a\n".as_slice(),
            b"RIFF\0\0\0\0WEBP".as_slice(),
        ] {
            let mut block = image();
            block["source"]["media_type"] =
                json!(crate::tools::read::sniff_image_mime(bytes).unwrap());
            block["source"]["data"] = json!(STANDARD.encode(bytes));
            assert!(check(NATIVE, block).is_err());
        }
    }

    #[test]
    fn pdf_requires_matching_header_and_nearby_eof() {
        for bytes in [
            b"not a PDF %%EOF".to_vec(),
            b"%PDF-1.7\ntruncated".to_vec(),
            [b"%PDF-1.7\n%%EOF".as_slice(), &vec![b'x'; 1030]].concat(),
        ] {
            let mut block = pdf();
            block["source"]["data"] = json!(STANDARD.encode(bytes));
            assert!(check(NATIVE, block)
                .unwrap_err()
                .starts_with("PDF attachment is invalid"));
        }
    }

    #[test]
    fn canonical_sources_mime_and_titles_are_exact() {
        for (key, value) in [
            ("type", json!("url")),
            ("media_type", json!("IMAGE/PNG")),
            ("data", json!(null)),
            ("url", json!("https://example.invalid/private")),
            ("file_id", json!("secret-id")),
        ] {
            let mut block = image();
            block["source"][key] = value;
            assert_eq!(check(NATIVE, block).unwrap_err(), BAD_ATTACHMENT);
        }
        let mut titled_image = image();
        titled_image["title"] = json!("image.png");
        assert_eq!(check(NATIVE, titled_image).unwrap_err(), BAD_ATTACHMENT);
        for title in [
            "",
            ".",
            "..",
            "../private.txt",
            "a/b",
            "a\\b",
            "private\nname",
        ] {
            let mut block = text("private-payload");
            block["title"] = json!(title);
            assert_eq!(
                check(NATIVE, block).unwrap_err(),
                "Document attachment title must be a bounded filename"
            );
        }
        let mut missing_title = text("private-payload");
        missing_title.as_object_mut().unwrap().remove("title");
        assert_eq!(check(NATIVE, missing_title).unwrap_err(), BAD_ATTACHMENT);
        let mut long_title = text("x");
        long_title["title"] = json!("x".repeat(MAX_TITLE_BYTES + 1));
        assert!(check(NATIVE, long_title).is_err());
        for mime in [
            "text/html",
            "application/octet-stream",
            "text/plain; charset=utf-8",
        ] {
            let mut block = text("x");
            block["source"]["media_type"] = json!(mime);
            assert_eq!(check(NATIVE, block).unwrap_err(), BAD_ATTACHMENT);
        }
    }

    #[test]
    fn byte_bounds_cover_encoded_and_decoded_lengths_and_utf8() {
        assert_eq!(MAX_IMAGE_BYTES, 3_670_016);
        assert_eq!(MAX_PDF_BYTES, 10 * 1024 * 1024);
        assert_eq!(MAX_TEXT_DOCUMENT_BYTES, 256 * 1024);
        assert_eq!(MAX_HISTORY_ENCODED_BYTES, 20 * 1024 * 1024);
        assert_eq!(MAX_BROKER_PROXY_BYTES, 2 * 1024 * 1024);
        assert_eq!(
            decode_bounded(&STANDARD.encode([0_u8; 2]), 2).unwrap(),
            vec![0, 0]
        );
        // Three bytes fit the same encoded-length bound as two; the decoded
        // bound must still reject them after decoding.
        assert_eq!(
            decode_bounded(&STANDARD.encode([0_u8; 3]), 2).unwrap_err(),
            TOO_LARGE
        );
        for (mut block, limit) in [(image(), MAX_IMAGE_BYTES), (pdf(), MAX_PDF_BYTES)] {
            block["source"]["data"] = json!("A".repeat(limit.div_ceil(3) * 4 + 4));
            assert_eq!(check(NATIVE, block).unwrap_err(), TOO_LARGE);
        }
        let at_limit = "é".repeat(MAX_TEXT_DOCUMENT_BYTES / 2);
        assert!(check(NATIVE, text(&at_limit)).is_ok());
        assert_eq!(
            check(NATIVE, text(&(at_limit + "é"))).unwrap_err(),
            TOO_LARGE
        );
    }

    #[test]
    fn nested_tool_results_share_message_count_but_history_messages_do_not() {
        let seven = vec![image(); MAX_ATTACHMENTS_PER_MESSAGE - 1];
        let nested = json!({"type":"tool_result","tool_use_id":"read-1","content":seven});
        assert!(validate_messages(NATIVE, &[message(vec![image(), nested.clone()])]).is_ok());
        let error =
            validate_messages(NATIVE, &[message(vec![image(), image(), nested])]).unwrap_err();
        assert_eq!(
            error,
            "A message may contain at most 8 attachments, including tool media"
        );
        assert!(validate_tool_blocks(NATIVE, &vec![image(); MAX_ATTACHMENTS_PER_MESSAGE]).is_ok());
        assert!(
            validate_tool_blocks(NATIVE, &vec![image(); MAX_ATTACHMENTS_PER_MESSAGE + 1]).is_err()
        );
        let full = message(vec![image(); MAX_ATTACHMENTS_PER_MESSAGE]);
        assert!(validate_messages(NATIVE, &[full.clone(), full]).is_ok());
    }

    #[test]
    fn encoded_history_budget_covers_all_messages_and_json_escaping() {
        let full = message(vec![
            text(&"x".repeat(MAX_TEXT_DOCUMENT_BYTES));
            MAX_ATTACHMENTS_PER_MESSAGE
        ]);
        assert!(validate_messages(NATIVE, &vec![full.clone(); 9]).is_ok());
        assert_eq!(
            validate_messages(NATIVE, &vec![full; 10]).unwrap_err(),
            HISTORY_LIMIT
        );
        let escaped = text(&"\n".repeat(MAX_TEXT_DOCUMENT_BYTES));
        assert!(serialized_size(&escaped, MAX_TEXT_DOCUMENT_BYTES).is_none());
        let size = serialized_size(&escaped, usize::MAX).unwrap();
        assert_eq!(serialized_size(&escaped, size), Some(size));
        assert_eq!(serialized_size(&escaped, size - 1), None);
    }

    #[test]
    fn broker_budget_counts_ordinary_history_and_keeps_envelope_reserve() {
        let full = text(&"x".repeat(MAX_TEXT_DOCUMENT_BYTES));
        assert!(validate_messages(
            "local/attachment-validator-budget-sa26",
            &[message(vec![full.clone(); 7])]
        )
        .is_ok());
        assert_eq!(
            validate_messages(
                "local/attachment-validator-budget-sa26",
                &[message(vec![full; 8])]
            )
            .unwrap_err(),
            BROKER_LIMIT
        );
        let history = [message(vec![
            text("small"),
            json!({"type":"text","text":"x".repeat(MAX_BROKER_PROXY_BYTES)}),
        ])];
        assert_eq!(
            validate_messages("local/attachment-validator-budget-sa26", &history).unwrap_err(),
            BROKER_LIMIT
        );
        assert!(validate_messages(NATIVE, &history).is_ok());
    }

    #[test]
    fn malformed_media_containers_fail_but_opaque_tool_inputs_are_not_walked() {
        let assistant = Arc::new(json!({"role":"assistant","content":[image()]}));
        assert!(validate_messages(NATIVE, &[assistant]).is_err());
        let single = Arc::new(json!({"role":"user","content":image()}));
        assert!(validate_messages(NATIVE, &[single]).is_err());
        for block in [
            json!({"type":"image_url","image_url":{"url":"https://example.invalid/image"}}),
            json!({"type":"input_file","file_id":"private-id"}),
            json!({"type":"tool_result","content":image()}),
            json!({"type":"tool_result","content":[{"type":"tool_result","content":[image()]}]}),
            json!([image()]),
        ] {
            assert!(check(NATIVE, block).is_err());
        }
        let opaque = Arc::new(json!({"role":"assistant","content":[
            {"type":"tool_use","id":"t","name":"example","input":{"type":"image","source":{"data":"not media"}}},
            {"type":"thinking","thinking":"ordinary reasoning","signature":"opaque"},
            {"type":"provider_extension","value":"opaque"}
        ]}));
        assert!(validate_messages("plugin:provider:model", &[opaque]).is_ok());
        assert!(
            validate_tool_blocks(NATIVE, &[json!({"type":"tool_result","content":[image()]})])
                .is_err()
        );
    }
}
