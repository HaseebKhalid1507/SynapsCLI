use super::{expand_path, Tool, ToolContext, ToolOutput};
use crate::{Result, RuntimeError};
use serde_json::{json, Value};

/// Raw-byte cap. 3.5 MiB × 4/3 base64 = 4.89 MB < Anthropic's 5 MB/image.
pub(crate) const MAX_IMAGE_BYTES: usize = 3_670_016;
/// Anthropic rejects any side > 8000 px.
const MAX_IMAGE_SIDE_PX: u32 = 8000;
/// Above this long edge Anthropic downscales server-side (coordinate caveat).
const PROVIDER_DOWNSCALE_LONG_EDGE: u32 = 1568;

pub struct ReadTool;

#[async_trait::async_trait]
impl Tool for ReadTool {
    fn effect(&self) -> crate::tools::catalog::ToolEffect {
        crate::tools::catalog::ToolEffect::ReadOnly
    }

    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Builtin
    }

    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Returns lines with line numbers. Reads up to 500 lines by default. For large files, use offset and limit to read in sections. Image files (PNG, JPEG, GIF, WebP) are returned as an image the model can see, up to 3.5 MB."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read"
                },
                "offset": {
                    "type": "integer",
                    "description": "Line number to start reading from (0-indexed, default: 0)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read (default: all lines)"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        self.execute_rich(params, ctx)
            .await
            .map(ToolOutput::into_summary)
    }

    async fn execute_rich(&self, params: Value, _ctx: ToolContext) -> Result<ToolOutput> {
        let raw_path = params["path"]
            .as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing path parameter".to_string()))?;
        let path = expand_path(raw_path);

        // Read raw bytes first to detect binary files
        let bytes = tokio::fs::read(&path).await.map_err(|e| {
            RuntimeError::Tool(format!("Failed to read file '{}': {}", path.display(), e))
        })?;

        if let Some(mime) = sniff_image_mime(&bytes) {
            return image_output(&path, mime, &bytes);
        }

        let content = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => return Err(RuntimeError::Tool(format!(
                "File '{}' appears to be binary (not valid UTF-8). Use `bash` with `xxd` or `file` to inspect binary files. Image files (png/jpg/gif/webp) are returned as images automatically.",
                path.display()
            ))),
        };

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        let offset = params["offset"].as_u64().unwrap_or(0) as usize;
        let limit = params["limit"]
            .as_u64()
            .map(|l| l as usize)
            .unwrap_or(500.min(total_lines));

        let start = offset.min(total_lines);
        let end = (start + limit).min(total_lines);

        let mut result = String::new();
        for (i, line) in lines[start..end].iter().enumerate() {
            result.push_str(&format!("{}\t{}\n", start + i + 1, line));
        }

        if total_lines > end {
            result.push_str(&format!("\n... ({} more lines)", total_lines - end));
        }

        Ok(ToolOutput::Text(result))
    }
}

/// Magic-byte sniff — content, never extension.
pub(crate) fn sniff_image_mime(b: &[u8]) -> Option<&'static str> {
    if b.len() >= 8 && b[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        return Some("image/png");
    }
    if b.len() >= 3 && b[..3] == [0xFF, 0xD8, 0xFF] {
        return Some("image/jpeg");
    }
    if b.len() >= 6 && (&b[..6] == b"GIF87a" || &b[..6] == b"GIF89a") {
        return Some("image/gif");
    }
    if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Header-only dimension parse. Best effort: `None` → label shows `?x?`.
/// Never a hard failure — dims are for the label and the 8000px guard only.
pub(crate) fn image_dimensions(mime: &str, b: &[u8]) -> Option<(u32, u32)> {
    match mime {
        "image/png" if b.len() >= 24 => Some((be32(&b[16..20]), be32(&b[20..24]))),
        "image/gif" if b.len() >= 10 => Some((le16(&b[6..8]) as u32, le16(&b[8..10]) as u32)),
        "image/webp" if b.len() >= 30 => match &b[12..16] {
            b"VP8 " => Some((
                (le16(&b[26..28]) & 0x3FFF) as u32,
                (le16(&b[28..30]) & 0x3FFF) as u32,
            )),
            b"VP8L" => {
                let x = [b[21], b[22], b[23], b[24]];
                Some((
                    1 + (((x[1] & 0x3F) as u32) << 8 | x[0] as u32),
                    1 + (((x[3] & 0x0F) as u32) << 10
                        | (x[2] as u32) << 2
                        | ((x[1] & 0xC0) as u32) >> 6),
                ))
            }
            b"VP8X" => Some((1 + le24(&b[24..27]), 1 + le24(&b[27..30]))),
            _ => None,
        },
        "image/jpeg" => jpeg_dimensions(b),
        _ => None,
    }
}

