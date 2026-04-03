//! Field-level patching — modify single columns without full row decode/encode.
//!
//! Inspired by InnoDB's `btr_cur_upd_rec_in_place()` which patches only the
//! changed bytes (e.g., 4 bytes for an INT) instead of re-encoding the entire row.
//!
//! For `UPDATE t SET score = score + 1`, this reduces per-row work from
//! ~469 bytes (full decode + encode + WAL) to ~28 bytes (read field + write field).
//!
//! ## Limitations
//!
//! - Only works for fixed-size columns (Bool, Int, BigInt, Real, Date, Timestamp)
//! - Cannot patch Text/Bytes columns (variable-length encoding)
//! - Requires no NULLs in preceding columns (offset calculation depends on null bitmap)
//! - Falls back to full decode/encode if any condition fails

use crate::types::DataType;
use crate::value::Value;
use axiomdb_core::error::DbError;

/// Size in bytes of a fixed-size column type in the row encoding.
/// Returns None for variable-length types (Text, Bytes, Decimal, Uuid).
pub fn fixed_encoded_size(dt: DataType) -> Option<usize> {
    match dt {
        DataType::Bool => Some(1),
        DataType::Int | DataType::Date => Some(4),
        DataType::BigInt | DataType::Real | DataType::Timestamp => Some(8),
        // Variable-length or complex types: cannot patch in place.
        DataType::Text | DataType::Bytes | DataType::Decimal | DataType::Uuid => None,
    }
}

/// Pre-computed field location within an encoded row.
#[derive(Debug, Clone, Copy)]
pub struct FieldLocation {
    /// Column index in the table schema.
    pub col_idx: usize,
    /// Byte offset from the start of the row data (after RowHeader).
    pub offset: usize,
    /// Encoded size in bytes.
    pub size: usize,
    /// Data type for encoding/decoding.
    pub data_type: DataType,
}

/// Calculates the byte offset of column `target_col` within an encoded row,
/// scanning the actual row bytes to handle variable-length columns.
///
/// This is the SQLite-style approach: parse the row header sequentially,
/// advancing through variable-length fields until the target column is reached.
///
/// Returns `None` if:
/// - The target column itself is variable-length
/// - The target column is NULL
pub fn compute_field_location(
    schema: &[DataType],
    target_col: usize,
    null_bitmap: &[u8],
) -> Option<FieldLocation> {
    compute_field_location_runtime(schema, target_col, null_bitmap, None)
}

/// Like `compute_field_location` but also accepts the row data bytes for
/// runtime scanning of variable-length columns (Text/Bytes with u24 length).
pub fn compute_field_location_runtime(
    schema: &[DataType],
    target_col: usize,
    null_bitmap: &[u8],
    row_data: Option<&[u8]>,
) -> Option<FieldLocation> {
    if target_col >= schema.len() {
        return None;
    }

    // Target column must be fixed-size.
    let target_size = fixed_encoded_size(schema[target_col])?;

    // If target column is NULL, can't patch.
    if is_null(null_bitmap, target_col) {
        return None;
    }

    // Calculate offset by scanning preceding columns.
    let bitmap_len = schema.len().div_ceil(8);
    let mut offset = bitmap_len;
    for (i, &dt) in schema[..target_col].iter().enumerate() {
        if is_null(null_bitmap, i) {
            continue;
        }
        match fixed_encoded_size(dt) {
            Some(sz) => offset += sz,
            None => {
                // Variable-length column: need row data to read the u24 length.
                let data = row_data?;
                if offset + 3 > data.len() {
                    return None;
                }
                let payload_len = data[offset] as usize
                    | (data[offset + 1] as usize) << 8
                    | (data[offset + 2] as usize) << 16;
                offset += 3 + payload_len; // u24 prefix + payload
            }
        }
    }

    Some(FieldLocation {
        col_idx: target_col,
        offset,
        size: target_size,
        data_type: schema[target_col],
    })
}

/// Reads a single field value from encoded row bytes at the given location.
pub fn read_field(row_data: &[u8], loc: &FieldLocation) -> Result<Value, DbError> {
    let bytes = &row_data[loc.offset..loc.offset + loc.size];
    match loc.data_type {
        DataType::Bool => Ok(Value::Bool(bytes[0] != 0)),
        DataType::Int | DataType::Date => {
            let v = i32::from_le_bytes(bytes.try_into().unwrap());
            if loc.data_type == DataType::Date {
                Ok(Value::Date(v))
            } else {
                Ok(Value::Int(v))
            }
        }
        DataType::BigInt => Ok(Value::BigInt(i64::from_le_bytes(bytes.try_into().unwrap()))),
        DataType::Real => Ok(Value::Real(f64::from_le_bytes(bytes.try_into().unwrap()))),
        DataType::Timestamp => Ok(Value::Timestamp(i64::from_le_bytes(
            bytes.try_into().unwrap(),
        ))),
        _ => Err(DbError::Other("field_patch: unsupported type".into())),
    }
}

