//! Incremental UTF-8 decoding for PTY output.
//!
//! The TypeScript service spawns `node-pty` with `encoding: "utf8"`, so `onData`
//! delivers a **JavaScript string** — the `onDynamicData` wire payload is therefore a
//! string (tag 1), not base64 and not `WireValue::Bytes` (that tag is for binary *return
//! values* such as `file.readFileRange`). A PTY hands us raw bytes whose multibyte
//! sequences can straddle two reads, so decoding must be incremental: a sequence split
//! across reads is buffered until it completes.
//!
//! Invalid bytes become U+FFFD, matching Node's `Buffer.toString("utf8")`.

use std::str;

const REPLACEMENT: char = '\u{fffd}';

#[derive(Default)]
pub struct Utf8StreamDecoder {
    pending: Vec<u8>,
}

impl Utf8StreamDecoder {
    /// Feeds one raw chunk and returns the text decoded so far.
    pub fn push(&mut self, chunk: &[u8]) -> String {
        if self.pending.is_empty() {
            // Fast path: a complete, valid chunk decodes without merging buffers.
            if let Ok(text) = str::from_utf8(chunk) {
                return text.to_string();
            }
        }

        let mut bytes = std::mem::take(&mut self.pending);
        bytes.extend_from_slice(chunk);

        let mut out = String::with_capacity(bytes.len());
        let mut offset = 0usize;
        let mut complete = true;
        loop {
            match str::from_utf8(&bytes[offset..]) {
                Ok(text) => {
                    out.push_str(text);
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        if let Ok(text) = str::from_utf8(&bytes[offset..offset + valid]) {
                            out.push_str(text);
                        }
                        offset += valid;
                    }
                    match error.error_len() {
                        // Invalid bytes: one replacement character per invalid sequence.
                        Some(length) => {
                            out.push(REPLACEMENT);
                            offset += length;
                        }
                        // Incomplete trailing sequence: hold it for the next chunk.
                        None => {
                            complete = false;
                            break;
                        }
                    }
                }
            }
        }

        if !complete {
            self.pending = bytes[offset..].to_vec();
        }
        out
    }

    /// Bytes still buffered — only ever an incomplete trailing sequence.
    pub fn buffered_bytes(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_ascii_and_multibyte() {
        let mut decoder = Utf8StreamDecoder::default();
        assert_eq!(decoder.push(b"ok"), "ok");
        assert_eq!(decoder.push("中文".as_bytes()), "中文");
    }

    #[test]
    fn keeps_an_incomplete_sequence_until_it_completes() {
        let bytes = "a汉b".as_bytes();
        // 'Han' is E6 B1 89: split the sequence across several reads.
        let mut decoder = Utf8StreamDecoder::default();
        assert_eq!(decoder.push(&bytes[..2]), "a");
        assert_eq!(decoder.buffered_bytes(), 1);
        assert_eq!(decoder.push(&bytes[2..3]), "");
        assert_eq!(decoder.buffered_bytes(), 2);
        assert_eq!(decoder.push(&bytes[3..4]), "汉");
        assert_eq!(decoder.buffered_bytes(), 0);
        assert_eq!(decoder.push(&bytes[4..]), "b");
    }
    #[test]
    fn invalid_bytes_become_replacement_characters() {
        let mut decoder = Utf8StreamDecoder::default();
        assert_eq!(decoder.push(&[0x61, 0xff, 0x62, 0x63]), "a\u{fffd}bc");
        assert_eq!(decoder.buffered_bytes(), 0);
    }

    #[test]
    fn four_byte_emoji_split_one_byte_per_read() {
        let bytes = "🙂".as_bytes();
        let mut decoder = Utf8StreamDecoder::default();
        let mut out = String::new();
        for byte in bytes {
            out.push_str(&decoder.push(&[*byte]));
        }
        assert_eq!(out, "🙂");
        assert_eq!(decoder.buffered_bytes(), 0);
    }
}
