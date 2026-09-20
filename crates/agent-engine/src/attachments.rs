//! Explicit local attachment ingestion. No URL fetches or implicit path expansion
//! in prompts. Bytes are captured once and retained in private session history;
//! archive/memory projections deliberately omit image/document source payloads.
use base64::Engine;
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const MAX_ATTACHMENTS: usize = 8;
pub const MAX_PDF_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAX_TOTAL_BYTES: usize = 15 * 1024 * 1024;

#[derive(Clone)]
pub struct LoadedAttachment {
    block: Value,
    summary: String,
    bytes: usize,
}
impl std::fmt::Debug for LoadedAttachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedAttachment")
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

#[derive(Default, Debug)]
pub struct PendingAttachments {
    items: Vec<LoadedAttachment>,
}
impl PendingAttachments {
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    pub fn clear(&mut self) {
        self.items.clear();
    }
    pub fn summaries(&self) -> Vec<String> {
        self.items.iter().map(|a| a.summary.clone()).collect()
    }
    pub fn add(&mut self, attachment: LoadedAttachment) -> Result<(), String> {
        if self.len() >= MAX_ATTACHMENTS {
            return Err("At most 8 attachments may be staged".into());
        }
        if self.items.iter().map(|a| a.bytes).sum::<usize>() + attachment.bytes > MAX_TOTAL_BYTES {
            return Err("Attachments exceed the 15 MiB combined raw-byte limit".into());
        }
        self.items.push(attachment);
        Ok(())
    }
    pub fn build_content(&self, text: &str) -> Value {
        if self.is_empty() {
            return Value::String(text.into());
        }
        let mut blocks = Vec::new();
        if !text.is_empty() {
            blocks.push(json!({"type":"text", "text": text}));
        }
        blocks.extend(self.items.iter().map(|a| a.block.clone()));
        Value::Array(blocks)
    }
}

/// Asynchronous bounded read on an opened regular-file handle. On Unix,
/// O_NONBLOCK avoids FIFO-open hangs and O_NOFOLLOW refuses symlink leaves.
/// Parent symlinks are allowed: selection of a local path is explicit consent.
pub async fn load_attachment(path: &Path) -> Result<LoadedAttachment, String> {
    let path = crate::tools::expand_path(&path.to_string_lossy());
    tokio::task::spawn_blocking(move || load_blocking(&path))
        .await
        .map_err(|_| "Attachment reader failed".to_string())?
}

pub async fn build_user_content(text: &str, paths: &[PathBuf]) -> Result<Value, String> {
    if paths.len() > MAX_ATTACHMENTS {
        return Err("At most 8 attachments are allowed".into());
    }
    let mut pending = PendingAttachments::default();
    for path in paths {
        pending.add(load_attachment(path).await?)?;
    }
    Ok(pending.build_content(text))
}