/// Writes a single field value into encoded row bytes at the given location.
/// Returns the old bytes (for WAL undo) and writes new bytes in place.
pub fn write_field(
    row_data: &mut [u8],
    loc: &FieldLocation,
    new_value: &Value,
) -> Result<[u8; 8], DbError> {
    let mut old_bytes = [0u8; 8];
    old_bytes[..loc.size].copy_from_slice(&row_data[loc.offset..loc.offset + loc.size]);

    let new_bytes = encode_field_value(new_value, loc.data_type)?;
    row_data[loc.offset..loc.offset + loc.size].copy_from_slice(&new_bytes[..loc.size]);

    Ok(old_bytes)
}

/// Encodes a single `Value` to its fixed-size byte representation as `[u8; 8]`.
///
/// Only `size` bytes (from `fixed_encoded_size`) are meaningful; the rest are
/// zeroed. Used by the zero-alloc UPDATE fast path to build WAL `FieldDelta`
/// entries and to supply `new_bytes` to `patch_field_in_place` without
/// allocating a `Vec<u8>`.
pub fn encode_value_fixed(value: &Value, dt: DataType) -> Result<[u8; 8], DbError> {
    encode_field_value(value, dt)
}

/// Encodes a single Value to its fixed-size byte representation.
fn encode_field_value(value: &Value, dt: DataType) -> Result<[u8; 8], DbError> {
    let mut buf = [0u8; 8];
    match (value, dt) {
        (Value::Bool(b), DataType::Bool) => {
            buf[0] = if *b { 1 } else { 0 };
        }
        (Value::Int(n), DataType::Int) | (Value::Int(n), DataType::Date) => {
            buf[..4].copy_from_slice(&n.to_le_bytes());
        }
        (Value::Date(n), DataType::Date) => {
            buf[..4].copy_from_slice(&n.to_le_bytes());
        }
        (Value::BigInt(n), DataType::BigInt) => {
            buf[..8].copy_from_slice(&n.to_le_bytes());
        }
        (Value::Real(f), DataType::Real) => {
            buf[..8].copy_from_slice(&f.to_le_bytes());
        }
        (Value::Timestamp(t), DataType::Timestamp) => {
            buf[..8].copy_from_slice(&t.to_le_bytes());
        }
        _ => {
            return Err(DbError::Other(format!(
                "field_patch: cannot encode {value:?} as {dt:?}"
            )));
        }
    }
    Ok(buf)
}

#[inline]
fn is_null(bitmap: &[u8], col: usize) -> bool {
    (bitmap[col / 8] >> (col % 8)) & 1 == 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode_row, encode_row};

    #[test]
    fn test_compute_field_location_simple() {
        // Schema: (id: Int, name: Text, score: Int)
        // Can't compute score offset because name is variable-length.
        let schema = [DataType::Int, DataType::Text, DataType::Int];
        let bitmap = [0u8]; // no NULLs
        assert!(compute_field_location(&schema, 2, &bitmap).is_none()); // Text before score
    }

    #[test]
    fn test_compute_field_location_all_fixed() {
        // Schema: (id: Int, score: Int, active: Bool)
        let schema = [DataType::Int, DataType::Int, DataType::Bool];
        let bitmap = [0u8]; // no NULLs, bitmap = 1 byte for 3 cols

        let loc = compute_field_location(&schema, 1, &bitmap).unwrap(); // score
        assert_eq!(loc.col_idx, 1);
        assert_eq!(loc.offset, 1 + 4); // 1 bitmap byte + 4 bytes for id
        assert_eq!(loc.size, 4);

        let loc2 = compute_field_location(&schema, 2, &bitmap).unwrap(); // active
        assert_eq!(loc2.offset, 1 + 4 + 4); // bitmap + id + score
        assert_eq!(loc2.size, 1);
    }

    #[test]
    fn test_read_write_field() {
        let schema = [DataType::Int, DataType::Int, DataType::Bool];
        let original = vec![Value::Int(10), Value::Int(42), Value::Bool(true)];
        let mut encoded = encode_row(&original, &schema).unwrap();

        let bitmap = [0u8];
        let loc = compute_field_location(&schema, 1, &bitmap).unwrap();

        // Read score.
        let val = read_field(&encoded, &loc).unwrap();
        assert_eq!(val, Value::Int(42));

        // Write score = 43.
        let _old = write_field(&mut encoded, &loc, &Value::Int(43)).unwrap();

        // Verify by full decode.
        let decoded = decode_row(&encoded, &schema).unwrap();
        assert_eq!(decoded[0], Value::Int(10));
        assert_eq!(decoded[1], Value::Int(43)); // patched!
        assert_eq!(decoded[2], Value::Bool(true));
    }

    #[test]
    fn test_null_column_before_target() {
        let schema = [DataType::Int, DataType::Int];
        let mut bitmap = [0u8];
        bitmap[0] |= 1 << 0; // col 0 is NULL

        // Can still compute offset for col 1: bitmap(1) + 0 (col 0 is NULL) = offset 1
        let loc = compute_field_location(&schema, 1, &bitmap).unwrap();
        assert_eq!(loc.offset, 1); // bitmap only, col 0 takes 0 bytes
        assert_eq!(loc.size, 4);
    }
}
