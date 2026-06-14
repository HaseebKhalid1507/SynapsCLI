//! SSE line buffering — zero-copy line extraction from streamed byte chunks.
//!
//! SSE events arrive as `data: {...}\n` lines, but HTTP chunks split at
//! arbitrary byte offsets — mid-line, even mid-UTF-8-codepoint. This module
//! buffers raw bytes and yields complete lines without the double-copy the
//! old inline implementation paid (`to_vec()` + `from_utf8_lossy().to_string()`
//! per line — REVIEW.md P2).
//!
//! Usage:
//! ```ignore
//! let mut buf = SseLineBuffer::new();
//! buf.extend(&chunk);
//! while let Some(line) = buf.next_line() {
//!     if let Some(data) = line.strip_prefix("data: ") { ... }
//! }
//! // at stream end:
//! if let Some(rest) = buf.take_remaining() { ... }
//! ```

/// Buffers raw SSE bytes; yields complete `\n`-delimited lines as owned
/// Strings only when a full line is available. Handles CRLF, trailing
/// whitespace, and UTF-8 sequences split across chunk boundaries (bytes
/// stay buffered until their line completes, so codepoints can't tear).
pub(super) struct SseLineBuffer {
    buf: Vec<u8>,
    /// Read cursor — consumed lines advance this instead of draining the
    /// front of the buffer (O(1) per line instead of O(n) memmove).
    pos: usize,
}

impl SseLineBuffer {
    pub(super) fn new() -> Self {
        Self { buf: Vec::with_capacity(8192), pos: 0 }
    }

    /// Append a raw chunk from the network.
    pub(super) fn extend(&mut self, chunk: &[u8]) {
        // Compact consumed prefix before growing — keeps the buffer from
        // expanding unboundedly across a long stream.
        if self.pos > 0 && self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        } else if self.pos > 4096 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        self.buf.extend_from_slice(chunk);
    }

    /// Next complete line (without the trailing `\n`/`\r\n`), trimmed of
    /// trailing whitespace. Returns None when no full line is buffered.
    ///
    /// Returns `&str` borrowed from the internal buffer — zero allocation
    /// for the common case. Invalid UTF-8 lines are skipped (SSE payloads
    /// from the API are always valid UTF-8; torn codepoints can't occur
    /// because we only parse complete lines).
    pub(super) fn next_line(&mut self) -> Option<&str> {
        let start = self.pos;
        let rel = memchr::memchr(b'\n', &self.buf[start..])?;
        let end = start + rel;
        self.pos = end + 1;
        match std::str::from_utf8(&self.buf[start..end]) {
            Ok(s) => Some(s.trim_end()),
            Err(_) => Some(""), // skip torn line — yields harmless empty
        }
    }

    /// Drain any final partial line at stream end (no trailing newline).
    pub(super) fn take_remaining(&mut self) -> Option<String> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let rest = String::from_utf8_lossy(&self.buf[self.pos..]).trim().to_string();
        self.buf.clear();
        self.pos = 0;
        if rest.is_empty() { None } else { Some(rest) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `input` re-chunked at every possible split point; assert the
    /// emitted line sequence is identical regardless of chunking.
    fn lines_with_chunking(input: &[u8], split_at: usize) -> Vec<String> {
        let mut buf = SseLineBuffer::new();
        let mut out = Vec::new();
        let (a, b) = input.split_at(split_at.min(input.len()));
        for chunk in [a, b] {
            if chunk.is_empty() { continue; }
            buf.extend(chunk);
            while let Some(line) = buf.next_line() {
                out.push(line.to_string());
            }
        }
        if let Some(rest) = buf.take_remaining() {
            out.push(rest);
        }
        out
    }

    #[test]
    fn whole_lines_pass_through() {
        let mut buf = SseLineBuffer::new();
        buf.extend(b"data: {\"a\":1}\ndata: {\"b\":2}\n");
        assert_eq!(buf.next_line(), Some("data: {\"a\":1}"));
        assert_eq!(buf.next_line(), Some("data: {\"b\":2}"));
        assert_eq!(buf.next_line(), None);
    }

    #[test]
    fn line_split_across_chunks() {
        let mut buf = SseLineBuffer::new();
        buf.extend(b"data: {\"par");
        assert_eq!(buf.next_line(), None, "no complete line yet");
        buf.extend(b"tial\":true}\n");
        assert_eq!(buf.next_line(), Some("data: {\"partial\":true}"));
    }

    #[test]
    fn utf8_codepoint_split_across_chunks() {
        // '…' is E2 80 A6 — split between its bytes
        let input = "data: {\"t\":\"…\"}\n".as_bytes();
        for split in 0..input.len() {
            let lines = lines_with_chunking(input, split);
            assert_eq!(lines, vec!["data: {\"t\":\"…\"}"], "split at {split}");
        }
    }

    #[test]
    fn crlf_line_endings_trimmed() {
        let mut buf = SseLineBuffer::new();
        buf.extend(b"data: x\r\ndata: y\r\n");
        assert_eq!(buf.next_line(), Some("data: x"));
        assert_eq!(buf.next_line(), Some("data: y"));
    }

    #[test]
    fn empty_lines_and_comments() {
        let mut buf = SseLineBuffer::new();
        buf.extend(b"\n: keepalive\n\ndata: real\n");
        assert_eq!(buf.next_line(), Some(""));
        assert_eq!(buf.next_line(), Some(": keepalive"));
        assert_eq!(buf.next_line(), Some(""));
        assert_eq!(buf.next_line(), Some("data: real"));
    }

    #[test]
    fn remaining_partial_line_at_stream_end() {
        let mut buf = SseLineBuffer::new();
        buf.extend(b"data: complete\ndata: no-newline");
        assert_eq!(buf.next_line(), Some("data: complete"));
        assert_eq!(buf.next_line(), None);
        assert_eq!(buf.take_remaining(), Some("data: no-newline".to_string()));
        assert_eq!(buf.take_remaining(), None, "second take is empty");
    }

    #[test]
    fn exhaustive_rechunking_preserves_event_stream() {
        // Realistic SSE sample with multi-byte UTF-8, CRLF mix, keepalive
        let input = "event: message\ndata: {\"text\":\"héllo ✨\"}\r\n: ping\ndata: [DONE]\n".as_bytes();
        let baseline = lines_with_chunking(input, input.len());
        for split in 1..input.len() {
            assert_eq!(lines_with_chunking(input, split), baseline, "split at {split}");
        }
    }

    #[test]
    fn buffer_compaction_keeps_unread_bytes() {
        let mut buf = SseLineBuffer::new();
        // Push enough consumed data to trigger the pos > 4096 compaction
        for _ in 0..600 {
            buf.extend(b"data: x\n");
            assert_eq!(buf.next_line(), Some("data: x"));
        }
        // Now a split line across the compaction boundary
        buf.extend(b"data: tail-");
        buf.extend(b"end\n");
        assert_eq!(buf.next_line(), Some("data: tail-end"));
    }
}
