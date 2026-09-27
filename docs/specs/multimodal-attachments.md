# Multimodal attachment implementation

## Scope
- Real user-selected local attachments in TUI (`/attach PATH`, `/attachments`, `/detach`) and headless chat; RPC existing path attachments become actual content, never just path hints.
- Reuse Anthropic canonical user content: image/source base64; document/source base64 application/pdf; document/source text text/plain with title filename. All attachment content is lower-authority data.
- Bounded reads of regular files, image validation reused from read tool, content-based MIME detection, UTF8 text only for non-image/non-PDF. No URL fetching, remote file IDs, arbitrary binary, directories/devices, multipart upload service, clipboard image decoding, automatic path harvesting, or automatic paid live calls.
- Preserve inline bytes in existing private session JSONL so resume needs no original files. Context archive/history capture continues to whitelist text/tool text, excluding media/document source bytes; traces metadata only. Text documents lower to text only at wire boundary, not canonical memory.
- OpenAI chat: multipart image_url data URIs, supported file wire if capability allows; Responses/Codex: input_image and input_file, text documents as input_text. Tool media must become associated user media after all sibling tool results; never base64 in tool text, never drop silently.
- Anthropic native image/PDF/text documents pass through. Other not-yet-supported native/cloud/extension transports fail closed for binary media. UTF8 documents may be rejected explicitly where no adapter yet; no silent fallback.
- Model+transport checks: parse actual Codex input_modalities, exact-model capability cache first and evidence-backed static image rows (observed official local Codex cache 2026-09-05). Astra text+image; spark text only; no Codex PDF fallback. Generic compatible models require advertised modalities. Known Anthropic models support images/PDF. No name-substring guesses.
- Revalidate all outgoing media before request to catch resume and model switch. Unsupported rich tool outputs become explicit text tool errors rather than poisoning history. Bounds on decoded size, count and total bytes; privacy-safe errors, no payload logs.

## Verification
- Loader MIME/invalid/oversize/regular-file bounds + pending queue atomic behavior.
- Exact serializer tests chat and Responses including multiple tool results, mixed assistant text+tool calls, images, PDF and text files, unsupported capability errors.
- Metadata parser + capability cache/static evidence tests, unsupported routes, prompt/resume/model switch paths.
- Privacy archive sentinel regressions for image/PDF/text documents and session roundtrip.
- Workspace test suite, clippy with max8 total compile/test workers. No binary install until user requests.
