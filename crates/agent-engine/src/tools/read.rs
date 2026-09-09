use super::{resolve_path_in, Tool, ToolContext, ToolOutput};
use crate::{Result, RuntimeError};
use serde_json::{json, Value};

/// Raw-byte cap. 3.5 MiB × 4/3 base64 = 4.89 MB < Anthropic's 5 MB/image.
pub(crate) const MAX_IMAGE_BYTES: usize = 3_670_016;
/// Anthropic rejects any side > 8000 px.
const MAX_IMAGE_SIDE_PX: u32 = 8000;
/// Above this long edge Anthropic downscales server-side (coordinate caveat).
const PROVIDER_DOWNSCALE_LONG_EDGE: u32 = 1568;
/// Seatbelt: refuse to load anything larger than this into memory at all.
/// Checked via `fs::metadata` BEFORE the read, so a 200 MB `.tif` or a
/// device file never gets pulled in.
pub(crate) const MAX_READ_BYTES: u64 = 64 * 1024 * 1024;

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

    async fn execute_rich(&self, params: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let raw_path = params["path"]
            .as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing path parameter".to_string()))?;
        let path = resolve_path_in(raw_path, ctx.capabilities.cwd.as_deref());

        // Size guard BEFORE reading: one stat, no bytes loaded.
        let meta = tokio::fs::metadata(&path).await.map_err(|e| {
            RuntimeError::Tool(format!("Failed to read file '{}': {}", path.display(), e))
        })?;
        if !meta.is_file() {
            return Err(RuntimeError::Tool(format!(
                "'{}' is not a regular file. Use `bash` (ls, file, head) to inspect it.",
                path.display()
            )));
        }
        if meta.len() > MAX_READ_BYTES {
            return Err(RuntimeError::Tool(format!(
                "File '{}' is {} KB; the read tool refuses files over {} KB. Use `bash` with `head`, `tail`, `xxd`, or `sed -n` to read a slice, or shrink an image with `convert`.",
                path.display(),
                meta.len().div_ceil(1024),
                MAX_READ_BYTES / 1024
            )));
        }

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

/// Header-only dimension parse. `None` → `image_output` rejects the file
/// (an unsized image bypasses the 8000 px guard → provider 400 → poison pill).
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
fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Cheap structural integrity check — trailer/size sanity only, no decode.
/// A truncated or corrupt image shipped to the provider is a poison pill:
/// the 400 repeats on every request until the session is cleared, because
/// a `tool_result` user message always survives history repair. Reject here.
pub(crate) fn image_integrity_error(mime: &str, b: &[u8]) -> Option<&'static str> {
    match mime {
        "image/png" => {
            if b.len() < 16 || &b[12..16] != b"IHDR" {
                return Some("first chunk is not IHDR");
            }
            if !b.ends_with(b"IEND\xAE\x42\x60\x82") {
                return Some("missing IEND trailer (truncated?)");
            }
        }
        "image/jpeg" if !b.ends_with(&[0xFF, 0xD9]) => {
            return Some("missing EOI marker (truncated?)");
        }
        "image/gif" if !b.ends_with(&[0x3B]) => {
            return Some("missing GIF trailer (truncated?)");
        }
        "image/webp" => {
            let riff = le32(&b[4..8]) as usize;
            // RIFF size = file length − 8; allow a single pad byte of slack.
            if riff + 8 != b.len() && riff + 9 != b.len() {
                return Some("RIFF size does not match file length (truncated?)");
            }
        }
        _ => {}
    }
    None
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
    if let Some(why) = image_integrity_error(mime, bytes) {
        return Err(RuntimeError::Tool(format!(
            "Image '{}' ({mime}, {kb} KB) appears truncated or corrupt: {why}. \
             Not sent to the model. Verify with `file` / `identify`, or re-export it and read the new file.",
            path.display()
        )));
    }
    // Unknown dims → reject. Shipping an image we cannot size means the
    // 8000 px guard is bypassed and a provider 400 poisons the session.
    let Some((w, h)) = image_dimensions(mime, bytes) else {
        return Err(RuntimeError::Tool(format!(
            "Image '{}' ({mime}, {kb} KB): could not parse dimensions from the header. \
             Not sent to the model. Re-export it (e.g. `convert '{}' /tmp/fixed.png`) and read the new file.",
            path.display(),
            path.display()
        )));
    };
    // Zero-sized images are invalid for every supported format and are
    // rejected by providers — same poison-pill class as unknown dims.
    if w == 0 || h == 0 {
        return Err(RuntimeError::Tool(format!(
            "Image '{}' ({mime}, {kb} KB) declares invalid dimensions {w}x{h} (zero-sized).              Not sent to the model. Re-export it and read the new file.",
            path.display()
        )));
    }
    if w > MAX_IMAGE_SIDE_PX || h > MAX_IMAGE_SIDE_PX {
        return Err(RuntimeError::Tool(format!(
            "Image '{}' is {w}x{h}; the provider rejects images with any side over {MAX_IMAGE_SIDE_PX} px. \
             Downscale it with bash (e.g. `convert ... -resize 1568x1568\\>`) and read the new file.",
            path.display()
        )));
    }
    let mut summary = format!("Image: {} ({w}x{h}, {mime}, {kb} KB)", path.display());
    if w.max(h) > PROVIDER_DOWNSCALE_LONG_EDGE {
        summary.push_str("\nNote: long edge exceeds 1568 px; the provider downscales before viewing, so pixel coordinates read from the image are approximate.");
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

    const PNG_IEND: [u8; 12] = [0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82];

    /// Structurally complete PNG with the given dims: IHDR + IEND, no IDAT.
    fn png_with_dims(w: u32, h: u32) -> Vec<u8> {
        let mut v = png_header(w, h);
        v.extend_from_slice(&[8, 6, 0, 0, 0]); // depth, color, comp, filter, interlace
        v.extend_from_slice(&[0, 0, 0, 0]); // crc (not validated)
        v.extend_from_slice(&PNG_IEND);
        v
    }

    /// Minimal 1×1 PNG.
    fn tiny_png() -> Vec<u8> {
        png_with_dims(1, 1)
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
        let p = tmp("wide.png", &png_with_dims(8001, 10));
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

        let p = tmp("wide2.png", &png_with_dims(2000, 100));
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
            eprintln!(
                "SKIPPED real_avatar_png_round_trips_as_image_blocks: {} absent",
                p.display()
            );
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
        let ToolOutput::Text(text) = out else {
            panic!("expected Text")
        };
        assert!(text.starts_with("1\tuse super::"), "{text}");

        let Ok(big) = std::env::var("SYNAPS_TEST_BIG_IMAGE") else {
            eprintln!("SKIPPED big-image half of real_rs_file_and_optional_big_image: SYNAPS_TEST_BIG_IMAGE unset");
            return;
        };
        let err = ReadTool
            .execute_rich(json!({"path": big}), create_tool_context())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("accepts images up to 3584 KB"), "{err}");
        assert!(err.contains("convert"));
        assert!(err.len() < 1024);
    }

    // ── S2: poison-pill integrity checks ────────────────────────────────────

    #[test]
    fn integrity_check_per_format() {
        // PNG
        assert_eq!(image_integrity_error("image/png", &tiny_png()), None);
        assert!(image_integrity_error("image/png", &png_header(1, 1)).is_some()); // no IEND
        let mut bad_chunk = tiny_png();
        bad_chunk[12..16].copy_from_slice(b"iTXt");
        assert!(image_integrity_error("image/png", &bad_chunk).is_some());
        assert!(image_integrity_error("image/png", &PNG_SIG).is_some());
        // JPEG
        assert_eq!(
            image_integrity_error("image/jpeg", &[0xFF, 0xD8, 0xFF, 0xD9]),
            None
        );
        assert!(image_integrity_error("image/jpeg", &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]).is_some());
        // GIF
        assert_eq!(
            image_integrity_error("image/gif", b"GIF89a\x01\x00\x01\x00\x3B"),
            None
        );
        assert!(image_integrity_error("image/gif", b"GIF89a\x01\x00\x01\x00").is_some());
        // WebP: RIFF size must equal len - 8.
        let mut webp = b"RIFF\x00\x00\x00\x00WEBPVP8 ".to_vec();
        webp.extend_from_slice(&[0u8; 20]);
        assert!(image_integrity_error("image/webp", &webp).is_some());
        let riff = (webp.len() - 8) as u32;
        webp[4..8].copy_from_slice(&riff.to_le_bytes());
        assert_eq!(image_integrity_error("image/webp", &webp), None);
        webp.pop();
        assert!(image_integrity_error("image/webp", &webp).is_some());
    }

    #[tokio::test]
    async fn truncated_png_rejected_no_image_block() {
        // Header + 3 KB of zeros, no IEND — the "half-written screenshot" case.
        let mut bytes = png_header(10, 10);
        bytes.resize(3_000, 0);
        let p = tmp("trunc.png", &bytes);
        let err = ReadTool
            .execute_rich(json!({"path": p.to_string_lossy()}), create_tool_context())
            .await
            .unwrap_err();
        let RuntimeError::Tool(msg) = err else {
            panic!("expected Tool error, got {err:?}");
        };
        assert!(msg.contains("truncated or corrupt"), "{msg}");
        assert!(msg.contains("IEND"), "{msg}");
        assert!(!msg.contains("iVBORw0KGgo"), "no base64 in error");
        assert!(msg.len() < 1024);
        // Legacy path: same rejection, plain string, no image anywhere.
        let s = ReadTool
            .execute(json!({"path": p.to_string_lossy()}), create_tool_context())
            .await
            .unwrap_err()
            .to_string();
        assert!(s.contains("truncated or corrupt"), "{s}");
        let _ = std::fs::remove_file(p);
    }

    #[tokio::test]
    async fn truncated_jpeg_and_gif_rejected() {
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0,
            0,
        ];
        let p = tmp("trunc.jpg", &jpeg);
        let err = ReadTool
            .execute_rich(json!({"path": p.to_string_lossy()}), create_tool_context())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("EOI"), "{err}");
        let _ = std::fs::remove_file(p);

        let p = tmp("trunc.gif", b"GIF89a\x01\x00\x01\x00\x00\x00\x00");
        let err = ReadTool
            .execute_rich(json!({"path": p.to_string_lossy()}), create_tool_context())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("GIF trailer"), "{err}");
        let _ = std::fs::remove_file(p);
    }

    #[tokio::test]
    async fn unparseable_dims_rejected_not_shipped() {
        // Structurally complete JPEG (SOI … EOI) but SOS before any SOF →
        // dims None → must NOT become an image block.
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x08, 0, 0, 0, 0, 0, 0, 0xFF, 0xD9,
        ];
        let p = tmp("nodims.jpg", &jpeg);
        let err = ReadTool
            .execute_rich(json!({"path": p.to_string_lossy()}), create_tool_context())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("could not parse dimensions"), "{err}");
        assert!(err.contains("Not sent to the model"), "{err}");
        let _ = std::fs::remove_file(p);
    }

    #[tokio::test]
    async fn zero_dimension_images_rejected_not_shipped() {
        for (name, w, h) in [
            ("w0.png", 0u32, 7u32),
            ("h0.png", 7, 0),
            ("both0.png", 0, 0),
        ] {
            let p = tmp(name, &png_with_dims(w, h));
            let err = ReadTool
                .execute_rich(json!({"path": p.to_string_lossy()}), create_tool_context())
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("zero-sized"), "{name}: {err}");
            assert!(err.contains(&format!("{w}x{h}")), "{name}: {err}");
            assert!(err.contains("Not sent to the model"), "{name}: {err}");
            let _ = std::fs::remove_file(p);
        }
    }

    #[tokio::test]
    async fn complete_gif_and_jpeg_round_trip_through_execute_rich() {
        let p = tmp("ok.gif", b"GIF89a\x02\x00\x03\x00\x00\x00\x00\x3B");
        let out = ReadTool
            .execute_rich(json!({"path": p.to_string_lossy()}), create_tool_context())
            .await
            .unwrap();
        assert!(
            out.summary().contains("(2x3, image/gif, 1 KB)"),
            "{}",
            out.summary()
        );
        assert!(matches!(out, ToolOutput::Blocks { .. }));
        let _ = std::fs::remove_file(p);

        let mut j = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08];
        j.extend_from_slice(&480u16.to_be_bytes());
        j.extend_from_slice(&640u16.to_be_bytes());
        j.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1, 0xFF, 0xD9]);
        let p = tmp("ok.jpg", &j);
        let out = ReadTool
            .execute_rich(json!({"path": p.to_string_lossy()}), create_tool_context())
            .await
            .unwrap();
        assert!(
            out.summary().contains("(640x480, image/jpeg, 1 KB)"),
            "{}",
            out.summary()
        );
        let _ = std::fs::remove_file(p);
    }

    // ── S4: metadata guard before the read ──────────────────────────────────

    #[tokio::test]
    async fn oversize_file_rejected_by_metadata_without_reading() {
        // Sparse file: set_len allocates no blocks, so this is instant on
        // disk but would be a 65 MiB read if the guard ran after the read.
        let p = std::env::temp_dir().join(format!("read_sparse_{}.bin", std::process::id()));
        let f = std::fs::File::create(&p).unwrap();
        f.set_len(MAX_READ_BYTES + 1).unwrap();
        drop(f);
        let start = std::time::Instant::now();
        let err = ReadTool
            .execute_rich(json!({"path": p.to_string_lossy()}), create_tool_context())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("refuses files over"), "{err}");
        assert!(err.contains("head"), "{err}");
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        let _ = std::fs::remove_file(p);
    }

    #[tokio::test]
    async fn directory_and_device_rejected_as_not_regular_file() {
        let err = ReadTool
            .execute_rich(
                json!({"path": std::env::temp_dir().to_string_lossy()}),
                create_tool_context(),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a regular file"), "{err}");

        #[cfg(unix)]
        if std::path::Path::new("/dev/zero").exists() {
            let start = std::time::Instant::now();
            let err = ReadTool
                .execute_rich(json!({"path": "/dev/zero"}), create_tool_context())
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("not a regular file"), "{err}");
            assert!(start.elapsed() < std::time::Duration::from_secs(2));
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
