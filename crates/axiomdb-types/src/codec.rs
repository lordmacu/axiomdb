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

use crate::{array_codec, types::DataType, value::Value};

/// Maximum byte length of an inline Text or Bytes value.
/// Matches the maximum representable by a u24 prefix (3 bytes LE).
const MAX_INLINE_LEN: usize = 0xFF_FFFF; // 16,777,215

// ── TOAST sentinels (Phase 11.2) ─────────────────────────────────────────────

/// u24 sentinel: value stored in uncompressed overflow chain.
/// The next 12 bytes after the u24 are: `[page_id:u64 LE][raw_len:u32 LE]`.
pub const TOAST_SENTINEL_RAW: usize = 0xFF_FFFE;

/// u24 sentinel: value stored in LZ4-compressed overflow chain.
/// Same layout as TOAST_SENTINEL_RAW; data in chain is LZ4 compressed.
pub const TOAST_SENTINEL_LZ4: usize = 0xFF_FFFD;

/// Maximum inline row size before TOAST triggers.
/// Chosen so that 2 tuples fit per 16 KB page with comfortable margin.
pub const TOAST_THRESHOLD: usize = 8_000;

/// Minimum value size to attempt LZ4 compression (smaller values don't benefit).
pub const TOAST_COMPRESS_MIN: usize = 256;

/// Encodes a TOAST pointer: `[u24 sentinel][page_id:u64 LE][raw_len:u32 LE]`.
/// Total: 3 + 8 + 4 = 15 bytes inline (replaces the original u24 + payload).
pub fn encode_toast_pointer(buf: &mut Vec<u8>, sentinel: usize, page_id: u64, raw_len: u32) {
    write_u24(buf, sentinel);
    buf.extend_from_slice(&page_id.to_le_bytes());
    buf.extend_from_slice(&raw_len.to_le_bytes());
}

/// Decodes a TOAST pointer from bytes at `pos` (after reading the u24 sentinel).
/// Returns `(page_id, raw_len)` and advances position by 12.
pub fn decode_toast_pointer(bytes: &[u8], pos: &mut usize) -> Result<(u64, u32), DbError> {
    ensure_bytes(bytes, *pos, 12)?;
    let page_id = u64::from_le_bytes(bytes[*pos..*pos + 8].try_into().unwrap());
    let raw_len = u32::from_le_bytes(bytes[*pos + 8..*pos + 12].try_into().unwrap());
    *pos += 12;
    Ok((page_id, raw_len))
}

/// Returns true if the u24 length is a TOAST sentinel.
pub fn is_toast_sentinel(len: usize) -> bool {
    len == TOAST_SENTINEL_RAW || len == TOAST_SENTINEL_LZ4
}

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
        (value, dt.clone()),
        (Value::Bool(_), DataType::Bool)
            | (Value::Int(_), DataType::Int)
            | (Value::BigInt(_), DataType::BigInt)
            | (Value::Real(_), DataType::Real)
            | (Value::Decimal(..), DataType::Decimal)
            | (Value::Text(_), DataType::Text)
            | (Value::Json(_), DataType::Json)
            | (Value::Jsonb(_), DataType::Jsonb)
            | (Value::Bytes(_), DataType::Bytes)
            | (Value::Date(_), DataType::Date)
            | (Value::Timestamp(_), DataType::Timestamp)
            | (Value::Uuid(_), DataType::Uuid)
            | (Value::Array(_), DataType::Array(_))
            | (Value::Range(_), DataType::Range(_))
            | (Value::Money(..), DataType::Money)
            | (Value::Composite(_), DataType::Composite(_))
            | (Value::Ltree(_), DataType::Ltree)
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

