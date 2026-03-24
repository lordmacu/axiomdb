//! Order-preserving key encoding for B-Tree indexes.
//!
//! Converts a `&[Value]` into a `Vec<u8>` such that the lexicographic byte
//! order of the output matches the SQL comparison order of the input.
//!
//! ## Encoding per type
//!
//! Each value is prefixed with a 1-byte type tag so that values of different
//! types compare in a defined order (NULL < Bool < Int < BigInt < Real <
//! Decimal < Date < Timestamp < Text < Bytes < Uuid).
//!
//! | Type           | Tag  | Payload |
//! |----------------|------|---------|
//! | NULL           | 0x00 | none |
//! | Bool           | 0x01 | 1 byte (0 or 1) |
//! | Int(i32)       | 0x02 | 8 BE bytes after sign-flip |
//! | BigInt(i64)    | 0x03 | 8 BE bytes after sign-flip |
//! | Real(f64)      | 0x04 | 8 BE bytes — NaN=0, pos: MSB set, neg: all bits flipped |
//! | Decimal(i128,u8) | 0x05 | 1 byte scale + 16 BE bytes after sign-flip |
//! | Date(i32)      | 0x06 | 8 BE bytes after sign-flip |
//! | Timestamp(i64) | 0x07 | 8 BE bytes after sign-flip |
//! | Text           | 0x08 | NUL-terminated UTF-8 (NUL escaped as 0xFF 0x00) |
//! | Bytes          | 0x09 | NUL-terminated raw bytes (same escape) |
//! | Uuid([u8;16])  | 0x0A | 16 raw bytes (already lexicographically ordered) |
//!
//! ## Composite keys
//!
//! For multi-column indexes, the values are encoded in order and concatenated.
//! The first column has the most significant sort influence.
//!
//! ## Maximum key length
//!
//! Keys are limited to [`MAX_INDEX_KEY`] bytes.  Keys that exceed this limit
//! are rejected with [`DbError::IndexKeyTooLong`].

use axiomdb_core::error::DbError;
use axiomdb_types::Value;

/// Maximum allowed byte length for an encoded index key.
///
/// Chosen to stay within the B-Tree `MAX_KEY_LEN` (768 bytes per
/// `axiomdb-index/src/page_layout`).
pub const MAX_INDEX_KEY: usize = 768;

// ── Public API ────────────────────────────────────────────────────────────────

/// Encodes `values` into an order-preserving byte key.
///
/// The output satisfies: `encode(a) < encode(b)` iff `a < b` under SQL
/// comparison semantics (NULL sorts first, within a type numerically/lexicographically).
///
/// # Errors
/// Returns [`DbError::IndexKeyTooLong`] if the encoded key exceeds
/// [`MAX_INDEX_KEY`] bytes.
pub fn encode_index_key(values: &[Value]) -> Result<Vec<u8>, DbError> {
    let mut buf = Vec::with_capacity(32);
    for v in values {
        encode_value(v, &mut buf);
    }
    if buf.len() > MAX_INDEX_KEY {
        return Err(DbError::IndexKeyTooLong {
            key_len: buf.len(),
            max: MAX_INDEX_KEY,
        });
    }
    Ok(buf)
}

// ── Per-value encoding ────────────────────────────────────────────────────────

fn encode_value(v: &Value, buf: &mut Vec<u8>) {
    match v {
        Value::Null => {
            buf.push(0x00);
        }
        Value::Bool(b) => {
            buf.push(0x01);
            buf.push(*b as u8);
        }
        Value::Int(n) => {
            buf.push(0x02);
            // Sign-flip: XOR with i64::MIN then treat as u64 big-endian.
            // This makes negative integers sort before positive ones as bytes.
            let u = (*n as i64 ^ i64::MIN) as u64;
            buf.extend_from_slice(&u.to_be_bytes());
        }
        Value::BigInt(n) => {
            buf.push(0x03);
            let u = (*n ^ i64::MIN) as u64;
            buf.extend_from_slice(&u.to_be_bytes());
        }
        Value::Real(f) => {
            buf.push(0x04);
            buf.extend_from_slice(&encode_f64(*f));
        }
        Value::Decimal(mantissa, scale) => {
            buf.push(0x05);
            buf.push(*scale);
            // Sign-flip on 128-bit: XOR with i128::MIN then treat as u128 BE.
            let u = (*mantissa ^ i128::MIN) as u128;
            buf.extend_from_slice(&u.to_be_bytes());
        }
        Value::Date(days) => {
            buf.push(0x06);
            let u = (*days as i64 ^ i64::MIN) as u64;
            buf.extend_from_slice(&u.to_be_bytes());
        }
        Value::Timestamp(micros) => {
            buf.push(0x07);
            let u = (*micros ^ i64::MIN) as u64;
            buf.extend_from_slice(&u.to_be_bytes());
        }
        Value::Text(s) => {
            buf.push(0x08);
            encode_bytes_nul(s.as_bytes(), buf);
        }
        Value::Bytes(b) => {
            buf.push(0x09);
            encode_bytes_nul(b, buf);
        }
        Value::Uuid(u) => {
            buf.push(0x0A);
            buf.extend_from_slice(u);
        }
    }
}

