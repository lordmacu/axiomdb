//! Row codec — binary encode/decode for `&[Value]` ↔ `&[u8]`.
//!
//! ## Binary format
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │ null_bitmap: ⌈n_cols / 8⌉ bytes                            │
//! │   bit i = (bitmap[i/8] >> (i%8)) & 1 == 1 → col i is NULL │
//! ├─────────────────────────────────────────────────────────────┤
//! │ For each non-NULL column in column order:                   │
//! │   Bool      → 1 byte  (0x00/0x01)                          │
//! │   Int/Date  → 4 bytes LE i32                               │
//! │   BigInt/Real/Timestamp → 8 bytes LE                       │
//! │   Decimal   → 16 bytes LE i128 + 1 byte scale              │
//! │   Uuid      → 16 bytes as-is                               │
//! │   Text/Bytes→ u24 LE length (3B) + payload bytes           │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Two independent size limits
//!
//! - **Codec limit**: Text/Bytes > 16,777,215 bytes → `DbError::ValueTooLarge`.
//!   This is the maximum representable by a u24 length prefix.
//! - **Storage limit**: encoded row > `MAX_TUPLE_DATA` (~16 KB) →
//!   `DbError::HeapPageFull` (enforced by `heap::insert_tuple`, not here).
//!   The codec does not know or enforce `PAGE_SIZE`.

use axiomdb_core::error::DbError;

use crate::{types::DataType, value::Value};

/// Maximum byte length of an inline Text or Bytes value.
/// Matches the maximum representable by a u24 prefix (3 bytes LE).
const MAX_INLINE_LEN: usize = 0xFF_FFFF; // 16,777,215

// ── Bitmap helpers ────────────────────────────────────────────────────────────

#[inline]
fn bitmap_len(n_cols: usize) -> usize {
    n_cols.div_ceil(8)
}

/// Returns `true` if column `col` is NULL according to `bitmap`.
#[inline]
fn is_null_bit(bitmap: &[u8], col: usize) -> bool {
    (bitmap[col / 8] >> (col % 8)) & 1 == 1
}

/// Marks column `col` as NULL in `bitmap`.
#[inline]
fn set_null_bit(bitmap: &mut [u8], col: usize) {
    bitmap[col / 8] |= 1 << (col % 8);
}

// ── u24 helpers ───────────────────────────────────────────────────────────────

fn write_u24(buf: &mut Vec<u8>, n: usize) {
    buf.push((n & 0xFF) as u8);
    buf.push(((n >> 8) & 0xFF) as u8);
    buf.push(((n >> 16) & 0xFF) as u8);
}

fn read_u24(bytes: &[u8], pos: usize) -> Result<usize, DbError> {
    if pos + 3 > bytes.len() {
        return Err(DbError::ParseError {
            message: format!("truncated: expected u24 length at offset {pos}"),
            position: None,
        });
    }
    Ok(bytes[pos] as usize | (bytes[pos + 1] as usize) << 8 | (bytes[pos + 2] as usize) << 16)
}

// ── Type validation ───────────────────────────────────────────────────────────

