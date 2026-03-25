//! MySQL packet framing — 3-byte payload_length + 1-byte sequence_id.
//!
//! Every MySQL message in both directions uses this envelope:
//! ```text
//! [payload_length: u24 LE] [sequence_id: u8] [payload: payload_length bytes]
//! ```
//!
//! This codec is used with `tokio_util::codec::{FramedRead, FramedWrite}`.
//!
//! ## Logical packet reassembly (Phase 5.4a)
//!
//! MySQL splits commands larger than 16,777,215 bytes (`0xFF_FFFF`) across
//! multiple physical packets called *continuation fragments*. A fragment with
//! `payload_length = 0xFF_FFFF` signals that more data follows; the final
//! fragment has `payload_length < 0xFF_FFFF`. The decoder reassembles all
//! fragments into a single logical payload before returning it to the caller.
//!
//! ## `max_payload_len` enforcement
//!
//! The decoder rejects logical payloads that exceed `max_payload_len` bytes
//! with `MySqlCodecError::PacketTooLarge` **before** the payload is
//! fully assembled — the check happens incrementally as each fragment is
//! scanned. This prevents unbounded memory allocation.
//!
//! The effective limit is kept in sync with the session variable
//! `@@max_allowed_packet` via `set_max_payload_len()`.

use std::fmt;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

/// A decoded MySQL logical packet: `(sequence_id, payload)`.
///
/// `sequence_id` is taken from the first physical fragment; subsequent
/// fragments' sequence IDs are consumed but not surfaced to the caller,
/// matching MySQL client driver expectations.
pub type Packet = (u8, Bytes);

/// The maximum physical-packet payload size.  A fragment with this exact size
/// is a continuation marker — more fragments follow.
const MAX_PHYSICAL_FRAGMENT: usize = 0xFF_FFFF;

// ── Error type ────────────────────────────────────────────────────────────────

/// Decode-path error for the MySQL wire framing layer.
///
/// Returned by `MySqlCodec` as the `Decoder::Error` type.  The caller must
/// handle `PacketTooLarge` by sending an `ER_NET_PACKET_TOO_LARGE` response
/// and closing the connection.
#[derive(Debug)]
pub enum MySqlCodecError {
    /// Underlying I/O error from the TCP stream.
    Io(std::io::Error),
    /// The logical payload (summed across all physical fragments) exceeded the
    /// connection's `max_payload_len`.
    PacketTooLarge { actual: usize, max: usize },
}

impl fmt::Display for MySqlCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::PacketTooLarge { actual, max } => {
                write!(f, "packet too large: {actual} bytes exceeds limit of {max}")
            }
        }
    }
}

impl std::error::Error for MySqlCodecError {}

/// Required by `tokio_util::codec::FramedRead`: underlying socket errors are
/// surfaced through the decoder's error type.
impl From<std::io::Error> for MySqlCodecError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ── Codec ─────────────────────────────────────────────────────────────────────

/// Framing codec for MySQL packets.
///
/// Handles the 4-byte packet envelope (`u24 LE` payload length + `u8` sequence
/// ID) and enforces a configurable inbound payload-size limit.
///
/// The encoder is unchanged from the original: it always writes a single
/// physical packet with a 4-byte header.  Outbound fragmentation is not needed
/// because AxiomDB sends result sets that fit within the MySQL default limit.
pub struct MySqlCodec {
    max_payload_len: usize,
}

impl MySqlCodec {
    /// Creates a codec with the given inbound payload limit (bytes).
    pub fn new(max_payload_len: usize) -> Self {
        Self { max_payload_len }
    }

    /// Returns the current inbound payload limit.
    pub fn max_payload_len(&self) -> usize {
        self.max_payload_len
    }

    /// Updates the inbound payload limit.
    ///
    /// Called by `handle_connection` after authentication and after the client
    /// changes `@@max_allowed_packet` with `SET max_allowed_packet = N`.
    pub fn set_max_payload_len(&mut self, max_payload_len: usize) {
        self.max_payload_len = max_payload_len;
    }
}

/// Default uses the MySQL-standard `67108864` (64 MiB) limit, matching
/// `ConnectionState::DEFAULT_MAX_ALLOWED_PACKET`.
impl Default for MySqlCodec {
    fn default() -> Self {
        // 64 MiB — must equal ConnectionState::DEFAULT_MAX_ALLOWED_PACKET.
        Self::new(67_108_864)
    }
}

// ── Decoder ───────────────────────────────────────────────────────────────────

impl Decoder for MySqlCodec {
    type Item = Packet;
    type Error = MySqlCodecError;