/// Infers the element DataType from the first non-null element of an array.
/// Defaults to `DataType::Int` if all elements are null or array is empty.
fn infer_array_elem_type(elems: &[Value]) -> crate::types::DataType {
    for elem in elems {
        match elem {
            Value::Null => continue,
            Value::Bool(_) => return crate::types::DataType::Bool,
            Value::Int(_) => return crate::types::DataType::Int,
            Value::BigInt(_) => return crate::types::DataType::BigInt,
            Value::Real(_) => return crate::types::DataType::Real,
            Value::Decimal(..) => return crate::types::DataType::Decimal,
            Value::Text(_) => return crate::types::DataType::Text,
            Value::Json(_) => return crate::types::DataType::Json,
            Value::Jsonb(_) => return crate::types::DataType::Jsonb,
            Value::Bytes(_) => return crate::types::DataType::Bytes,
            Value::Date(_) => return crate::types::DataType::Date,
            Value::Timestamp(_) => return crate::types::DataType::Timestamp,
            Value::Uuid(_) => return crate::types::DataType::Uuid,
            Value::Array(_) => {
                return crate::types::DataType::Array(Box::new(crate::types::DataType::Int))
            }
            Value::Range(_) => {
                return crate::types::DataType::Range(Box::new(crate::types::DataType::Int))
            }
            Value::Money(..) => return crate::types::DataType::Money,
            Value::Composite(_) => return crate::types::DataType::Composite(vec![]),
            Value::Ltree(_) => return crate::types::DataType::Ltree,
        }
    }
    crate::types::DataType::Int // default
}

/// Infers the `ColumnType` for wire-protocol array encoding from element values.
///
/// Used when column metadata does not carry array element type information
/// (e.g., computed expressions whose type was not resolved at analysis time).
pub fn infer_elem_ct_for_wire(elems: &[crate::value::Value]) -> crate::array_codec::ColumnType {
    use crate::array_codec::data_type_to_column_type;
    let dt = infer_array_elem_type(elems);
    data_type_to_column_type(&dt)
}

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
            Value::Text(s) | Value::Json(s) => 3 + s.len(),
            Value::Jsonb(b) => 3 + b.len(),
            Value::Bytes(b) => 3 + b.len(),
            Value::Array(elems) => {
                // Compute actual encoded length using encode_array.
                // Infer element type from the first non-null element (defaulting to Int).
                let elem_type =
                    array_codec::data_type_to_column_type(&infer_array_elem_type(elems));
                let array_value = Value::Array(elems.clone());
                let blob =
                    array_codec::encode_array(&array_value, elem_type).unwrap_or_else(|_| vec![]);
                3 + blob.len()
            }
            Value::Range(rv) => {
                // 1-byte flags + optional lower + optional upper (u24 length prefix)
                let payload = range_payload_len(rv);
                3 + payload
            }
            // Money: 16 bytes mantissa + 1 byte scale + 3 bytes currency = 20 bytes (fixed)
            Value::Money(..) => 20,
            // Ltree: 4-byte LE u32 length + UTF-8 bytes
            Value::Ltree(s) => 4 + s.len(),
            // Composite: 4-byte length + 1-byte field_count + N type_tags + field data
            Value::Composite(fields) => {
                let n = fields.len();
                let inner_len: usize = fields
                    .iter()
                    .map(|f| match f {
                        Value::Null => 0,
                        Value::Bool(_) => 1,
                        Value::Int(_) | Value::Date(_) => 4,
                        Value::BigInt(_) | Value::Real(_) | Value::Timestamp(_) => 8,
                        Value::Decimal(..) => 17,
                        Value::Uuid(_) => 16,
                        Value::Text(s) | Value::Json(s) => 3 + s.len(),
                        Value::Jsonb(b) => 3 + b.len(),
                        Value::Bytes(b) => 3 + b.len(),
                        Value::Money(..) => 20,
                        // Nested composite/array/ltree: overestimate
                        Value::Array(_) | Value::Range(_) | Value::Composite(_) | Value::Ltree(_) => 16,
                    })
                    .sum::<usize>()
                    + bitmap_len(n);
                4 + 1 + n + inner_len
            }
        })
        .sum();
    blen + data
}

// ── Composite field helpers ────────────────────────────────────────────────────