/// Checks that a non-Null `value` matches `dt`. Called by `encode_row`.
fn validate_type(value: &Value, dt: DataType) -> Result<(), DbError> {
    let ok = matches!(
        (value, dt),
        (Value::Bool(_), DataType::Bool)
            | (Value::Int(_), DataType::Int)
            | (Value::BigInt(_), DataType::BigInt)
            | (Value::Real(_), DataType::Real)
            | (Value::Decimal(..), DataType::Decimal)
            | (Value::Text(_), DataType::Text)
            | (Value::Bytes(_), DataType::Bytes)
            | (Value::Date(_), DataType::Date)
            | (Value::Timestamp(_), DataType::Timestamp)
            | (Value::Uuid(_), DataType::Uuid)
    );
    if ok {
        Ok(())
    } else {
        Err(DbError::TypeMismatch {
            expected: dt.name().to_string(),
            got: value.variant_name().to_string(),
        })
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Returns the encoded byte length for `values` without allocating.
///
/// Infallible and schema-free: each `Value` variant determines its own size.
/// Use before `insert_tuple` to check whether a row fits in the heap page.
pub fn encoded_len(values: &[Value]) -> usize {
    let blen = bitmap_len(values.len());
    let data: usize = values
        .iter()
        .map(|v| match v {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Int(_) | Value::Date(_) => 4,
            Value::BigInt(_) | Value::Real(_) | Value::Timestamp(_) => 8,
            Value::Decimal(..) => 17,
            Value::Uuid(_) => 16,
            Value::Text(s) => 3 + s.len(),
            Value::Bytes(b) => 3 + b.len(),
        })
        .sum();
    blen + data
}

/// Encodes `values` into a compact binary row using `schema` for type validation.
///
/// `schema` and `values` must have the same length. `Value::Null` is valid
/// for any `DataType`. A non-Null `Value` must match its `DataType`.
///
/// # Errors
/// - [`DbError::TypeMismatch`]  — lengths differ, or Value/DataType mismatch.
/// - [`DbError::InvalidValue`]  — `Value::Real` contains NaN.
/// - [`DbError::ValueTooLarge`] — Text or Bytes exceeds 16,777,215 bytes.
pub fn encode_row(values: &[Value], schema: &[DataType]) -> Result<Vec<u8>, DbError> {
    if values.len() != schema.len() {
        return Err(DbError::TypeMismatch {
            expected: format!("{} columns", schema.len()),
            got: format!("{} values", values.len()),
        });
    }

    let n = values.len();
    let blen = bitmap_len(n);
    let mut buf = Vec::with_capacity(encoded_len(values));

    // Phase 1: reserve null bitmap (zeroed).
    buf.resize(blen, 0u8);

    // Phase 2: set null bits + validate non-null types.
    for (i, (v, &dt)) in values.iter().zip(schema.iter()).enumerate() {
        if v.is_null() {
            set_null_bit(&mut buf, i);
        } else {
            validate_type(v, dt)?;
        }
    }

    // Phase 3: encode non-null values in column order.
    for (i, v) in values.iter().enumerate() {
        if is_null_bit(&buf[0..blen], i) {
            continue;
        }
        match v {
            Value::Bool(b) => buf.push(if *b { 1 } else { 0 }),
            Value::Int(n) => buf.extend_from_slice(&n.to_le_bytes()),
            Value::BigInt(n) => buf.extend_from_slice(&n.to_le_bytes()),
            Value::Real(f) => {
                if f.is_nan() {
                    return Err(DbError::InvalidValue {
                        reason: "NaN is not a valid SQL real value".into(),
                    });
                }
                buf.extend_from_slice(&f.to_le_bytes());
            }
            Value::Decimal(m, s) => {
                buf.extend_from_slice(&m.to_le_bytes());
                buf.push(*s);
            }
            Value::Text(s) => {
                let bytes = s.as_bytes();
                if bytes.len() > MAX_INLINE_LEN {
                    return Err(DbError::ValueTooLarge {
                        len: bytes.len(),
                        max: MAX_INLINE_LEN,
                    });
                }
                write_u24(&mut buf, bytes.len());
                buf.extend_from_slice(bytes);
            }
            Value::Bytes(b) => {
                if b.len() > MAX_INLINE_LEN {
                    return Err(DbError::ValueTooLarge {
                        len: b.len(),
                        max: MAX_INLINE_LEN,
                    });
                }
                write_u24(&mut buf, b.len());
                buf.extend_from_slice(b);
            }
            Value::Date(d) => buf.extend_from_slice(&d.to_le_bytes()),
            Value::Timestamp(t) => buf.extend_from_slice(&t.to_le_bytes()),
            Value::Uuid(u) => buf.extend_from_slice(u),
            Value::Null => unreachable!("null already skipped by bitmap check"),
        }
    }

    Ok(buf)
}

/// Decodes a binary row back into `Vec<Value>`.
///
/// `schema` must match the schema used when the row was encoded.
///
/// # Errors
/// - [`DbError::ParseError`] — bytes are truncated or structurally invalid.
/// - [`DbError::ParseError`] — a Text value contains invalid UTF-8.
pub fn decode_row(bytes: &[u8], schema: &[DataType]) -> Result<Vec<Value>, DbError> {
    let n = schema.len();
    let blen = bitmap_len(n);

    if bytes.len() < blen {
        return Err(DbError::ParseError {
            message: format!("truncated: need {blen} bitmap bytes, got {}", bytes.len()),
            position: None,
        });
    }

    let bitmap = &bytes[0..blen];
    let mut pos = blen;
    let mut values = Vec::with_capacity(n);

    for (i, &dt) in schema.iter().enumerate() {
        if is_null_bit(bitmap, i) {
            values.push(Value::Null);
            continue;
        }

        let v = match dt {
            DataType::Bool => {
                ensure_bytes(bytes, pos, 1)?;
                let v = bytes[pos] != 0;
                pos += 1;
                Value::Bool(v)
            }
            DataType::Int => {
                ensure_bytes(bytes, pos, 4)?;
                let v = i32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
                pos += 4;
                Value::Int(v)
            }
            DataType::BigInt => {
                ensure_bytes(bytes, pos, 8)?;
                let v = i64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
                pos += 8;
                Value::BigInt(v)
            }
            DataType::Real => {
                ensure_bytes(bytes, pos, 8)?;
                let v = f64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
                pos += 8;
                Value::Real(v)
            }
            DataType::Decimal => {
                ensure_bytes(bytes, pos, 17)?;
                let m = i128::from_le_bytes(bytes[pos..pos + 16].try_into().unwrap());
                let s = bytes[pos + 16];
                pos += 17;
                Value::Decimal(m, s)
            }
            DataType::Text => {
                let len = read_u24(bytes, pos)?;
                pos += 3;
                ensure_bytes(bytes, pos, len)?;
                let s = std::str::from_utf8(&bytes[pos..pos + len])
                    .map_err(|_| DbError::ParseError {
                        message: format!("invalid UTF-8 in Text column at offset {pos}"),
                        position: None,
                    })?
                    .to_string();
                pos += len;
                Value::Text(s)
            }
            DataType::Bytes => {
                let len = read_u24(bytes, pos)?;
                pos += 3;
                ensure_bytes(bytes, pos, len)?;
                let b = bytes[pos..pos + len].to_vec();
                pos += len;
                Value::Bytes(b)
            }
            DataType::Date => {
                ensure_bytes(bytes, pos, 4)?;
                let v = i32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
                pos += 4;
                Value::Date(v)
            }
            DataType::Timestamp => {
                ensure_bytes(bytes, pos, 8)?;
                let v = i64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
                pos += 8;
                Value::Timestamp(v)
            }
            DataType::Uuid => {
                ensure_bytes(bytes, pos, 16)?;
                let u: [u8; 16] = bytes[pos..pos + 16].try_into().unwrap();
                pos += 16;
                Value::Uuid(u)
            }
        };
        values.push(v);
    }

    Ok(values)
}

/// Like [`decode_row`] but skips decoding columns where `mask[i] == false`.
///
/// For skipped columns the byte cursor is still advanced by the column's wire
/// size (so subsequent columns decode correctly), but no heap allocation is
/// made and `Value::Null` is stored in the output slot.
///
/// ## Null + mask interaction
///
/// If `is_null_bit(bitmap, i)` is true, the column has no wire bytes regardless
/// of `mask[i]`. The cursor is not advanced and `Value::Null` is pushed.
///
/// ## Errors
/// - [`DbError::TypeMismatch`] — `mask.len() != schema.len()`
/// - [`DbError::ParseError`] — bytes truncated or structurally invalid (even
///   for skipped columns — corrupt input is always an error)
pub fn decode_row_masked(
    bytes: &[u8],
    schema: &[DataType],
    mask: &[bool],
) -> Result<Vec<Value>, DbError> {
    if mask.len() != schema.len() {
        return Err(DbError::TypeMismatch {
            expected: format!("mask length {}", schema.len()),
            got: format!("mask length {}", mask.len()),
        });
    }

    let n = schema.len();
    let blen = bitmap_len(n);

    if bytes.len() < blen {
        return Err(DbError::ParseError {
            message: format!("truncated: need {blen} bitmap bytes, got {}", bytes.len()),
            position: None,
        });
    }

    let bitmap = &bytes[0..blen];
    let mut pos = blen;
    let mut values = Vec::with_capacity(n);

    for (i, &dt) in schema.iter().enumerate() {
        // NULL columns have no wire bytes — handle before checking mask.
        if is_null_bit(bitmap, i) {
            values.push(Value::Null);
            continue;
        }

        if mask[i] {
            // Decode normally — identical to decode_row arms.
            let v = match dt {
                DataType::Bool => {
                    ensure_bytes(bytes, pos, 1)?;
                    let v = bytes[pos] != 0;
                    pos += 1;
                    Value::Bool(v)
                }
                DataType::Int => {
                    ensure_bytes(bytes, pos, 4)?;
                    let v = i32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
                    pos += 4;
                    Value::Int(v)
                }
                DataType::BigInt => {
                    ensure_bytes(bytes, pos, 8)?;
                    let v = i64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
                    pos += 8;
                    Value::BigInt(v)
                }
                DataType::Real => {
                    ensure_bytes(bytes, pos, 8)?;
                    let v = f64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
                    pos += 8;
                    Value::Real(v)
                }
                DataType::Decimal => {
                    ensure_bytes(bytes, pos, 17)?;
                    let m = i128::from_le_bytes(bytes[pos..pos + 16].try_into().unwrap());
                    let s = bytes[pos + 16];
                    pos += 17;
                    Value::Decimal(m, s)
                }
                DataType::Text => {
                    let len = read_u24(bytes, pos)?;
                    pos += 3;
                    ensure_bytes(bytes, pos, len)?;
                    let s = std::str::from_utf8(&bytes[pos..pos + len])
                        .map_err(|_| DbError::ParseError {
                            message: format!("invalid UTF-8 in Text column at offset {pos}"),
                            position: None,
                        })?
                        .to_string();
                    pos += len;
                    Value::Text(s)
                }
                DataType::Bytes => {
                    let len = read_u24(bytes, pos)?;
                    pos += 3;
                    ensure_bytes(bytes, pos, len)?;
                    let b = bytes[pos..pos + len].to_vec();
                    pos += len;
                    Value::Bytes(b)
                }
                DataType::Date => {
                    ensure_bytes(bytes, pos, 4)?;
                    let v = i32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
                    pos += 4;
                    Value::Date(v)
                }
                DataType::Timestamp => {
                    ensure_bytes(bytes, pos, 8)?;
                    let v = i64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
                    pos += 8;
                    Value::Timestamp(v)
                }
                DataType::Uuid => {
                    ensure_bytes(bytes, pos, 16)?;
                    let u: [u8; 16] = bytes[pos..pos + 16].try_into().unwrap();
                    pos += 16;
                    Value::Uuid(u)
                }
            };
            values.push(v);
        } else {
            // Skip: advance cursor without allocating.
            match dt {
                DataType::Bool => {
                    ensure_bytes(bytes, pos, 1)?;
                    pos += 1;
                }
                DataType::Int | DataType::Date => {
                    ensure_bytes(bytes, pos, 4)?;
                    pos += 4;
                }
                DataType::BigInt | DataType::Real | DataType::Timestamp => {
                    ensure_bytes(bytes, pos, 8)?;
                    pos += 8;
                }
                DataType::Decimal => {
                    ensure_bytes(bytes, pos, 17)?;
                    pos += 17;
                }
                DataType::Uuid => {
                    ensure_bytes(bytes, pos, 16)?;
                    pos += 16;
                }
                DataType::Text | DataType::Bytes => {
                    let len = read_u24(bytes, pos)?;
                    pos += 3;
                    ensure_bytes(bytes, pos, len)?;
                    pos += len; // skip payload — no copy, no allocation
                }
            }
            values.push(Value::Null);
        }
    }

    Ok(values)
}

/// Checks that `bytes[pos..pos+need]` is within bounds.
#[inline]
fn ensure_bytes(bytes: &[u8], pos: usize, need: usize) -> Result<(), DbError> {
    if pos + need > bytes.len() {
        Err(DbError::ParseError {
            message: format!(
                "truncated: need {} bytes at offset {pos}, got {}",
                need,
                bytes.len().saturating_sub(pos)
            ),
            position: None,
        })
    } else {
        Ok(())
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Bitmap helpers ────────────────────────────────────────────────────────

    #[test]
    fn test_bitmap_len() {
        assert_eq!(bitmap_len(0), 0);
        assert_eq!(bitmap_len(1), 1);
        assert_eq!(bitmap_len(8), 1);
        assert_eq!(bitmap_len(9), 2);
        assert_eq!(bitmap_len(16), 2);
        assert_eq!(bitmap_len(17), 3);
    }

    #[test]
    fn test_bitmap_set_and_read() {
        let mut bm = vec![0u8; 2];
        set_null_bit(&mut bm, 0);
        set_null_bit(&mut bm, 7);
        set_null_bit(&mut bm, 8);
        assert!(is_null_bit(&bm, 0));
        assert!(is_null_bit(&bm, 7));
        assert!(is_null_bit(&bm, 8));
        assert!(!is_null_bit(&bm, 1));
        assert!(!is_null_bit(&bm, 9));
        // byte 0 = bits 0 and 7 set = 0b10000001
        assert_eq!(bm[0], 0b10000001);
        // byte 1 = bit 0 set = 0b00000001
        assert_eq!(bm[1], 0b00000001);
    }

    // ── u24 helpers ───────────────────────────────────────────────────────────

    #[test]
    fn test_u24_roundtrip() {
        for n in [0usize, 1, 255, 256, 65535, 16_777_215] {
            let mut buf = Vec::new();
            write_u24(&mut buf, n);
            assert_eq!(buf.len(), 3);
            assert_eq!(read_u24(&buf, 0).unwrap(), n);
        }
    }

    #[test]
    fn test_u24_truncated() {
        let buf = [0x01u8, 0x00]; // only 2 bytes
        assert!(read_u24(&buf, 0).is_err());
    }

    // ── encoded_len ───────────────────────────────────────────────────────────

    #[test]
    fn test_encoded_len_empty_row() {
        assert_eq!(encoded_len(&[]), 0);
    }

    #[test]
    fn test_encoded_len_single_null() {
        assert_eq!(encoded_len(&[Value::Null]), 1); // bitmap only
    }

    #[test]
    fn test_encoded_len_matches_actual() {
        let values = vec![
            Value::Int(42),
            Value::Null,
            Value::Text("hello".into()),
            Value::Bool(true),
        ];
        let schema = &[DataType::Int, DataType::Int, DataType::Text, DataType::Bool];
        let predicted = encoded_len(&values);
        let actual = encode_row(&values, schema).unwrap().len();
        assert_eq!(predicted, actual);
    }
}
