//! Channel RPC wire protocol (VS Code-derived), ported byte-for-byte from
//! `packages/rpc/src` so the **current frontend** can talk to the **native backend**
//! without any renderer change.
//!
//! Three layers, same split as TS:
//! 1. [`serialization`] — typed binary values (Undefined/String/Buffer/VSBuffer/Array/
//!    JSON object/int) with VQL length prefixes.
//! 2. [`protocol`] — 13-byte frames `[type:u8][id:u32 BE][ack:u32 BE][length:u32 BE]`
//!    and a chunk stream that tolerates fragmentation and coalescing.
//! 3. [`channel`] — request/response/event messages riding those frames
//!    (`RequestType` 100–103, `ResponseType` 200–204).
//!
//! Fidelity evidence: `tests/golden.json` is generated from the real TS modules by
//! `tests/generate_golden.mjs` (esbuild bundle + node) and asserted in
//! `tests/golden_parity.rs`. Changing the wire format without regenerating the golden
//! file fails the tests — that is deliberate.

pub mod channel;
pub mod protocol;
pub mod serialization;

pub use channel::{encode_request, ChannelMessage, RequestType, ResponseType, RpcErrorPayload};
pub use protocol::{
    encode_frame, encode_frame_with, Frame, FrameDecoder, ProtocolMessageType, HEADER_SIZE,
    PROTOCOL_MESSAGE_REGULAR,
};
pub use serialization::{decode_value, encode_value, WireValue};
