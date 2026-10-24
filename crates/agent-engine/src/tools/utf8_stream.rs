//! Decode a byte stream that arrives in arbitrary chunks as UTF-8.
//!
//! `String::from_utf8_lossy` on each chunk turns a character split between
//! two reads into U+FFFD (issue #286). This decoder holds back an incomplete
//! trailing sequence (at most 3 bytes) and prepends it to the next chunk.
//! Genuinely invalid bytes still become U+FFFD, exactly as `from_utf8_lossy`
//! would replace them.

#[derive(Debug, Default)]
pub(crate) struct Utf8StreamDecoder {
    pending: Vec<u8>,
}

impl Utf8StreamDecoder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Decode `chunk`, keeping an incomplete trailing sequence for next time.
    pub(crate) fn decode(&mut self, chunk: &[u8]) -> String {
        if self.pending.is_empty() {
            return self.decode_bytes(chunk);
        }
        let mut bytes = std::mem::take(&mut self.pending);
        bytes.extend_from_slice(chunk);
        self.decode_bytes(&bytes)
    }

    /// End of stream: anything still held back is invalid and becomes U+FFFD.
    pub(crate) fn finish(&mut self) -> String {
        let rest = std::mem::take(&mut self.pending);
        String::from_utf8_lossy(&rest).into_owned()
    }

    fn decode_bytes(&mut self, mut bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len());
        loop {
            match std::str::from_utf8(bytes) {
                Ok(valid) => {
                    out.push_str(valid);
                    return out;
                }
                Err(e) => {
                    let (valid, rest) = bytes.split_at(e.valid_up_to());
                    // `valid_up_to` marks a valid prefix, so this never fails.
                    out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                    match e.error_len() {
                        Some(len) => {
                            out.push('\u{FFFD}');
                            bytes = &rest[len..];
                        }
                        None => {
                            // Unexpected end: a prefix of a valid sequence.
                            self.pending.extend_from_slice(rest);
                            return out;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(chunks: &[&[u8]]) -> String {
        let mut d = Utf8StreamDecoder::new();
        let mut out: String = chunks.iter().map(|c| d.decode(c)).collect();
        out.push_str(&d.finish());
        out
    }

    #[test]
    fn splits_at_every_offset_reassemble() {
        for text in ["é", "▀", "😀", "aé▀😀z", "日本語テキスト"] {
            let bytes = text.as_bytes();
            for split in 0..=bytes.len() {
                let out = decode_all(&[&bytes[..split], &bytes[split..]]);
                assert_eq!(out.as_bytes(), bytes, "{text:?} split at {split}");
            }
            let singles: Vec<&[u8]> = bytes.chunks(1).collect();
            assert_eq!(decode_all(&singles), text);
        }
    }

    #[test]
    fn holds_back_at_most_three_bytes() {
        let mut d = Utf8StreamDecoder::new();
        assert_eq!(d.decode(b"x\xf0\x9f\x98"), "x");
        assert_eq!(d.pending.len(), 3);
        assert_eq!(d.decode(b"\x80"), "😀");
        assert!(d.pending.is_empty());
    }

    #[test]
    fn matches_from_utf8_lossy_on_invalid_input() {
        let cases: &[&[u8]] = &[
            b"ok\xffok",
            b"\x80\x80",
            b"a\xe2Xb",
            b"\xf0\x9f\x98X",
            b"\xed\xa0\x80", // surrogate
            b"\xc0\xaf",     // overlong
            b"tail\xe2\x96",
            b"\xf4\x90\x80\x80", // above U+10FFFF
        ];
        for bytes in cases {
            let expected = String::from_utf8_lossy(bytes).into_owned();
            for split in 0..=bytes.len() {
                let out = decode_all(&[&bytes[..split], &bytes[split..]]);
                assert_eq!(out, expected, "{bytes:?} split at {split}");
            }
        }
    }

    #[test]
    fn incomplete_sequence_is_flushed_at_eof() {
        let mut d = Utf8StreamDecoder::new();
        assert_eq!(d.decode(b"ab\xe2\x96"), "ab");
        assert_eq!(d.finish(), "\u{FFFD}");
        assert_eq!(d.finish(), "");
    }
}