fn jpeg_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    // Walk segments from offset 2 until a SOFn marker (C0–CF except C4, C8, CC).
    let mut i = 2usize;
    while i + 4 <= b.len() {
        if b[i] != 0xFF {
            return None;
        }
        let m = b[i + 1];
        if m == 0xFF {
            i += 1; // fill byte
            continue;
        }
        if m == 0xD8 || m == 0x01 || (0xD0..=0xD7).contains(&m) {
            i += 2; // standalone marker
            continue;
        }
        if m == 0xD9 || m == 0xDA {
            return None; // EOI / SOS: no SOF found
        }
        let len = be16(&b[i + 2..i + 4]) as usize;
        if matches!(m, 0xC0..=0xCF) && !matches!(m, 0xC4 | 0xC8 | 0xCC) {
            if i + 9 > b.len() {
                return None;
            }
            // SOFn payload: Lf(2) P(1) Y(2) X(2) — height then width.
            return Some((be16(&b[i + 7..i + 9]) as u32, be16(&b[i + 5..i + 7]) as u32));
        }
        i += 2 + len;
    }
    None
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}
fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
fn le24(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], 0])
}

fn image_output(path: &std::path::Path, mime: &'static str, bytes: &[u8]) -> Result<ToolOutput> {
    use base64::Engine as _;
    let kb = bytes.len().div_ceil(1024);
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(RuntimeError::Tool(format!(
            "Image '{p}' is {kb} KB; the read tool accepts images up to {cap} KB. \
             Shrink it with bash and read the new file, e.g.:\n  \
             convert '{p}' -resize 1568x1568\\> -quality 85 /tmp/shrunk.jpg   (ImageMagick)\n  \
             sips -Z 1568 '{p}' --out /tmp/shrunk.png                        (macOS)",
            p = path.display(),
            kb = kb,
            cap = MAX_IMAGE_BYTES / 1024
        )));
    }
    let dims = image_dimensions(mime, bytes);
    if let Some((w, h)) = dims {
        if w > MAX_IMAGE_SIDE_PX || h > MAX_IMAGE_SIDE_PX {
            return Err(RuntimeError::Tool(format!(
                "Image '{}' is {w}x{h}; the provider rejects images with any side over {MAX_IMAGE_SIDE_PX} px. \
                 Downscale it with bash (e.g. `convert ... -resize 1568x1568\\>`) and read the new file.",
                path.display()
            )));
        }
    }
    let dims_label = dims
        .map(|(w, h)| format!("{w}x{h}"))
        .unwrap_or_else(|| "?x?".into());
    let mut summary = format!("Image: {} ({dims_label}, {mime}, {kb} KB)", path.display());
    if let Some((w, h)) = dims {
        if w.max(h) > PROVIDER_DOWNSCALE_LONG_EDGE {
            summary.push_str("\nNote: long edge exceeds 1568 px; the provider downscales before viewing, so pixel coordinates read from the image are approximate.");
        }
    }
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(ToolOutput::Blocks {
        blocks: vec![
            // FIRST — invariant: blocks[0] is text == summary.
            json!({ "type": "text", "text": summary }),
            json!({ "type": "image", "source": { "type": "base64", "media_type": mime, "data": data } }),
        ],
        summary,
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::create_tool_context;
    use super::*;
    use crate::tools::Tool;
    use serde_json::json;

    #[test]
    fn test_read_tool_schema() {
        let tool = ReadTool;
        assert_eq!(tool.name(), "read");
        assert!(!tool.description().is_empty());

        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"].is_object());
        assert!(params["required"].is_array());
    }

    #[tokio::test]
    async fn test_read_tool_execution() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("read_tool_test.txt");

        // Create temp file with known content
        let content = "line 1\nline 2\nline 3\nline 4\nline 5";
        std::fs::write(&test_file, content).unwrap();

        let tool = ReadTool;
        let ctx = create_tool_context();

        // Test basic read
        let params = json!({
            "path": test_file.to_string_lossy()
        });
        let result = tool.execute(params, ctx).await.unwrap();

        // Verify line numbers and content
        assert!(result.contains("1\tline 1"));
        assert!(result.contains("2\tline 2"));
        assert!(result.contains("5\tline 5"));

        // Test with offset and limit
        let ctx = create_tool_context();
        let params = json!({
            "path": test_file.to_string_lossy(),
            "offset": 2,
            "limit": 2
        });
        let result = tool.execute(params, ctx).await.unwrap();

        assert!(result.contains("3\tline 3"));
        assert!(result.contains("4\tline 4"));
        assert!(!result.contains("1\tline 1"));
        assert!(!result.contains("5\tline 5"));

        // Cleanup
        let _ = std::fs::remove_file(&test_file);
    }

    #[tokio::test]
    async fn test_read_tool_offset() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("test_read_tool_offset.txt");

        // Write 10 lines
        let lines = (1..=10).map(|i| format!("line {}", i)).collect::<Vec<_>>();
        let content = lines.join("\n");
        std::fs::write(&test_file, &content).unwrap();

        let tool = ReadTool;
        let ctx = create_tool_context();

        // Read with offset=5 (0-indexed, so starts at line 6)
        let params = json!({
            "path": test_file.to_string_lossy(),
            "offset": 5
        });

        let result = tool.execute(params, ctx).await.unwrap();

        // First line shown should be line 6 (1-indexed in output)
        assert!(result.contains("6\tline 6"));
        // Should not contain earlier lines
        assert!(!result.contains("1\tline 1"));
        assert!(!result.contains("5\tline 5"));

        // Cleanup
        let _ = std::fs::remove_file(&test_file);
    }

    // ── image support ───────────────────────────────────────────────────────

    const PNG_SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

    /// PNG signature + IHDR chunk header with the given dims (24 bytes total).
    fn png_header(w: u32, h: u32) -> Vec<u8> {
        let mut v = PNG_SIG.to_vec();
        v.extend_from_slice(&13u32.to_be_bytes()); // IHDR length
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v
    }

    /// Minimal 1×1 PNG: header + IHDR body tail + (fake) CRC.
    fn tiny_png() -> Vec<u8> {
        let mut v = png_header(1, 1);
        v.extend_from_slice(&[8, 6, 0, 0, 0]); // depth, color, comp, filter, interlace
        v.extend_from_slice(&[0, 0, 0, 0]); // crc (not validated)
        v
    }

    fn tmp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("read_img_{}_{name}", std::process::id()));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn sniff_png_jpeg_gif_webp() {
        assert_eq!(sniff_image_mime(&PNG_SIG), Some("image/png"));
        assert_eq!(
            sniff_image_mime(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some("image/jpeg")
        );
        assert_eq!(
            sniff_image_mime(b"GIF89a\x01\x00\x01\x00"),
            Some("image/gif")
        );
        assert_eq!(
            sniff_image_mime(b"GIF87a\x01\x00\x01\x00"),
            Some("image/gif")
        );
        assert_eq!(
            sniff_image_mime(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            Some("image/webp")
        );
        assert_eq!(sniff_image_mime(b"hello"), None);
        assert_eq!(
            sniff_image_mime(b"BM\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"),
            None
        );
        assert_eq!(sniff_image_mime(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3"), None);
        assert_eq!(sniff_image_mime(b"RIFF\x00\x00\x00\x00WAVEfmt "), None);
        assert_eq!(sniff_image_mime(b""), None);
    }

    #[tokio::test]
    async fn sniff_ignores_extension() {
        let text_as_png = tmp("notes.png", b"just text\nline two");
        let out = ReadTool
            .execute_rich(
                json!({"path": text_as_png.to_string_lossy()}),
                create_tool_context(),
            )
            .await
            .unwrap();
        match out {
            ToolOutput::Text(s) => assert!(s.contains("1\tjust text"), "{s}"),
            other => panic!("expected Text, got {other:?}"),
        }

        let png_as_txt = tmp("data.txt", &tiny_png());
        let out = ReadTool
            .execute_rich(
                json!({"path": png_as_txt.to_string_lossy()}),
                create_tool_context(),
            )
            .await
            .unwrap();
        assert!(matches!(out, ToolOutput::Blocks { .. }), "{out:?}");
        let _ = std::fs::remove_file(text_as_png);
        let _ = std::fs::remove_file(png_as_txt);
    }

    #[test]
    fn png_dimensions_from_ihdr() {
        assert_eq!(image_dimensions("image/png", &tiny_png()), Some((1, 1)));
        assert_eq!(
            image_dimensions("image/png", &png_header(1024, 1536)),
            Some((1024, 1536))
        );
        assert_eq!(image_dimensions("image/png", &PNG_SIG), None);
    }

    #[test]
    fn jpeg_dimensions_from_sof0() {
        let mut j = vec![0xFF, 0xD8]; // SOI
        j.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]); // APP0, len 16
        j.extend_from_slice(b"JFIF\0");
        j.extend_from_slice(&[1, 1, 0, 0, 1, 0, 1, 0, 0]); // 14 payload bytes total
        j.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]); // SOF0, len 17, precision
        j.extend_from_slice(&480u16.to_be_bytes()); // height
        j.extend_from_slice(&640u16.to_be_bytes()); // width
        j.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        assert_eq!(image_dimensions("image/jpeg", &j), Some((640, 480)));

        // SOI + SOS only → no SOF → None, no panic.
        let sos_only = [0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x08, 0, 0, 0, 0, 0, 0];
        assert_eq!(image_dimensions("image/jpeg", &sos_only), None);

        // Truncated SOF header → None, no panic.
        let trunc = [0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01];
        assert_eq!(image_dimensions("image/jpeg", &trunc), None);

        // Garbage after SOI: deterministic pseudo-random bytes, must never panic.
        let mut seed = 0x9E37_79B9u32;
        for _ in 0..200 {
            let mut buf = vec![0xFF, 0xD8, 0xFF];
            for _ in 0..64 {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                buf.push((seed & 0xFF) as u8);
            }
            let _ = image_dimensions("image/jpeg", &buf);
        }
    }

    #[test]
    fn webp_dimensions_vp8_vp8l_vp8x() {
        // VP8 (lossy): frame tag (3) + start code 9D 01 2A + w(2) + h(2)
        let mut vp8 = b"RIFF\x00\x00\x00\x00WEBPVP8 \x00\x00\x00\x00".to_vec();
        vp8.extend_from_slice(&[0x00, 0x00, 0x00, 0x9D, 0x01, 0x2A]);
        vp8.extend_from_slice(&320u16.to_le_bytes());
        vp8.extend_from_slice(&240u16.to_le_bytes());
        assert_eq!(image_dimensions("image/webp", &vp8), Some((320, 240)));

        // VP8L (lossless): signature 0x2F then 14-bit w-1, 14-bit h-1.
        let (w, h) = (100u32, 50u32);
        let bits: u32 = (w - 1) | ((h - 1) << 14);
        let mut vp8l = b"RIFF\x00\x00\x00\x00WEBPVP8L\x00\x00\x00\x00\x2F".to_vec();
        vp8l.extend_from_slice(&bits.to_le_bytes());
        vp8l.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        assert_eq!(image_dimensions("image/webp", &vp8l), Some((100, 50)));

        // VP8X (extended): flags(4) + 24-bit w-1 + 24-bit h-1.
        let mut vp8x = b"RIFF\x00\x00\x00\x00WEBPVP8X\x0A\x00\x00\x00".to_vec();
        vp8x.extend_from_slice(&[0, 0, 0, 0]);
        vp8x.extend_from_slice(&(1999u32).to_le_bytes()[..3]);
        vp8x.extend_from_slice(&(999u32).to_le_bytes()[..3]);
        assert_eq!(image_dimensions("image/webp", &vp8x), Some((2000, 1000)));
    }

    #[tokio::test]
    async fn image_read_block_ordering() {
        use base64::Engine as _;
        let bytes = tiny_png();
        let p = tmp("order.png", &bytes);
        let out = ReadTool
            .execute_rich(json!({"path": p.to_string_lossy()}), create_tool_context())
            .await
            .unwrap();
        let ToolOutput::Blocks { blocks, summary } = out else {
            panic!("expected Blocks");
        };
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], summary);
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["type"], "base64");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(blocks[1]["source"]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, bytes);
        let _ = std::fs::remove_file(p);
    }

    #[tokio::test]
    async fn image_over_cap_rejected() {
        let mut bytes = png_header(10, 10);
        bytes.resize(MAX_IMAGE_BYTES + 1, 0);
        let p = tmp("big.png", &bytes);
        let err = ReadTool
            .execute_rich(json!({"path": p.to_string_lossy()}), create_tool_context())
            .await
            .unwrap_err();
        let RuntimeError::Tool(msg) = err else {
            panic!("expected Tool error, got {err:?}");
        };
        assert!(msg.contains("convert"), "{msg}");
        assert!(msg.contains("KB"), "{msg}");
        assert!(msg.len() < 1024, "{}", msg.len());
        let _ = std::fs::remove_file(p);
    }

    #[tokio::test]
    async fn image_over_8000px_rejected() {
        let p = tmp("wide.png", &png_header(8001, 10));
        let err = ReadTool
            .execute_rich(json!({"path": p.to_string_lossy()}), create_tool_context())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("8000"), "{err}");
        let _ = std::fs::remove_file(p);
    }

    #[tokio::test]
    async fn legacy_execute_returns_summary_for_images() {
        let p = tmp("legacy.png", &tiny_png());
        let s = ReadTool
            .execute(json!({"path": p.to_string_lossy()}), create_tool_context())
            .await
            .unwrap();
        assert!(s.starts_with("Image: "), "{s}");
        assert!(s.len() < 512);
        let _ = std::fs::remove_file(p);
    }

    #[tokio::test]
    async fn label_format() {
        let p = tmp("label.png", &tiny_png());
        let out = ReadTool
            .execute_rich(json!({"path": p.to_string_lossy()}), create_tool_context())
            .await
            .unwrap();
        assert_eq!(
            out.summary(),
            format!("Image: {} (1x1, image/png, 1 KB)", p.display())
        );
        let _ = std::fs::remove_file(p);

        let p = tmp("wide2.png", &png_header(2000, 100));
        let out = ReadTool
            .execute_rich(json!({"path": p.to_string_lossy()}), create_tool_context())
            .await
            .unwrap();
        assert!(out.summary().starts_with(&format!(
            "Image: {} (2000x100, image/png, 1 KB)",
            p.display()
        )));
        assert!(out.summary().contains("1568 px"), "{}", out.summary());
        let _ = std::fs::remove_file(p);
    }

    /// Spec §5.7 scripted integration check against the real test asset.
    /// Skips (passes) if the asset is absent so CI without ~/Jawz stays green.
    #[tokio::test]
    async fn real_avatar_png_round_trips_as_image_blocks() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let p = std::path::PathBuf::from(home).join("Jawz/media/jawz-avatar-v1.png");
        if !p.exists() {
            eprintln!("skip: {} absent", p.display());
            return;
        }
        let out = ReadTool
            .execute_rich(json!({"path": p.to_string_lossy()}), create_tool_context())
            .await
            .unwrap();
        let ToolOutput::Blocks { blocks, summary } = out else {
            panic!("expected Blocks");
        };
        assert!(
            summary.starts_with(&format!(
                "Image: {} (1024x1536, image/png, 2292 KB)",
                p.display()
            )),
            "{summary}"
        );
        assert_eq!(blocks[0]["text"], summary);
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
        let data = blocks[1]["source"]["data"].as_str().unwrap();
        assert!(data.starts_with("iVBORw0KGgo"));
        assert!(serde_json::to_vec(&blocks[1]).unwrap().len() < 5_000_000);
        eprintln!(
            "INTEGRATION summary={summary:?} base64[..40]={}",
            &data[..40]
        );
    }

    /// Integration evidence: a real .rs file still reads as numbered text,
    /// and (when `SYNAPS_TEST_BIG_IMAGE` points at a >3.5 MiB image) the
    /// oversize path rejects in-slot with the shrink hint and no base64.
    #[tokio::test]
    async fn real_rs_file_and_optional_big_image() {
        let out = ReadTool
            .execute_rich(
                json!({"path": concat!(env!("CARGO_MANIFEST_DIR"), "/src/tools/read.rs"), "limit": 3}),
                create_tool_context(),
            )
            .await
            .unwrap();
        let ToolOutput::Text(text) = out else { panic!("expected Text") };
        assert!(text.starts_with("1\tuse super::"), "{text}");
        eprintln!("INTEGRATION rs_read_first_line={:?}", text.lines().next().unwrap());

        if let Ok(big) = std::env::var("SYNAPS_TEST_BIG_IMAGE") {
            let err = ReadTool
                .execute_rich(json!({"path": big}), create_tool_context())
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("accepts images up to 3584 KB"), "{err}");
            assert!(err.contains("convert"));
            assert!(err.len() < 1024);
            eprintln!("INTEGRATION big_image_rejected={err:?}");
        }
    }

    #[tokio::test]
    async fn non_image_binary_mentions_image_support() {
        let p = tmp("blob.bin", &[0xFF, 0xFE, 0x00, 0x80, 0x81]);
        let err = ReadTool
            .execute_rich(json!({"path": p.to_string_lossy()}), create_tool_context())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("returned as images automatically"),
            "{err}"
        );
        let _ = std::fs::remove_file(p);
    }
}