/// Encodes an f64 into 8 bytes that preserve comparison order.
///
/// - NaN → 8 zero bytes (sorts before everything)
/// - Positive: set MSB (0x80...) so positive numbers sort above negative
/// - Negative: flip all bits so that more negative values sort lower
fn encode_f64(f: f64) -> [u8; 8] {
    if f.is_nan() {
        return [0u8; 8];
    }
    let bits = f.to_bits();
    let result: u64 = if f >= 0.0 { bits | (1u64 << 63) } else { !bits };
    result.to_be_bytes()
}

/// NUL-terminated byte encoding with 0xFF-escape for embedded NUL bytes.
///
/// Embedded `0x00` → `[0xFF, 0x00]` to preserve sort order.
/// Sequence is terminated with a plain `0x00` sentinel.
fn encode_bytes_nul(b: &[u8], buf: &mut Vec<u8>) {
    for &byte in b {
        if byte == 0x00 {
            buf.push(0xFF);
            buf.push(0x00);
        } else {
            buf.push(byte);
        }
    }
    buf.push(0x00); // terminator
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(values: &[Value]) -> Vec<u8> {
        encode_index_key(values).unwrap()
    }

    fn assert_order(lesser: &[Value], greater: &[Value]) {
        assert!(
            enc(lesser) < enc(greater),
            "expected {:?} < {:?}",
            lesser,
            greater
        );
    }

    #[test]
    fn test_null_sorts_first() {
        assert_order(&[Value::Null], &[Value::Bool(false)]);
        assert_order(&[Value::Null], &[Value::Int(i32::MIN)]);
        assert_order(&[Value::Null], &[Value::Text(String::new())]);
    }

    #[test]
    fn test_int_sort_order() {
        assert_order(&[Value::Int(-100)], &[Value::Int(-1)]);
        assert_order(&[Value::Int(-1)], &[Value::Int(0)]);
        assert_order(&[Value::Int(0)], &[Value::Int(1)]);
        assert_order(&[Value::Int(i32::MIN)], &[Value::Int(i32::MAX)]);
    }

    #[test]
    fn test_bigint_sort_order() {
        assert_order(&[Value::BigInt(i64::MIN)], &[Value::BigInt(-1)]);
        assert_order(&[Value::BigInt(-1)], &[Value::BigInt(0)]);
        assert_order(&[Value::BigInt(0)], &[Value::BigInt(i64::MAX)]);
    }

    #[test]
    fn test_real_sort_order() {
        assert_order(&[Value::Real(-1.0)], &[Value::Real(0.0)]);
        assert_order(&[Value::Real(0.0)], &[Value::Real(1.0)]);
        assert_order(&[Value::Real(-100.0)], &[Value::Real(-1.0)]);
        assert_order(
            &[Value::Real(f64::NEG_INFINITY)],
            &[Value::Real(f64::INFINITY)],
        );
    }

    #[test]
    fn test_text_lexicographic_order() {
        assert_order(&[Value::Text("a".into())], &[Value::Text("b".into())]);
        assert_order(&[Value::Text("abc".into())], &[Value::Text("abd".into())]);
        assert_order(&[Value::Text("".into())], &[Value::Text("a".into())]);
    }

    #[test]
    fn test_text_with_nul_byte_escaping() {
        // A string that starts with NUL should sort AFTER Null (tag 0x08 > 0x00)
        // and the NUL byte should be escaped, not terminate early.
        let s_with_nul = Value::Text("\x00z".into());
        let s_plain = Value::Text("a".into());
        // \x00z encodes as [0x08, 0xFF, 0x00, 'z', 0x00]
        // "a"   encodes as [0x08, 'a', 0x00]
        // 0xFF > 'a' so "\x00z" > "a"
        assert_order(&[Value::Text("".into())], &[s_with_nul.clone()]);
        let encoded = enc(&[s_with_nul]);
        // Ensure no premature NUL terminator (byte at index 2 should be 0x00 not 0x00-as-terminator)
        assert_eq!(encoded[1], 0xFF, "NUL escape first byte should be 0xFF");
        assert_eq!(encoded[2], 0x00, "NUL escape second byte should be 0x00");
        assert_eq!(encoded[3], b'z');
    }

    #[test]
    fn test_composite_key_order() {
        // (1, "a") < (1, "b")
        assert_order(
            &[Value::Int(1), Value::Text("a".into())],
            &[Value::Int(1), Value::Text("b".into())],
        );
        // (1, "z") < (2, "a")
        assert_order(
            &[Value::Int(1), Value::Text("z".into())],
            &[Value::Int(2), Value::Text("a".into())],
        );
    }

    #[test]
    fn test_key_too_long_error() {
        // A very long text value should exceed MAX_INDEX_KEY.
        let long_text = Value::Text("x".repeat(MAX_INDEX_KEY + 1));
        let err = encode_index_key(&[long_text]).unwrap_err();
        assert!(
            matches!(err, DbError::IndexKeyTooLong { .. }),
            "expected IndexKeyTooLong, got: {err}"
        );
    }

    #[test]
    fn test_roundtrip_order_uuid() {
        let u1 = [0u8; 16];
        let mut u2 = [0u8; 16];
        u2[15] = 1;
        assert_order(&[Value::Uuid(u1)], &[Value::Uuid(u2)]);
    }

    #[test]
    fn test_timestamp_sort_order() {
        assert_order(&[Value::Timestamp(-1_000_000)], &[Value::Timestamp(0)]);
        assert_order(&[Value::Timestamp(0)], &[Value::Timestamp(1_000_000)]);
    }
}
