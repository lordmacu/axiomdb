//! MySQL packet framing — 3-byte payload_length + 1-byte sequence_id.
//!
//! Every MySQL message in both directions uses this envelope:
//! ```text
//! [payload_length: u24 LE] [sequence_id: u8] [payload: payload_length bytes]
//! ```
//!
//! This codec is used with `tokio_util::codec::{FramedRead, FramedWrite}`.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

/// A decoded MySQL packet: `(sequence_id, payload)`.
pub type Packet = (u8, Bytes);

/// Framing codec for MySQL packets.
pub struct MySqlCodec;

impl Decoder for MySqlCodec {
    type Item = Packet;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // Need at least 4 bytes for the header (3 length + 1 seq_id).
        if src.len() < 4 {
            return Ok(None);
        }

        // Read payload length (3 bytes LE) without consuming.
        let payload_len = u32::from_le_bytes([src[0], src[1], src[2], 0]) as usize;

        // Wait until the full packet (header + payload) is buffered.
        if src.len() < 4 + payload_len {
            // Reserve space to avoid excessive re-allocations.
            src.reserve(4 + payload_len - src.len());
            return Ok(None);
        }

        // Consume the 3-byte length field.
        src.advance(3);
        let seq_id = src.get_u8();
        let payload = src.split_to(payload_len).freeze();

        Ok(Some((seq_id, payload)))
    }
}

impl Encoder<(u8, &[u8])> for MySqlCodec {
    type Error = std::io::Error;

    fn encode(
        &mut self,
        (seq_id, payload): (u8, &[u8]),
        dst: &mut BytesMut,
    ) -> Result<(), Self::Error> {
        let len = payload.len() as u32;
        // 3-byte little-endian payload length.
        dst.put_u8((len & 0xFF) as u8);
        dst.put_u8(((len >> 8) & 0xFF) as u8);
        dst.put_u8(((len >> 16) & 0xFF) as u8);
        dst.put_u8(seq_id);
        dst.put_slice(payload);
        Ok(())
    }
}