    /// Decodes one **logical** MySQL command from the read buffer.
    ///
    /// A logical command may span multiple physical *continuation fragments*
    /// when any fragment has `payload_length = 0xFF_FFFF`.  All fragments are
    /// reassembled into a single contiguous `Bytes` payload.
    ///
    /// Returns:
    /// - `Ok(None)` — not enough data yet; the buffer is not advanced.
    /// - `Ok(Some((seq_id, payload)))` — a complete logical packet.
    /// - `Err(PacketTooLarge)` — the cumulative payload would exceed
    ///   `max_payload_len`; the connection must be closed.
    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let mut pos: usize = 0;
        let mut total_payload: usize = 0;
        let mut fragment_count: usize = 0;

        // ── Scan phase: verify we have a complete logical packet ──────────────
        //
        // Walk through physical packet headers without consuming bytes.
        // This lets us detect incomplete data (return None) and oversized
        // payloads (return Err) before touching the buffer.
        loop {
            // Need at least 4 bytes for the next fragment header.
            if src.len() < pos + 4 {
                src.reserve(4usize.saturating_sub(src.len().saturating_sub(pos)));
                return Ok(None);
            }

            let frag_len = u32::from_le_bytes([src[pos], src[pos + 1], src[pos + 2], 0]) as usize;

            // Need the full fragment body to be buffered.
            if src.len() < pos + 4 + frag_len {
                src.reserve(pos + 4 + frag_len - src.len());
                return Ok(None);
            }

            // Accumulate and check against limit before committing.
            total_payload = total_payload.saturating_add(frag_len);
            if total_payload > self.max_payload_len {
                return Err(MySqlCodecError::PacketTooLarge {
                    actual: total_payload,
                    max: self.max_payload_len,
                });
            }

            fragment_count += 1;
            pos += 4 + frag_len;

            if frag_len < MAX_PHYSICAL_FRAGMENT {
                // This is the final (or only) fragment.
                break;
            }
            // frag_len == MAX_PHYSICAL_FRAGMENT → continuation; read next.
        }

        // ── Consume phase: extract sequence_id and build the payload ──────────

        // The sequence_id is taken from the first physical fragment (byte 3).
        let seq_id = src[3];

        if fragment_count == 1 {
            // Fast path: single physical packet.  No extra allocation needed —
            // `split_to` returns a zero-copy view into the existing buffer.
            src.advance(4); // consume the 4-byte header
            let payload = src.split_to(total_payload).freeze();
            return Ok(Some((seq_id, payload)));
        }

        // Multi-fragment path: allocate one contiguous buffer and copy each
        // fragment's payload into it, then advance `src` past all headers.
        let mut assembled = BytesMut::with_capacity(total_payload);
        loop {
            let frag_len = u32::from_le_bytes([src[0], src[1], src[2], 0]) as usize;
            src.advance(4); // consume header
            assembled.put_slice(&src[..frag_len]);
            src.advance(frag_len);
            if frag_len < MAX_PHYSICAL_FRAGMENT {
                break;
            }
        }
        Ok(Some((seq_id, assembled.freeze())))
    }
}