fn load_blocking(path: &Path) -> Result<LoadedAttachment, String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = options.open(path).map_err(|_| {
        "Cannot open attachment (check path, permissions, and symlinks)".to_string()
    })?;
    let metadata = file
        .metadata()
        .map_err(|_| "Cannot inspect attachment".to_string())?;
    if !metadata.is_file() {
        return Err("Attachment must be a regular file".into());
    }
    if metadata.len() > MAX_PDF_BYTES as u64 {
        return Err("Attachment exceeds the 10 MiB file limit".into());
    }
    let mut bytes = Vec::with_capacity((metadata.len() as usize).min(MAX_PDF_BYTES));
    file.take((MAX_PDF_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read attachment".to_string())?;
    if bytes.len() > MAX_PDF_BYTES {
        return Err("Attachment grew beyond the 10 MiB file limit".into());
    }
    if bytes.is_empty() {
        return Err("Attachment is empty".into());
    }
    attachment_from_bytes(path, &bytes)
}

fn attachment_from_bytes(path: &Path, bytes: &[u8]) -> Result<LoadedAttachment, String> {
    // Never copy absolute paths or control sequences into provider filenames.
    let name: String = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '/' | '\\'))
        .take(128)
        .collect();
    let mut name = if name.is_empty() {
        "attachment".to_string()
    } else {
        name
    };
    while name.len() > 255 {
        name.pop();
    }
    let (block, kind) = if let Some(mime) = crate::tools::read::sniff_image_mime(bytes) {
        // Reuse read's size, structural-integrity and dimension guards. Its
        // detailed error has a local path, so surface only a bounded generic error.
        let output = crate::tools::read::image_output(Path::new(&name), mime, bytes)
            .map_err(|_| "Invalid or oversized image: use valid PNG/JPEG/GIF/WebP, at most 3.5 MiB and 8000 pixels per side".to_string())?;
        let (_, blocks) = output.into_parts();
        let image = blocks
            .and_then(|b| b.into_iter().find(|b| b["type"] == "image"))
            .ok_or_else(|| "Image loader returned no image".to_string())?;
        (image, mime)
    } else if bytes.starts_with(b"%PDF-") {
        if bytes.len() > MAX_PDF_BYTES || !bytes.windows(5).rev().take(1024).any(|w| w == b"%%EOF")
        {
            return Err("Invalid or oversized PDF (10 MiB maximum)".into());
        }
        (
            json!({"type":"document", "title":name, "source":{"type":"base64", "media_type":"application/pdf", "data":base64::engine::general_purpose::STANDARD.encode(bytes)}}),
            "application/pdf",
        )
    } else {
        if bytes.len() > MAX_TEXT_BYTES {
            return Err("Text attachment exceeds 256 KiB; select a smaller file".into());
        }
        let text = std::str::from_utf8(bytes).map_err(|_| {
            "Unsupported binary attachment; use an image, PDF, or UTF-8 text file".to_string()
        })?;
        if text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        {
            return Err("Attachment is not plain UTF-8 text".into());
        }
        (
            json!({"type":"document", "title":name, "source":{"type":"text", "media_type":"text/plain", "data":text}}),
            "text/plain",
        )
    };
    Ok(LoadedAttachment {
        block,
        summary: format!("{name} ({kind}, {} bytes)", bytes.len()),
        bytes: bytes.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn text_file_bytes_not_path_and_atomic_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.txt");
        std::fs::write(&path, "unique file body").unwrap();
        let content = build_user_content("question", std::slice::from_ref(&path))
            .await
            .unwrap();
        assert_eq!(content[0]["text"], "question");
        assert_eq!(content[1]["source"]["data"], "unique file body");
        assert_eq!(content[1]["title"], "sample.txt");
        assert!(!content.to_string().contains(dir.path().to_str().unwrap()));
        assert!(build_user_content("q", &[path, dir.path().join("missing")])
            .await
            .is_err());
    }
    #[test]
    fn bounds_mime_and_redacted_debug() {
        assert!(attachment_from_bytes(Path::new("a.txt"), &[0, 1]).is_err());
        assert!(
            attachment_from_bytes(Path::new("a.txt"), &vec![b'x'; MAX_TEXT_BYTES + 1]).is_err()
        );
        let pdf = attachment_from_bytes(Path::new("wrong.txt"), b"%PDF-1.7\n%%EOF\n").unwrap();
        assert_eq!(pdf.block["source"]["media_type"], "application/pdf");
        assert!(!format!("{pdf:?}").contains("PDF"));
        assert!(attachment_from_bytes(Path::new("a.pdf"), b"%PDF-1.7\nmissing trailer").is_err());
        let mut pending = PendingAttachments::default();
        for _ in 0..8 {
            pending.add(pdf.clone()).unwrap();
        }
        assert!(pending.add(pdf).is_err());
        assert_eq!(pending.len(), 8);
        pending.clear();
        assert_eq!(pending.build_content("hello"), "hello");
    }
    #[tokio::test]
    async fn rejects_directory_and_oversize_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_attachment(dir.path()).await.is_err());
        let path = dir.path().join("large");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_PDF_BYTES as u64 + 1)
            .unwrap();
        assert!(load_attachment(&path).await.is_err());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_and_fifo_without_blocking() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("text");
        std::fs::write(&path, "text").unwrap();
        let link = dir.path().join("link");
        symlink(&path, &link).unwrap();
        assert!(load_attachment(&link).await.is_err());
        let fifo = dir.path().join("fifo");
        let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), load_attachment(&fifo))
                .await
                .unwrap()
                .is_err()
        );
    }
}
