//! 13-byte message framing — a byte-for-byte port of `packages/rpc/src/protocol.ts`.
//!
//! ```text
//! ┌─────────┬──────────┬──────────┬──────────────┐
//! │ type(1) │  id(4)   │  ack(4)  │  length(4)   │   all big-endian
//! └─────────┴──────────┴──────────┴──────────────┘
//! ```
//!
//! Framing discipline (same as TS): **the header is only consumed once the whole frame has
//! arrived**. When a fragment arrives we peek at the length first; if it is not enough we leave the
//! buffer untouched — otherwise the next segment that arrives would lose the frame header and the
//! message would be stuck forever, the old bug called out by name in the TS comment. The same is
//! avoided here by "decide completeness first, then drain".
//!
//! The only difference from TS: `length` has a cap (64 MiB by default). TS treats any u32 length as
//! the number of bytes to wait for, so a malicious/corrupted frame can grow the buffer to 4 GiB;
//! this host rejects it outright and reports an error, which is fail-closed rather than a change of
//! behaviour.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolMessageType {
    None = 0,
    Regular = 1,
    Control = 2,
    Ack = 3,
    Disconnect = 5,
    ReplayRequest = 6,
    Pause = 7,
    Resume = 8,
    KeepAlive = 9,
}

impl ProtocolMessageType {
    pub fn from_byte(value: u8) -> Option<Self> {
        Some(match value {
            0 => Self::None,
            1 => Self::Regular,
            2 => Self::Control,
            3 => Self::Ack,
            5 => Self::Disconnect,
            6 => Self::ReplayRequest,
            7 => Self::Pause,
            8 => Self::Resume,
            9 => Self::KeepAlive,
            _ => return None,
        })
    }
}

/// The byte value of the `Regular` frame type (channel messages only ever use this one).
pub const PROTOCOL_MESSAGE_REGULAR: u8 = 1;
pub const HEADER_SIZE: usize = 13;
pub const DEFAULT_MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub message_type: u8,
    pub id: u32,
    pub ack: u32,
    pub payload: Vec<u8>,
}

/// `writeProtocolMessage`: the four fields `type/id/ack/length` in big-endian order + payload.
pub fn encode_frame_with(message_type: u8, id: u32, ack: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_SIZE + payload.len());
    out.push(message_type);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&ack.to_be_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// A regular frame (type=Regular, id=0, ack=0), matching TS `SocketProtocol.send`.
pub fn encode_frame(payload: &[u8]) -> Vec<u8> {
    encode_frame_with(PROTOCOL_MESSAGE_REGULAR, 0, 0, payload)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameError {
    pub message: String,
}

/// Incremental framer: tolerates any fragmentation/coalescing and only emits complete frames.
pub struct FrameDecoder {
    buffer: Vec<u8>,
    max_frame_bytes: usize,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_FRAME_BYTES)
    }
}

impl FrameDecoder {
    pub fn new(max_frame_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            max_frame_bytes,
        }
    }

    /// Append a chunk of bytes and return every frame that can now be parsed completely (possibly none).
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Frame>, FrameError> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        loop {
            if self.buffer.len() < HEADER_SIZE {
                break;
            }
            let message_type = self.buffer[0];
            let id = u32::from_be_bytes(self.buffer[1..5].try_into().expect("4 bytes"));
            let ack = u32::from_be_bytes(self.buffer[5..9].try_into().expect("4 bytes"));
            let length = u32::from_be_bytes(self.buffer[9..13].try_into().expect("4 bytes"));
            if length as usize > self.max_frame_bytes {
                return Err(FrameError {
                    message: format!(
                        "declared frame length {} exceeds the {} byte limit",
                        length, self.max_frame_bytes
                    ),
                });
            }
            let total = HEADER_SIZE + length as usize;
            if self.buffer.len() < total {
                // The full frame has not arrived yet: keep the header and wait for the next chunk.
                break;
            }
            let payload = self.buffer[HEADER_SIZE..total].to_vec();
            self.buffer.drain(..total);
            frames.push(Frame {
                message_type,
                id,
                ack,
                payload,
            });
        }
        Ok(frames)
    }

    pub fn pending_bytes(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_header_is_thirteen_bytes_with_be_fields() {
        let frame = encode_frame(&[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(
            frame,
            vec![
                0x01, // Regular
                0, 0, 0, 0, // id
                0, 0, 0, 0, // ack
                0, 0, 0, 4, // length
                0xde, 0xad, 0xbe, 0xef,
            ]
        );
        let empty = encode_frame(&[]);
        assert_eq!(empty.len(), HEADER_SIZE);
        assert_eq!(empty, vec![0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn decoder_reassembles_fragmented_frames() {
        let mut decoder = FrameDecoder::default();
        let frame = encode_frame(b"hello");
        assert!(decoder.push(&frame[..5]).unwrap().is_empty());
        assert!(decoder.push(&frame[5..12]).unwrap().is_empty());
        let frames = decoder.push(&frame[12..]).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].payload, b"hello");
        assert_eq!(frames[0].message_type, PROTOCOL_MESSAGE_REGULAR);
        assert_eq!(decoder.pending_bytes(), 0);
    }

    #[test]
    fn decoder_splits_coalesced_frames_and_keeps_order() {
        let mut input = encode_frame(b"one");
        input.extend_from_slice(&encode_frame_with(3, 7, 9, b"two"));
        input.extend_from_slice(&encode_frame(&[]));
        let frames = FrameDecoder::default().push(&input).unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].payload, b"one");
        assert_eq!(frames[1].payload, b"two");
        assert_eq!(
            (frames[1].message_type, frames[1].id, frames[1].ack),
            (3, 7, 9)
        );
        assert_eq!(frames[2].payload, b"");
    }

    #[test]
    fn oversized_declared_length_is_rejected_fail_closed() {
        let mut decoder = FrameDecoder::new(16);
        let mut header = vec![1, 0, 0, 0, 0, 0, 0, 0, 0];
        header.extend_from_slice(&(17u32).to_be_bytes());
        assert!(decoder.push(&header).is_err());
    }
}