// ── Encoder ───────────────────────────────────────────────────────────────────

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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_physical(seq: u8, payload: &[u8]) -> Vec<u8> {
        let len = payload.len();
        let mut out = vec![
            (len & 0xFF) as u8,
            ((len >> 8) & 0xFF) as u8,
            ((len >> 16) & 0xFF) as u8,
            seq,
        ];
        out.extend_from_slice(payload);
        out
    }

    // ── Single packet ─────────────────────────────────────────────────────────

    #[test]
    fn test_single_packet_under_limit() {
        let mut codec = MySqlCodec::new(1024);
        let payload = b"SELECT 1";
        let mut buf = BytesMut::from(encode_physical(1, payload).as_slice());
        let (seq, data) = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(seq, 1);
        assert_eq!(data.as_ref(), payload);
        assert!(buf.is_empty(), "buffer must be fully consumed");
    }

    #[test]
    fn test_single_packet_exactly_at_limit() {
        let mut codec = MySqlCodec::new(8);
        let payload = b"SELECTXX"; // exactly 8 bytes
        let mut buf = BytesMut::from(encode_physical(0, payload).as_slice());
        let (_, data) = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(data.as_ref(), payload);
    }

    #[test]
    fn test_single_packet_one_byte_over_limit() {
        let mut codec = MySqlCodec::new(7);
        let payload = b"SELECTXX"; // 8 bytes — exceeds limit by 1
        let mut buf = BytesMut::from(encode_physical(0, payload).as_slice());
        match codec.decode(&mut buf).unwrap_err() {
            MySqlCodecError::PacketTooLarge { actual: 8, max: 7 } => {}
            e => panic!("expected PacketTooLarge, got: {e}"),
        }
    }

    #[test]
    fn test_partial_header_returns_none() {
        let mut codec = MySqlCodec::default();
        let mut buf = BytesMut::from(&[0x01, 0x00][..]);
        assert!(codec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn test_partial_payload_returns_none() {
        let mut codec = MySqlCodec::default();
        // Header says 5 bytes payload but only 3 are present.
        let mut buf = BytesMut::from(&[0x05, 0x00, 0x00, 0x01, b'a', b'b', b'c'][..]);
        assert!(codec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn test_empty_payload_is_accepted() {
        let mut codec = MySqlCodec::default();
        let mut buf = BytesMut::from(encode_physical(0, b"").as_slice());
        let (seq, data) = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(seq, 0);
        assert!(data.is_empty());
    }

    // ── Multi-packet reassembly ───────────────────────────────────────────────

    /// Build a continuation fragment of exactly MAX_PHYSICAL_FRAGMENT (0xFFFFFF) bytes.
    fn make_full_fragment(seq: u8) -> Vec<u8> {
        let payload = vec![0xABu8; MAX_PHYSICAL_FRAGMENT];
        encode_physical(seq, &payload)
    }

    #[test]
    fn test_two_fragment_reassembly_within_limit() {
        // Two-fragment logical packet: fragment 0 is MAX (continuation),
        // fragment 1 is 10 bytes (final).
        let total = MAX_PHYSICAL_FRAGMENT + 10;
        let mut codec = MySqlCodec::new(total + 1);

        let mut buf = BytesMut::new();
        buf.extend_from_slice(&make_full_fragment(0));
        buf.extend_from_slice(&encode_physical(1, &vec![0xCDu8; 10]));

        let (seq, data) = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(seq, 0, "sequence_id taken from first fragment");
        assert_eq!(data.len(), total);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_fragmented_total_exceeds_limit() {
        // First fragment (MAX_PHYSICAL_FRAGMENT bytes) already exceeds limit=1000.
        let mut codec = MySqlCodec::new(1000);
        let mut buf = BytesMut::from(make_full_fragment(0).as_slice());
        match codec.decode(&mut buf).unwrap_err() {
            MySqlCodecError::PacketTooLarge { .. } => {}
            e => panic!("expected PacketTooLarge, got: {e}"),
        }
    }

    #[test]
    fn test_fragmented_second_fragment_pushes_over_limit() {
        // Both fragments fit individually, but together exceed the limit.
        // limit = MAX_PHYSICAL_FRAGMENT + 5, first fragment = MAX, second = 10.
        let limit = MAX_PHYSICAL_FRAGMENT + 5; // accepts first, rejects after second
        let mut codec = MySqlCodec::new(limit);

        let mut buf = BytesMut::new();
        buf.extend_from_slice(&make_full_fragment(0));
        buf.extend_from_slice(&encode_physical(1, &vec![0u8; 10]));

        match codec.decode(&mut buf).unwrap_err() {
            MySqlCodecError::PacketTooLarge { actual, max } => {
                assert_eq!(actual, MAX_PHYSICAL_FRAGMENT + 10);
                assert_eq!(max, limit);
            }
            e => panic!("expected PacketTooLarge, got: {e}"),
        }
    }

    // ── set_max_payload_len ───────────────────────────────────────────────────

    #[test]
    fn test_set_max_payload_len_updates_limit() {
        let mut codec = MySqlCodec::new(1024);
        assert_eq!(codec.max_payload_len(), 1024);
        codec.set_max_payload_len(512);
        assert_eq!(codec.max_payload_len(), 512);

        // A 600-byte payload is now rejected.
        let payload = vec![0u8; 600];
        let mut buf = BytesMut::from(encode_physical(0, &payload).as_slice());
        assert!(matches!(
            codec.decode(&mut buf).unwrap_err(),
            MySqlCodecError::PacketTooLarge { .. }
        ));
    }

    // ── Encode/decode round-trip ──────────────────────────────────────────────

    #[test]
    fn test_encode_decode_round_trip() {
        let payload = b"Hello, MySQL!";
        let seq_id = 42u8;

        let mut dst = BytesMut::new();
        let mut codec = MySqlCodec::default();
        codec
            .encode((seq_id, payload.as_slice()), &mut dst)
            .unwrap();

        assert_eq!(dst.len(), 4 + payload.len());
        assert_eq!(dst[3], seq_id);

        let (decoded_seq, decoded_payload) = codec.decode(&mut dst).unwrap().unwrap();
        assert_eq!(decoded_seq, seq_id);
        assert_eq!(decoded_payload.as_ref(), payload);
    }
}