/// Returns the `ColumnType` tag for a composite field value.
/// Null fields get `Int` as a placeholder — the null bitmap records the null,
/// so the type tag is irrelevant during decode.
fn value_to_col_type(v: &Value) -> array_codec::ColumnType {
    match v {
        Value::Bool(_) => array_codec::ColumnType::Bool,
        Value::Int(_) => array_codec::ColumnType::Int,
        Value::BigInt(_) => array_codec::ColumnType::BigInt,
        Value::Real(_) => array_codec::ColumnType::Float,
        Value::Decimal(..) => array_codec::ColumnType::Decimal,
        Value::Text(_) => array_codec::ColumnType::Text,
        Value::Json(_) => array_codec::ColumnType::Json,
        Value::Jsonb(_) => array_codec::ColumnType::Jsonb,
        Value::Bytes(_) => array_codec::ColumnType::Bytes,
        Value::Date(_) => array_codec::ColumnType::Date,
        Value::Timestamp(_) => array_codec::ColumnType::Timestamp,
        Value::Uuid(_) => array_codec::ColumnType::Uuid,
        Value::Array(_) => array_codec::ColumnType::Array,
        Value::Range(_) => array_codec::ColumnType::Range,
        Value::Money(..) => array_codec::ColumnType::Money,
        Value::Ltree(_) => array_codec::ColumnType::Ltree,
        Value::Composite(_) | Value::Null => array_codec::ColumnType::Int,
    }
}

/// Converts a `ColumnType` tag to a `DataType` for use as an inner schema.
fn col_type_to_data_type(ct: array_codec::ColumnType) -> DataType {
    match ct {
        array_codec::ColumnType::Bool => DataType::Bool,
        array_codec::ColumnType::Int => DataType::Int,
        array_codec::ColumnType::BigInt => DataType::BigInt,
        array_codec::ColumnType::Float => DataType::Real,
        array_codec::ColumnType::Decimal => DataType::Decimal,
        array_codec::ColumnType::Text => DataType::Text,
        array_codec::ColumnType::Json => DataType::Json,
        array_codec::ColumnType::Jsonb => DataType::Jsonb,
        array_codec::ColumnType::Bytes => DataType::Bytes,
        array_codec::ColumnType::Date => DataType::Date,
        array_codec::ColumnType::Timestamp => DataType::Timestamp,
        array_codec::ColumnType::Uuid => DataType::Uuid,
        array_codec::ColumnType::Array => DataType::Array(Box::new(DataType::Int)),
        array_codec::ColumnType::Range => DataType::Range(Box::new(DataType::Int)),
        array_codec::ColumnType::Money => DataType::Money,
        array_codec::ColumnType::Composite => DataType::Composite(vec![]),
        array_codec::ColumnType::Ltree => DataType::Ltree,
    }
}

/// Decodes a self-describing composite blob (produced by the `encode_row` composite arm).
///
/// Format inside the 4-byte-length-prefixed wrapper:
/// ```text
/// 1 byte: N (field count)
/// N bytes: field ColumnType tags
/// remaining: encode_row(fields, derived_schema) bytes
/// ```
fn decode_composite_inner(inner: &[u8]) -> Result<Value, DbError> {
    if inner.is_empty() {
        return Ok(Value::Composite(vec![]));
    }
    let n = inner[0] as usize;
    if inner.len() < 1 + n {
        return Err(DbError::ParseError {
            message: "truncated composite: field type tags missing".into(),
            position: None,
        });
    }
    let inner_schema: Vec<DataType> = inner[1..1 + n]
        .iter()
        .map(|&tag| {
            let ct = array_codec::ColumnType::try_from(tag)?;
            Ok(col_type_to_data_type(ct))
        })
        .collect::<Result<_, DbError>>()?;
    let field_bytes = &inner[1 + n..];
    let inner_values = decode_row(field_bytes, &inner_schema)?;
    Ok(Value::Composite(inner_values))
}

// ── Public codec API ───────────────────────────────────────────────────────────

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
    for (i, (v, dt)) in values.iter().zip(schema.iter()).enumerate() {
        if v.is_null() {
            set_null_bit(&mut buf, i);
        } else {
            validate_type(v, dt.clone())?;
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
                // Phase 11.2e: NFC-normalize all Text before storage.
                // 'café' (NFD: 6 bytes) and 'café' (NFC: 5 bytes) become identical,
                // making '=' always correct for visually identical strings.
                // DuckDB does this; no other OLTP database does.
                use unicode_normalization::UnicodeNormalization;
                let normalized: String = s.nfc().collect();
                let bytes = normalized.as_bytes();
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
            Value::Json(s) => {
                // Phase 11.4: JSON stored as validated UTF-8 text (same wire format as Text).
                use unicode_normalization::UnicodeNormalization;
                let normalized: String = s.nfc().collect();
                serde_json::from_str::<serde_json::Value>(&normalized).map_err(|e| {
                    DbError::InvalidValue {
                        reason: format!("invalid JSON: {e}"),
                    }
                })?;
                let bytes = normalized.as_bytes();
                if bytes.len() > MAX_INLINE_LEN {
                    return Err(DbError::ValueTooLarge {
                        len: bytes.len(),
                        max: MAX_INLINE_LEN,
                    });
                }
                write_u24(&mut buf, bytes.len());
                buf.extend_from_slice(bytes);
            }
            Value::Jsonb(blob) => {
                // Phase 11.16: binary JSONB stored as u24 length + raw binary bytes.
                let bytes: &[u8] = blob.as_ref();
                if bytes.len() > MAX_INLINE_LEN {
                    return Err(DbError::ValueTooLarge {
                        len: bytes.len(),
                        max: MAX_INLINE_LEN,
                    });
                }
                write_u24(&mut buf, bytes.len());
                buf.extend_from_slice(bytes);
            }
            Value::Date(d) => buf.extend_from_slice(&d.to_le_bytes()),
            Value::Timestamp(t) => buf.extend_from_slice(&t.to_le_bytes()),
            Value::Uuid(u) => buf.extend_from_slice(u),
            Value::Array(_elems) => {
                // Get element type from DataType::Array in schema
                if let DataType::Array(ref elem_dt) = schema[i] {
                    let elem_type = array_codec::data_type_to_column_type(elem_dt);
                    let array_blob = array_codec::encode_array(v, elem_type)?;
                    write_u24(&mut buf, array_blob.len());
                    buf.extend_from_slice(&array_blob);
                } else {
                    return Err(DbError::TypeMismatch {
                        expected: "Array".to_string(),
                        got: schema[i].name(),
                    });
                }
            }
            Value::Range(rv) => {
                if let DataType::Range(ref elem_dt) = schema[i] {
                    let payload = encode_range_payload(rv, elem_dt)?;
                    write_u24(&mut buf, payload.len());
                    buf.extend_from_slice(&payload);
                } else {
                    return Err(DbError::TypeMismatch {
                        expected: "Range".to_string(),
                        got: schema[i].name(),
                    });
                }
            }
            Value::Money(m, s, c) => {
                buf.extend_from_slice(&m.to_le_bytes());
                buf.push(*s);
                buf.extend_from_slice(c);
            }
            Value::Composite(fields) => {
                // Self-describing encoding: [1 byte N][N type_tags][encode_row bytes]
                // Field types are stored inline so decode works without catalog access.
                let n = fields.len();
                let type_tags: Vec<u8> = fields
                    .iter()
                    .map(|f| u8::from(value_to_col_type(f)))
                    .collect();
                let inner_schema: Vec<DataType> = fields
                    .iter()
                    .map(|f| col_type_to_data_type(value_to_col_type(f)))
                    .collect();
                let field_bytes = encode_row(fields, &inner_schema)?;
                let mut inner = Vec::with_capacity(1 + n + field_bytes.len());
                inner.push(n as u8);
                inner.extend_from_slice(&type_tags);
                inner.extend_from_slice(&field_bytes);
                let data_len = inner.len() as u32;
                buf.extend_from_slice(&data_len.to_le_bytes());
                buf.extend_from_slice(&inner);
            }
            Value::Ltree(s) => {
                let bytes = s.as_bytes();
                let data_len = bytes.len() as u32;
                buf.extend_from_slice(&data_len.to_le_bytes());
                buf.extend_from_slice(bytes);
            }
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

    for (i, dt) in schema.iter().enumerate() {
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
                if is_toast_sentinel(len) {
                    // TOAST pointer — skip the 12-byte pointer. The caller must
                    // resolve the overflow chain via storage. For now, return a
                    // placeholder indicating the value is external.
                    ensure_bytes(bytes, pos, 12)?;
                    let page_id = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
                    let raw_len = u32::from_le_bytes(bytes[pos + 8..pos + 12].try_into().unwrap());
                    pos += 12;
                    // Placeholder: the de-TOAST layer replaces this before user sees it.
                    Value::Text(format!(
                        "__toast__:{}:{}:{}",
                        page_id,
                        len == TOAST_SENTINEL_LZ4,
                        raw_len
                    ))
                } else {
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
            }
            DataType::Bytes => {
                let len = read_u24(bytes, pos)?;
                pos += 3;
                if is_toast_sentinel(len) {
                    ensure_bytes(bytes, pos, 12)?;
                    let page_id = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
                    let raw_len = u32::from_le_bytes(bytes[pos + 8..pos + 12].try_into().unwrap());
                    pos += 12;
                    Value::Bytes(
                        format!(
                            "__toast__:{}:{}:{}",
                            page_id,
                            len == TOAST_SENTINEL_LZ4,
                            raw_len
                        )
                        .into_bytes(),
                    )
                } else {
                    ensure_bytes(bytes, pos, len)?;
                    let b = bytes[pos..pos + len].to_vec();
                    pos += len;
                    Value::Bytes(b)
                }
            }
            DataType::Json => {
                // Phase 11.4: JSON decoded as Text wire format → Value::Json.
                let len = read_u24(bytes, pos)?;
                pos += 3;
                if is_toast_sentinel(len) {
                    let (page_id, raw_len) = decode_toast_pointer(bytes, &mut pos)?;
                    Value::Json(format!(
                        "__toast__:{}:{}:{}",
                        page_id,
                        len == TOAST_SENTINEL_LZ4,
                        raw_len
                    ))
                } else {
                    ensure_bytes(bytes, pos, len)?;
                    let s = std::str::from_utf8(&bytes[pos..pos + len])
                        .map_err(|_| DbError::ParseError {
                            message: format!("invalid UTF-8 in JSON column at offset {pos}"),
                            position: None,
                        })?
                        .to_string();
                    pos += len;
                    Value::Json(s)
                }
            }
            DataType::Jsonb => {
                // Phase 11.16: binary JSONB blob, u24 length prefix + raw bytes.
                let len = read_u24(bytes, pos)?;
                pos += 3;
                if is_toast_sentinel(len) {
                    let (page_id, raw_len) = decode_toast_pointer(bytes, &mut pos)?;
                    // TOAST placeholder: encode as empty JSONB blob with a placeholder string.
                    // The de-TOAST layer resolves this before the user sees it.
                    let placeholder = format!(
                        "__toast__:{}:{}:{}",
                        page_id,
                        len == TOAST_SENTINEL_LZ4,
                        raw_len
                    );
                    // Store the placeholder as a JSON text value for the TOAST layer.
                    Value::Json(placeholder)
                } else {
                    ensure_bytes(bytes, pos, len)?;
                    let blob = bytes[pos..pos + len].to_vec();
                    pos += len;
                    Value::Jsonb(std::sync::Arc::new(blob))
                }
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
            DataType::Array(_elem) => {
                // Array decoding with array_codec (Step 2).
                let len = read_u24(bytes, pos)?;
                pos += 3;
                if is_toast_sentinel(len) {
                    let (_page_id, _raw_len) = decode_toast_pointer(bytes, &mut pos)?;
                    Value::Array(vec![])
                } else {
                    ensure_bytes(bytes, pos, len)?;
                    let blob = &bytes[pos..pos + len];
                    pos += len;
                    // decode_array returns (Value::Array, elem_type, ndim)
                    let (arr, _, _) = array_codec::decode_array(blob)?;
                    arr
                }
            }
            DataType::Range(elem_dt) => {
                let len = read_u24(bytes, pos)?;
                pos += 3;
                ensure_bytes(bytes, pos, len)?;
                let blob = &bytes[pos..pos + len];
                pos += len;
                Value::Range(Box::new(decode_range_payload(blob, elem_dt)?))
            }
            DataType::Money => {
                ensure_bytes(bytes, pos, 20)?;
                let m = i128::from_le_bytes(bytes[pos..pos + 16].try_into().unwrap());
                let s = bytes[pos + 16];
                let mut c = [0u8; 3];
                c.copy_from_slice(&bytes[pos + 17..pos + 20]);
                pos += 20;
                Value::Money(m, s, c)
            }
            DataType::Composite(_) => {
                ensure_bytes(bytes, pos, 4)?;
                let data_len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
                pos += 4;
                ensure_bytes(bytes, pos, data_len)?;
                let inner = &bytes[pos..pos + data_len];
                pos += data_len;
                decode_composite_inner(inner)?
            }
            DataType::Ltree => {
                ensure_bytes(bytes, pos, 4)?;
                let data_len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
                pos += 4;
                ensure_bytes(bytes, pos, data_len)?;
                let s = std::str::from_utf8(&bytes[pos..pos + data_len])
                    .map_err(|_| DbError::ParseError {
                        message: format!("invalid UTF-8 in Ltree column at offset {pos}"),
                        position: None,
                    })?
                    .to_string();
                pos += data_len;
                Value::Ltree(s)
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

    for (i, dt) in schema.iter().enumerate() {
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
                DataType::Text | DataType::Json => {
                    let len = read_u24(bytes, pos)?;
                    pos += 3;
                    if is_toast_sentinel(len) {
                        let (page_id, raw_len) = decode_toast_pointer(bytes, &mut pos)?;
                        let placeholder = format!(
                            "__toast__:{}:{}:{}",
                            page_id,
                            len == TOAST_SENTINEL_LZ4,
                            raw_len
                        );
                        if *dt == DataType::Json {
                            Value::Json(placeholder)
                        } else {
                            Value::Text(placeholder)
                        }
                    } else {
                        ensure_bytes(bytes, pos, len)?;
                        let s = std::str::from_utf8(&bytes[pos..pos + len])
                            .map_err(|_| DbError::ParseError {
                                message: format!("invalid UTF-8 in Text column at offset {pos}"),
                                position: None,
                            })?
                            .to_string();
                        pos += len;
                        if *dt == DataType::Json {
                            Value::Json(s)
                        } else {
                            Value::Text(s)
                        }
                    }
                }
                DataType::Jsonb => {
                    let len = read_u24(bytes, pos)?;
                    pos += 3;
                    if is_toast_sentinel(len) {
                        let (page_id, raw_len) = decode_toast_pointer(bytes, &mut pos)?;
                        let placeholder = format!(
                            "__toast__:{}:{}:{}",
                            page_id,
                            len == TOAST_SENTINEL_LZ4,
                            raw_len
                        );
                        Value::Json(placeholder)
                    } else {
                        ensure_bytes(bytes, pos, len)?;
                        let blob = bytes[pos..pos + len].to_vec();
                        pos += len;
                        Value::Jsonb(std::sync::Arc::new(blob))
                    }
                }
                DataType::Bytes => {
                    let len = read_u24(bytes, pos)?;
                    pos += 3;
                    if is_toast_sentinel(len) {
                        let (page_id, raw_len) = decode_toast_pointer(bytes, &mut pos)?;
                        Value::Bytes(
                            format!(
                                "__toast__:{}:{}:{}",
                                page_id,
                                len == TOAST_SENTINEL_LZ4,
                                raw_len
                            )
                            .into_bytes(),
                        )
                    } else {
                        ensure_bytes(bytes, pos, len)?;
                        let b = bytes[pos..pos + len].to_vec();
                        pos += len;
                        Value::Bytes(b)
                    }
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
                DataType::Array(_elem) => {
                    let len = read_u24(bytes, pos)?;
                    pos += 3;
                    if is_toast_sentinel(len) {
                        let (_page_id, _raw_len) = decode_toast_pointer(bytes, &mut pos)?;
                        Value::Array(vec![])
                    } else {
                        ensure_bytes(bytes, pos, len)?;
                        let blob = &bytes[pos..pos + len];
                        pos += len;
                        let (arr, _, _) = array_codec::decode_array(blob)?;
                        arr
                    }
                }
                DataType::Range(elem_dt) => {
                    let len = read_u24(bytes, pos)?;
                    pos += 3;
                    ensure_bytes(bytes, pos, len)?;
                    let blob = &bytes[pos..pos + len];
                    pos += len;
                    Value::Range(Box::new(decode_range_payload(blob, elem_dt)?))
                }
                DataType::Money => {
                    ensure_bytes(bytes, pos, 20)?;
                    let m = i128::from_le_bytes(bytes[pos..pos + 16].try_into().unwrap());
                    let s = bytes[pos + 16];
                    let mut c = [0u8; 3];
                    c.copy_from_slice(&bytes[pos + 17..pos + 20]);
                    pos += 20;
                    Value::Money(m, s, c)
                }
                DataType::Composite(_) => {
                    ensure_bytes(bytes, pos, 4)?;
                    let data_len =
                        u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
                    pos += 4;
                    ensure_bytes(bytes, pos, data_len)?;
                    let inner = &bytes[pos..pos + data_len];
                    pos += data_len;
                    decode_composite_inner(inner)?
                }
                DataType::Ltree => {
                    ensure_bytes(bytes, pos, 4)?;
                    let data_len =
                        u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
                    pos += 4;
                    ensure_bytes(bytes, pos, data_len)?;
                    let s = std::str::from_utf8(&bytes[pos..pos + data_len])
                        .map_err(|_| DbError::ParseError {
                            message: format!("invalid UTF-8 in Ltree column at offset {pos}"),
                            position: None,
                        })?
                        .to_string();
                    pos += data_len;
                    Value::Ltree(s)
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
                DataType::Money => {
                    ensure_bytes(bytes, pos, 20)?;
                    pos += 20;
                }
                DataType::Composite(_) | DataType::Ltree => {
                    // Skip: read u32 data_len then skip that many bytes.
                    ensure_bytes(bytes, pos, 4)?;
                    let data_len =
                        u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
                    pos += 4;
                    ensure_bytes(bytes, pos, data_len)?;
                    pos += data_len;
                }
                DataType::Text
                | DataType::Json
                | DataType::Jsonb
                | DataType::Bytes
                | DataType::Array(_)
                | DataType::Range(_) => {
                    let len = read_u24(bytes, pos)?;
                    pos += 3;
                    if is_toast_sentinel(len) {
                        let (_page_id, _raw_len) = decode_toast_pointer(bytes, &mut pos)?;
                    } else {
                        ensure_bytes(bytes, pos, len)?;
                        pos += len; // skip payload — no copy, no allocation
                    }
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

// ── Range codec helpers (Phase 20.13) ─────────────────────────────────────────

use crate::range_value::{
    RangeValue, RANGE_EMPTY, RANGE_LOWER_BOUNDED, RANGE_LOWER_INC, RANGE_UPPER_BOUNDED,
    RANGE_UPPER_INC,
};

/// Fixed encoded size of one bound value for a given element DataType.
fn range_bound_size(elem_dt: &DataType) -> usize {
    match elem_dt {
        DataType::Int | DataType::Date => 4,
        DataType::BigInt | DataType::Timestamp => 8,
        DataType::Decimal => 17,
        _ => 0,
    }
}

/// Returns the byte length of the range payload (flags + optional bounds).
fn range_payload_len(rv: &RangeValue) -> usize {
    if rv.is_empty {
        return 1; // just the EMPTY flags byte
    }
    // We need to know elem_dt to compute bound sizes, but encoded_len is
    // schema-free. Use a conservative upper bound of 17 bytes per bound.
    // This may over-estimate but won't under-estimate.
    1 + if rv.lower.is_some() { 17 } else { 0 } + if rv.upper.is_some() { 17 } else { 0 }
}

/// Encodes one bound value into the buffer according to elem_dt.
fn encode_bound(buf: &mut Vec<u8>, val: &Value, elem_dt: &DataType) -> Result<(), DbError> {
    match (val, elem_dt) {
        (Value::Int(n), DataType::Int) => buf.extend_from_slice(&n.to_le_bytes()),
        (Value::Date(d), DataType::Date) => buf.extend_from_slice(&d.to_le_bytes()),
        (Value::BigInt(n), DataType::BigInt) => buf.extend_from_slice(&n.to_le_bytes()),
        (Value::Timestamp(t), DataType::Timestamp) => buf.extend_from_slice(&t.to_le_bytes()),
        (Value::Decimal(m, s), DataType::Decimal) => {
            buf.extend_from_slice(&m.to_le_bytes());
            buf.push(*s);
        }
        _ => {
            return Err(DbError::TypeMismatch {
                expected: elem_dt.name(),
                got: val.variant_name().to_string(),
            })
        }
    }
    Ok(())
}

/// Decodes one bound value from `bytes[pos..]`.
fn decode_bound(bytes: &[u8], pos: &mut usize, elem_dt: &DataType) -> Result<Value, DbError> {
    match elem_dt {
        DataType::Int => {
            ensure_bytes(bytes, *pos, 4)?;
            let v = i32::from_le_bytes(bytes[*pos..*pos + 4].try_into().unwrap());
            *pos += 4;
            Ok(Value::Int(v))
        }
        DataType::Date => {
            ensure_bytes(bytes, *pos, 4)?;
            let v = i32::from_le_bytes(bytes[*pos..*pos + 4].try_into().unwrap());
            *pos += 4;
            Ok(Value::Date(v))
        }
        DataType::BigInt => {
            ensure_bytes(bytes, *pos, 8)?;
            let v = i64::from_le_bytes(bytes[*pos..*pos + 8].try_into().unwrap());
            *pos += 8;
            Ok(Value::BigInt(v))
        }
        DataType::Timestamp => {
            ensure_bytes(bytes, *pos, 8)?;
            let v = i64::from_le_bytes(bytes[*pos..*pos + 8].try_into().unwrap());
            *pos += 8;
            Ok(Value::Timestamp(v))
        }
        DataType::Decimal => {
            ensure_bytes(bytes, *pos, 17)?;
            let m = i128::from_le_bytes(bytes[*pos..*pos + 16].try_into().unwrap());
            let s = bytes[*pos + 16];
            *pos += 17;
            Ok(Value::Decimal(m, s))
        }
        _ => Err(DbError::ParseError {
            message: format!("unsupported range element type: {}", elem_dt.name()),
            position: None,
        }),
    }
}

/// Encodes a `RangeValue` into a binary payload blob.
pub(crate) fn encode_range_payload(
    rv: &RangeValue,
    elem_dt: &DataType,
) -> Result<Vec<u8>, DbError> {
    let bound_sz = range_bound_size(elem_dt);
    let cap = 1
        + if rv.lower.is_some() { bound_sz } else { 0 }
        + if rv.upper.is_some() { bound_sz } else { 0 };
    let mut buf = Vec::with_capacity(cap);

    let mut flags: u8 = 0;
    if rv.is_empty {
        flags |= RANGE_EMPTY;
        buf.push(flags);
        return Ok(buf);
    }
    if rv.lower.is_some() {
        flags |= RANGE_LOWER_BOUNDED;
    }
    if rv.upper.is_some() {
        flags |= RANGE_UPPER_BOUNDED;
    }
    if rv.lower_inc {
        flags |= RANGE_LOWER_INC;
    }
    if rv.upper_inc {
        flags |= RANGE_UPPER_INC;
    }
    buf.push(flags);

    if let Some(ref lo) = rv.lower {
        encode_bound(&mut buf, lo, elem_dt)?;
    }
    if let Some(ref hi) = rv.upper {
        encode_bound(&mut buf, hi, elem_dt)?;
    }
    Ok(buf)
}

/// Decodes a binary range payload back into a `RangeValue`.
pub(crate) fn decode_range_payload(
    bytes: &[u8],
    elem_dt: &DataType,
) -> Result<RangeValue, DbError> {
    if bytes.is_empty() {
        return Err(DbError::ParseError {
            message: "empty range payload".into(),
            position: None,
        });
    }
    let flags = bytes[0];
    let mut pos = 1usize;

    if flags & RANGE_EMPTY != 0 {
        return Ok(RangeValue::empty());
    }

    let lower = if flags & RANGE_LOWER_BOUNDED != 0 {
        Some(decode_bound(bytes, &mut pos, elem_dt)?)
    } else {
        None
    };
    let upper = if flags & RANGE_UPPER_BOUNDED != 0 {
        Some(decode_bound(bytes, &mut pos, elem_dt)?)
    } else {
        None
    };
    let lower_inc = flags & RANGE_LOWER_INC != 0;
    let upper_inc = flags & RANGE_UPPER_INC != 0;

    Ok(RangeValue {
        lower,
        upper,
        lower_inc,
        upper_inc,
        is_empty: false,
    })
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
