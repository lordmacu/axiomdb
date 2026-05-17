//! Binary codec for SQL array values — PostgreSQL-compatible on-disk format.
//!
//! ## On-disk blob format
//!
//! ```text
//! Byte layout:
//!   [total_len: u32 LE]    — total bytes of this array blob (including self)
//!   [ndim: i32 LE]          — number of dimensions (1-6, or 0=empty array)
//!   [dataoffset: i32 LE]    — 0 if no null elements; else offset from start of blob to element data
//!   [elemtype: u8]          — ColumnType discriminant of elements
//!   [dims[ndim]: i32 LE]   — dimension lengths, row-major order
//!   [lbound[ndim]: i32 LE]  — lower bounds (default all 1)
//!   [null_bitmap]           — ceil(nitems/8) bytes, present ONLY if dataoffset != 0
//!   [elements: u8[]]        — packed element values
//! ```
//!
//! Design rules:
//! - `total_len` is u32 — arrays can exceed 16MB
//! - `dataoffset = 0` when no null elements (null bitmap omitted)
//! - Row-major: last subscript varies fastest
//! - Empty array: ndim=0, dims=[], lbound=[], elements=[]
//! - Maximum dimensions: 6

use axiomdb_core::error::DbError;

use crate::value::Value;

// ── ColumnType (copied from axiomdb-catalog to avoid cyclic dependency) ──────────

/// SQL column type discriminant — mirrors `axiomdb_catalog::schema_database::ColumnType`.
///
/// Kept as a local definition here to avoid a cyclic dependency between
/// `axiomdb-types` and `axiomdb-catalog`. The numeric values must match the
/// catalog definition exactly.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Bool = 1,
    Int = 2,
    BigInt = 3,
    Float = 4, // f64
    Text = 5,
    Bytes = 6,
    Timestamp = 7,  // i64 microseconds
    Uuid = 8,       // [u8; 16]
    Json = 9,       // validated UTF-8 JSON text
    Jsonb = 10,     // binary JSONB blob
    Decimal = 11,   // i128 mantissa + u8 scale
    Date = 12,      // i32 days since 1970-01-01
    Array = 13,     // PostgreSQL array
    Range = 14,     // SQL range type
    Money = 15,     // SQL MONEY type (Phase 20.17)
    Composite = 16, // SQL composite type (Phase 20.18)
    Ltree = 17,     // SQL ltree hierarchical path (Phase 20.19)
    Xml = 18,       // SQL XML / XMLTYPE (Phase 20.20)
    TinyInt = 19,   // SQL TINYINT — i8 range, wire 0x01 TINY (Phase 24.1)
    SmallInt = 20,  // SQL SMALLINT — i16 range, wire 0x02 SHORT (Phase 24.1)
    Float32 = 21,   // SQL REAL/FLOAT4 — 4-byte f32 LE, wire 0x04 FLOAT (Phase 24.2)
}

impl TryFrom<u8> for ColumnType {
    type Error = DbError;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            1 => Ok(Self::Bool),
            2 => Ok(Self::Int),
            3 => Ok(Self::BigInt),
            4 => Ok(Self::Float),
            5 => Ok(Self::Text),
            6 => Ok(Self::Bytes),
            7 => Ok(Self::Timestamp),
            8 => Ok(Self::Uuid),
            9 => Ok(Self::Json),
            10 => Ok(Self::Jsonb),
            11 => Ok(Self::Decimal),
            12 => Ok(Self::Date),
            13 => Ok(Self::Array),
            14 => Ok(Self::Range),
            15 => Ok(Self::Money),
            16 => Ok(Self::Composite),
            17 => Ok(Self::Ltree),
            18 => Ok(Self::Xml),
            19 => Ok(Self::TinyInt),
            20 => Ok(Self::SmallInt),
            21 => Ok(Self::Float32),
            _ => Err(DbError::ParseError {
                message: format!("unknown ColumnType discriminant: {v}"),
                position: None,
            }),
        }
    }
}

impl From<ColumnType> for u8 {
    fn from(c: ColumnType) -> u8 {
        c as u8
    }
}

// ── Constants ─────────────────────────────────────────────────────────────────

/// Maximum supported array dimensions.
const MAX_ARRAY_DIMS: i32 = 6;

/// Maximum number of elements (2^31 - 1 per PG).
const MAX_ARRAY_ELEMENTS: i32 = i32::MAX;

/// Codec-level maximum inline array size (u24 limit, matching TEXT/BYTES).
/// Arrays larger than this should be stored in TOAST overflow pages.
const MAX_ARRAY_INLINE: usize = 0xFF_FFFF; // 16,777,215

// ── DataType → ColumnType mapping ───────────────────────────────────────────

/// Maps a scalar `DataType` (non-Array) to its corresponding `ColumnType` discriminant.
pub fn data_type_to_column_type(dt: &crate::types::DataType) -> ColumnType {
    match dt {
        crate::types::DataType::Bool => ColumnType::Bool,
        crate::types::DataType::TinyInt => ColumnType::TinyInt,
        crate::types::DataType::SmallInt => ColumnType::SmallInt,
        crate::types::DataType::Int => ColumnType::Int,
        crate::types::DataType::BigInt => ColumnType::BigInt,
        crate::types::DataType::Float => ColumnType::Float32,
        crate::types::DataType::Real => ColumnType::Float,
        crate::types::DataType::Text => ColumnType::Text,
        crate::types::DataType::Bytes => ColumnType::Bytes,
        crate::types::DataType::Timestamp => ColumnType::Timestamp,
        crate::types::DataType::Uuid => ColumnType::Uuid,
        crate::types::DataType::Json => ColumnType::Json,
        crate::types::DataType::Jsonb => ColumnType::Jsonb,
        crate::types::DataType::Decimal => ColumnType::Decimal,
        crate::types::DataType::Date => ColumnType::Date,
        crate::types::DataType::Array(_) => ColumnType::Array,
        crate::types::DataType::Range(_) => ColumnType::Range,
        crate::types::DataType::Money => ColumnType::Money,
        crate::types::DataType::Composite(_) => ColumnType::Composite,
        crate::types::DataType::Ltree => ColumnType::Ltree,
        crate::types::DataType::Xml => ColumnType::Xml,
    }
}

// ── Element encoding helpers ──────────────────────────────────────────────────

/// Encodes a single scalar element value into `buf` using `elem_type`.
///
/// Returns the number of bytes written.
fn encode_element(
    elem: &Value,
    elem_type: ColumnType,
    buf: &mut Vec<u8>,
) -> Result<usize, DbError> {
    let start_len = buf.len();
    match elem_type {
        ColumnType::Bool => {
            let b = match elem {
                Value::Bool(b) => *b,
                Value::Null => {
                    buf.push(0);
                    return Ok(1);
                }
                _ => {
                    return Err(DbError::TypeMismatch {
                        expected: "BOOL".to_string(),
                        got: elem.variant_name().to_string(),
                    });
                }
            };
            buf.push(if b { 1 } else { 0 });
        }
        ColumnType::TinyInt | ColumnType::SmallInt | ColumnType::Int | ColumnType::Date => {
            let n = match elem {
                Value::Int(n) => *n,
                Value::Date(d) => *d,
                Value::BigInt(n) => (*n).try_into().map_err(|_| DbError::TypeMismatch {
                    expected: "INT".to_string(),
                    got: "BIGINT out of range".to_string(),
                })?,
                Value::Null => {
                    buf.extend_from_slice(&0i32.to_le_bytes());
                    return Ok(4);
                }
                _ => {
                    return Err(DbError::TypeMismatch {
                        expected: "INT".to_string(),
                        got: elem.variant_name().to_string(),
                    });
                }
            };
            buf.extend_from_slice(&n.to_le_bytes());
        }
        ColumnType::Float32 => match elem {
            Value::Real(f) => {
                if f.is_nan() {
                    return Err(DbError::InvalidValue {
                        reason: "NaN is not a valid SQL real value".into(),
                    });
                }
                buf.extend_from_slice(&(*f as f32).to_le_bytes());
            }
            Value::Null => {
                buf.extend_from_slice(&0f32.to_le_bytes());
                return Ok(4);
            }
            _ => {
                return Err(DbError::TypeMismatch {
                    expected: "REAL".to_string(),
                    got: elem.variant_name().to_string(),
                });
            }
        },
        ColumnType::BigInt | ColumnType::Float | ColumnType::Timestamp => match elem {
            Value::BigInt(n) => buf.extend_from_slice(&n.to_le_bytes()),
            Value::Real(f) => {
                if f.is_nan() {
                    return Err(DbError::InvalidValue {
                        reason: "NaN is not a valid SQL real value".into(),
                    });
                }
                buf.extend_from_slice(&f.to_le_bytes());
            }
            Value::Timestamp(t) => buf.extend_from_slice(&t.to_le_bytes()),
            Value::Null => {
                buf.extend_from_slice(&0i64.to_le_bytes());
                return Ok(8);
            }
            _ => {
                return Err(DbError::TypeMismatch {
                    expected: "BIGINT/REAL/TIMESTAMP".to_string(),
                    got: elem.variant_name().to_string(),
                });
            }
        },
        ColumnType::Decimal => match elem {
            Value::Decimal(m, s) => {
                buf.extend_from_slice(&m.to_le_bytes());
                buf.push(*s);
            }
            Value::Null => {
                buf.extend_from_slice(&0i128.to_le_bytes());
                buf.push(0);
            }
            _ => {
                return Err(DbError::TypeMismatch {
                    expected: "DECIMAL".to_string(),
                    got: elem.variant_name().to_string(),
                });
            }
        },
        ColumnType::Uuid => match elem {
            Value::Uuid(u) => buf.extend_from_slice(u),
            Value::Null => buf.extend_from_slice(&[0u8; 16]),
            _ => {
                return Err(DbError::TypeMismatch {
                    expected: "UUID".to_string(),
                    got: elem.variant_name().to_string(),
                });
            }
        },
        ColumnType::Text | ColumnType::Json => match elem {
            Value::Text(s) | Value::Json(s) => {
                use unicode_normalization::UnicodeNormalization;
                let normalized: String = s.nfc().collect();
                let bytes = normalized.as_bytes();
                if bytes.len() > MAX_ARRAY_INLINE {
                    return Err(DbError::ValueTooLarge {
                        len: bytes.len(),
                        max: MAX_ARRAY_INLINE,
                    });
                }
                buf.push((bytes.len() & 0xFF) as u8);
                buf.push(((bytes.len() >> 8) & 0xFF) as u8);
                buf.push(((bytes.len() >> 16) & 0xFF) as u8);
                buf.extend_from_slice(bytes);
            }
            Value::Null => {
                buf.push(0);
                buf.push(0);
                buf.push(0);
            }
            _ => {
                return Err(DbError::TypeMismatch {
                    expected: "TEXT".to_string(),
                    got: elem.variant_name().to_string(),
                });
            }
        },
        ColumnType::Bytes => match elem {
            Value::Bytes(b) => {
                if b.len() > MAX_ARRAY_INLINE {
                    return Err(DbError::ValueTooLarge {
                        len: b.len(),
                        max: MAX_ARRAY_INLINE,
                    });
                }
                buf.push((b.len() & 0xFF) as u8);
                buf.push(((b.len() >> 8) & 0xFF) as u8);
                buf.push(((b.len() >> 16) & 0xFF) as u8);
                buf.extend_from_slice(b);
            }
            Value::Null => {
                buf.push(0);
                buf.push(0);
                buf.push(0);
            }
            _ => {
                return Err(DbError::TypeMismatch {
                    expected: "BYTES".to_string(),
                    got: elem.variant_name().to_string(),
                });
            }
        },
        ColumnType::Jsonb => match elem {
            Value::Jsonb(blob) => {
                let bytes: &[u8] = blob.as_ref();
                if bytes.len() > MAX_ARRAY_INLINE {
                    return Err(DbError::ValueTooLarge {
                        len: bytes.len(),
                        max: MAX_ARRAY_INLINE,
                    });
                }
                buf.push((bytes.len() & 0xFF) as u8);
                buf.push(((bytes.len() >> 8) & 0xFF) as u8);
                buf.push(((bytes.len() >> 16) & 0xFF) as u8);
                buf.extend_from_slice(bytes);
            }
            Value::Null => {
                buf.push(0);
                buf.push(0);
                buf.push(0);
            }
            _ => {
                return Err(DbError::TypeMismatch {
                    expected: "JSONB".to_string(),
                    got: elem.variant_name().to_string(),
                });
            }
        },
        ColumnType::Array => {
            return Err(DbError::InvalidValue {
                reason: "Array element type cannot itself be an array in this implementation"
                    .to_string(),
            });
        }
        ColumnType::Range => {
            return Err(DbError::InvalidValue {
                reason: "Range type cannot be used as an array element type".to_string(),
            });
        }
        ColumnType::Money => {
            return Err(DbError::InvalidValue {
                reason: "Money type cannot be used as an array element type".to_string(),
            });
        }
        ColumnType::Composite => {
            return Err(DbError::InvalidValue {
                reason: "Composite type cannot be used as an array element type".to_string(),
            });
        }
        ColumnType::Ltree => {
            return Err(DbError::InvalidValue {
                reason: "Ltree type cannot be used as an array element type".to_string(),
            });
        }
        ColumnType::Xml => {
            return Err(DbError::InvalidValue {
                reason: "Xml type cannot be used as an array element type".to_string(),
            });
        }
    }
    Ok(buf.len() - start_len)
}

/// Decodes a single scalar element from `blob[pos..]` using `elem_type`.
///
/// Returns `(decoded_value, bytes_consumed)`.
fn decode_element(
    blob: &[u8],
    pos: &mut usize,
    elem_type: ColumnType,
) -> Result<(Value, usize), DbError> {
    // Infallible slice-to-array conversion: all callers check blob bounds before calling
    // this helper, so the slice is always exactly the expected length.
    let slice_err = || DbError::ParseError {
        message: "internal: fixed-size slice conversion (bounds checked above)".into(),
        position: None,
    };
    match elem_type {
        ColumnType::Bool => {
            if *pos + 1 > blob.len() {
                return Err(DbError::ParseError {
                    message: "truncated: expected 1 byte for Bool element".to_string(),
                    position: None,
                });
            }
            let b = blob[*pos] != 0;
            *pos += 1;
            Ok((Value::Bool(b), 1))
        }
        ColumnType::TinyInt | ColumnType::SmallInt | ColumnType::Int | ColumnType::Date => {
            if *pos + 4 > blob.len() {
                return Err(DbError::ParseError {
                    message: "truncated: expected 4 bytes for Int/Date element".to_string(),
                    position: None,
                });
            }
            let n = i32::from_le_bytes(blob[*pos..*pos + 4].try_into().map_err(|_| slice_err())?);
            *pos += 4;
            let value = match elem_type {
                ColumnType::Date => Value::Date(n),
                _ => Value::Int(n),
            };
            Ok((value, 4))
        }
        ColumnType::Float32 => {
            if *pos + 4 > blob.len() {
                return Err(DbError::ParseError {
                    message: "truncated: expected 4 bytes for Float32 element".to_string(),
                    position: None,
                });
            }
            let f = f32::from_le_bytes(blob[*pos..*pos + 4].try_into().map_err(|_| slice_err())?);
            *pos += 4;
            Ok((Value::Real(f as f64), 4))
        }
        ColumnType::BigInt | ColumnType::Float | ColumnType::Timestamp => {
            if *pos + 8 > blob.len() {
                return Err(DbError::ParseError {
                    message: "truncated: expected 8 bytes for BigInt/Real/Timestamp element"
                        .to_string(),
                    position: None,
                });
            }
            *pos += 8;
            match elem_type {
                ColumnType::Float => {
                    let f = f64::from_le_bytes(
                        blob[*pos - 8..*pos].try_into().map_err(|_| slice_err())?,
                    );
                    Ok((Value::Real(f), 8))
                }
                ColumnType::Timestamp => {
                    let v = i64::from_le_bytes(
                        blob[*pos - 8..*pos].try_into().map_err(|_| slice_err())?,
                    );
                    Ok((Value::Timestamp(v), 8))
                }
                _ => {
                    let v = i64::from_le_bytes(
                        blob[*pos - 8..*pos].try_into().map_err(|_| slice_err())?,
                    );
                    Ok((Value::BigInt(v), 8))
                }
            }
        }
        ColumnType::Decimal => {
            if *pos + 17 > blob.len() {
                return Err(DbError::ParseError {
                    message: "truncated: expected 17 bytes for Decimal element".to_string(),
                    position: None,
                });
            }
            let m = i128::from_le_bytes(blob[*pos..*pos + 16].try_into().map_err(|_| slice_err())?);
            let s = blob[*pos + 16];
            *pos += 17;
            Ok((Value::Decimal(m, s), 17))
        }
        ColumnType::Uuid => {
            if *pos + 16 > blob.len() {
                return Err(DbError::ParseError {
                    message: "truncated: expected 16 bytes for Uuid element".to_string(),
                    position: None,
                });
            }
            let u: [u8; 16] = blob[*pos..*pos + 16].try_into().map_err(|_| slice_err())?;
            *pos += 16;
            Ok((Value::Uuid(u), 16))
        }
        ColumnType::Text | ColumnType::Json => {
            if *pos + 3 > blob.len() {
                return Err(DbError::ParseError {
                    message: "truncated: expected u24 length for Text/Json element".to_string(),
                    position: None,
                });
            }
            let len = blob[*pos] as usize
                | (blob[*pos + 1] as usize) << 8
                | (blob[*pos + 2] as usize) << 16;
            *pos += 3;
            if *pos + len > blob.len() {
                return Err(DbError::ParseError {
                    message: format!("truncated: expected {} bytes for Text/Json element", len),
                    position: None,
                });
            }
            let s = std::str::from_utf8(&blob[*pos..*pos + len])
                .map_err(|_| DbError::ParseError {
                    message: "invalid UTF-8 in Text/Json array element".to_string(),
                    position: None,
                })?
                .to_string();
            *pos += len;
            let value = if elem_type == ColumnType::Json {
                Value::Json(s)
            } else {
                Value::Text(s)
            };
            Ok((value, 3 + len))
        }
        ColumnType::Bytes => {
            if *pos + 3 > blob.len() {
                return Err(DbError::ParseError {
                    message: "truncated: expected u24 length for Bytes element".to_string(),
                    position: None,
                });
            }
            let len = blob[*pos] as usize
                | (blob[*pos + 1] as usize) << 8
                | (blob[*pos + 2] as usize) << 16;
            *pos += 3;
            if *pos + len > blob.len() {
                return Err(DbError::ParseError {
                    message: format!("truncated: expected {} bytes for Bytes element", len),
                    position: None,
                });
            }
            let b = blob[*pos..*pos + len].to_vec();
            *pos += len;
            Ok((Value::Bytes(b), 3 + len))
        }
        ColumnType::Jsonb => {
            if *pos + 3 > blob.len() {
                return Err(DbError::ParseError {
                    message: "truncated: expected u24 length for Jsonb element".to_string(),
                    position: None,
                });
            }
            let len = blob[*pos] as usize
                | (blob[*pos + 1] as usize) << 8
                | (blob[*pos + 2] as usize) << 16;
            *pos += 3;
            if *pos + len > blob.len() {
                return Err(DbError::ParseError {
                    message: format!("truncated: expected {} bytes for Jsonb element", len),
                    position: None,
                });
            }
            let b = blob[*pos..*pos + len].to_vec();
            *pos += len;
            Ok((Value::Jsonb(std::sync::Arc::new(b)), 3 + len))
        }
        ColumnType::Array => Err(DbError::ParseError {
            message: "Array element type not supported as leaf element".to_string(),
            position: None,
        }),
        ColumnType::Range => Err(DbError::ParseError {
            message: "Range element type not supported as array element".to_string(),
            position: None,
        }),
        ColumnType::Money => Err(DbError::ParseError {
            message: "Money element type not supported as array element".to_string(),
            position: None,
        }),
        ColumnType::Composite => Err(DbError::ParseError {
            message: "Composite element type not supported as array element".to_string(),
            position: None,
        }),
        ColumnType::Ltree => Err(DbError::ParseError {
            message: "Ltree element type not supported as array element".to_string(),
            position: None,
        }),
        ColumnType::Xml => Err(DbError::ParseError {
            message: "Xml element type not supported as array element".to_string(),
            position: None,
        }),
    }
}

// ── Null bitmap helpers ───────────────────────────────────────────────────────

/// Builds a null bitmap for `nitems` elements given the null flags.
/// Returns the bitmap bytes and the count of null elements.
fn build_null_bitmap(values: &[Value], nitems: usize) -> (Vec<u8>, usize) {
    let bitmap_len = nitems.div_ceil(8);
    let mut bitmap = vec![0u8; bitmap_len];
    let mut null_count = 0;
    for (i, v) in values.iter().enumerate() {
        if v.is_null() {
            bitmap[i / 8] |= 1 << (i % 8);
            null_count += 1;
        }
    }
    (bitmap, null_count)
}

/// Reads a null bitmap and returns a vector of bools (true = null).
fn read_null_bitmap(blob: &[u8], pos: &mut usize, nitems: usize) -> Result<Vec<bool>, DbError> {
    let bitmap_len = nitems.div_ceil(8);
    if *pos + bitmap_len > blob.len() {
        return Err(DbError::ParseError {
            message: format!(
                "truncated: expected {} bytes for null bitmap at offset {}",
                bitmap_len, *pos
            ),
            position: None,
        });
    }
    let mut nulls = Vec::with_capacity(nitems);
    for i in 0..nitems {
        let byte_idx = i / 8;
        let bit_idx = i % 8;
        nulls.push((blob[*pos + byte_idx] >> bit_idx) & 1 == 1);
    }
    *pos += bitmap_len;
    Ok(nulls)
}

// ── Dimension validation ──────────────────────────────────────────────────────

/// Computes the product of dimensions, checking for overflow and exceeding MAX.
fn compute_nitems(dims: &[i32]) -> Result<i64, DbError> {
    if dims.is_empty() {
        return Ok(0);
    }
    let mut product: i64 = 1;
    for &d in dims {
        if d < 0 {
            return Err(DbError::InvalidValue {
                reason: format!("array dimension cannot be negative: {}", d),
            });
        }
        product = product
            .checked_mul(d as i64)
            .ok_or_else(|| DbError::InvalidValue {
                reason: "array size exceeds maximum allowed".to_string(),
            })?;
        if product > MAX_ARRAY_ELEMENTS as i64 {
            return Err(DbError::InvalidValue {
                reason: "array size exceeds maximum allowed".to_string(),
            });
        }
    }
    Ok(product)
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Encodes a 1D `Value::Array` into a varlena blob.
///
/// Infers `dims=[len]` from the flat element vector.
///
/// The element type is required because `Value::Array` carries no type information.
pub fn encode_array(value: &Value, elem_type: ColumnType) -> Result<Vec<u8>, DbError> {
    match value {
        Value::Array(elems) => {
            let ndim = if elems.is_empty() { 0 } else { 1 };
            encode_array_nd(value, elem_type, ndim, &[elems.len() as i32])
        }
        _ => Err(DbError::TypeMismatch {
            expected: "Array".to_string(),
            got: value.variant_name().to_string(),
        }),
    }
}

/// Encodes a `Value::Array` into a varlena blob with full control over dimensions.
///
/// - `elem_type`: ColumnType discriminant of the array elements
/// - `ndim`: number of dimensions (0-6)
/// - `dims`: dimension lengths in row-major order (last subscript varies fastest)
pub fn encode_array_nd(
    value: &Value,
    elem_type: ColumnType,
    ndim: i32,
    dims: &[i32],
) -> Result<Vec<u8>, DbError> {
    let Value::Array(elems) = value else {
        return Err(DbError::TypeMismatch {
            expected: "Array".to_string(),
            got: value.variant_name().to_string(),
        });
    };

    // Validate ndim
    if !(0..=MAX_ARRAY_DIMS).contains(&ndim) {
        return Err(DbError::InvalidValue {
            reason: format!(
                "number of array dimensions ({}) exceeds the maximum allowed ({})",
                ndim, MAX_ARRAY_DIMS
            ),
        });
    }

    // Handle empty array (ndim=0)
    if ndim == 0 {
        let mut buf = Vec::with_capacity(13); // header only
        buf.extend_from_slice(&4i32.to_le_bytes()); // total_len placeholder
        buf.extend_from_slice(&0i32.to_le_bytes()); // ndim = 0
        buf.extend_from_slice(&0i32.to_le_bytes()); // dataoffset = 0 (no nulls)
        buf.push(elem_type as u8); // elemtype
                                   // No dims/lbounds for ndim=0
        let total_len = (buf.len() - 4) as u32; // exclude the total_len field itself
        buf[0..4].copy_from_slice(&total_len.to_le_bytes());
        return Ok(buf);
    }

    // Validate dims length matches ndim
    if dims.len() != ndim as usize {
        return Err(DbError::InvalidValue {
            reason: format!(
                "dims length ({}) does not match ndim ({})",
                dims.len(),
                ndim
            ),
        });
    }

    // Compute expected element count
    let nitems = compute_nitems(dims)? as usize;
    if elems.len() != nitems {
        return Err(DbError::InvalidValue {
            reason: format!(
                "array element count ({}) does not match dimensions {:?} (expected {})",
                elems.len(),
                dims,
                nitems
            ),
        });
    }

    // Build null bitmap
    let (null_bitmap, null_count) = build_null_bitmap(elems, nitems);
    let has_nulls = null_count > 0;

    // We need to build the element data first to know dataoffset
    // Header size: 4 (total_len) + 4 (ndim) + 4 (dataoffset) + 1 (elemtype)
    //   + ndim*4 (dims) + ndim*4 (lbound)
    let header_size = 13 + (ndim as usize) * 8;
    let null_bitmap_size = if has_nulls { nitems.div_ceil(8) } else { 0 };

    let mut elem_buf = Vec::new();
    for elem in elems.iter() {
        if !elem.is_null() {
            encode_element(elem, elem_type, &mut elem_buf)?;
        }
    }

    let elem_data_size = elem_buf.len();
    let dataoffset = if has_nulls {
        (header_size + null_bitmap_size) as i32
    } else {
        0
    };

    let total_len = (header_size + null_bitmap_size + elem_data_size) as u32;

    if total_len > MAX_ARRAY_INLINE as u32 {
        return Err(DbError::ValueTooLarge {
            len: total_len as usize,
            max: MAX_ARRAY_INLINE,
        });
    }

    // Assemble the blob
    let mut buf = Vec::with_capacity(total_len as usize);
    buf.extend_from_slice(&total_len.to_le_bytes()); // total_len
    buf.extend_from_slice(&ndim.to_le_bytes()); // ndim
    buf.extend_from_slice(&dataoffset.to_le_bytes()); // dataoffset
    buf.push(elem_type as u8); // elemtype
    for &d in dims {
        buf.extend_from_slice(&d.to_le_bytes()); // dims
    }
    for _ in 0..ndim {
        buf.extend_from_slice(&1i32.to_le_bytes()); // lbound = 1
    }
    if has_nulls {
        buf.extend_from_slice(&null_bitmap);
    }
    buf.extend_from_slice(&elem_buf);

    // Verify final size
    if buf.len() != total_len as usize {
        return Err(DbError::InvalidValue {
            reason: format!(
                "internal error: array encoding produced {} bytes, expected {}",
                buf.len(),
                total_len
            ),
        });
    }

    Ok(buf)
}

/// Decodes an array blob back to `(Value::Array, element ColumnType, ndim)`.
///
/// The blob is assumed to be a valid array encoding (no validation of total_len
/// against actual bytes — caller should trust the source).
pub fn decode_array(blob: &[u8]) -> Result<(Value, ColumnType, i32), DbError> {
    // Guard: minimum header size
    if blob.len() < 13 {
        return Err(DbError::ParseError {
            message: format!(
                "truncated: array blob too short ({} bytes), minimum 13 needed",
                blob.len()
            ),
            position: None,
        });
    }
    let slice_err = || DbError::ParseError {
        message: "internal: fixed-size slice conversion (bounds checked above)".into(),
        position: None,
    };

    let total_len = u32::from_le_bytes(blob[0..4].try_into().map_err(|_| slice_err())?) as usize;
    if blob.len() < total_len {
        return Err(DbError::ParseError {
            message: format!(
                "truncated: array blob claims {} bytes but only {} available",
                total_len,
                blob.len()
            ),
            position: None,
        });
    }

    let ndim = i32::from_le_bytes(blob[4..8].try_into().map_err(|_| slice_err())?);
    let dataoffset = i32::from_le_bytes(blob[8..12].try_into().map_err(|_| slice_err())?);
    let elem_type_val = blob[12];
    let elem_type = ColumnType::try_from(elem_type_val).map_err(|_| DbError::ParseError {
        message: format!(
            "unknown array element ColumnType discriminant: {}",
            elem_type_val
        ),
        position: None,
    })?;

    // Validate ndim
    if !(0..=MAX_ARRAY_DIMS).contains(&ndim) {
        return Err(DbError::InvalidValue {
            reason: format!(
                "number of array dimensions ({}) exceeds the maximum allowed ({})",
                ndim, MAX_ARRAY_DIMS
            ),
        });
    }

    // Empty array
    if ndim == 0 {
        return Ok((Value::Array(vec![]), elem_type, 0));
    }

    let ndim_usize = ndim as usize;
    let header_size = 13 + ndim_usize * 8; // 4+4+4+1 + dims+lbound

    if blob.len() < header_size {
        return Err(DbError::ParseError {
            message: format!(
                "truncated: array blob header claims {} dims but blob is too short",
                ndim
            ),
            position: None,
        });
    }

    // Read dims
    let mut dims = Vec::with_capacity(ndim_usize);
    for i in 0..ndim_usize {
        let d = i32::from_le_bytes(
            blob[13 + i * 4..13 + (i + 1) * 4]
                .try_into()
                .map_err(|_| slice_err())?,
        );
        dims.push(d);
    }

    // Read lbound (not currently used, but must be present)
    let _lbound_start = 13 + ndim_usize * 4;

    // Compute nitems
    let nitems_i64 = compute_nitems(&dims)?;
    let nitems = nitems_i64 as usize;

    // Read null bitmap if dataoffset != 0
    let nulls: Vec<bool> = if dataoffset != 0 {
        let mut pos = header_size;
        let bitmap_len = nitems.div_ceil(8);
        if blob.len() < header_size + bitmap_len {
            return Err(DbError::ParseError {
                message: "truncated: null bitmap extends beyond blob".to_string(),
                position: None,
            });
        }
        read_null_bitmap(blob, &mut pos, nitems)?
    } else {
        vec![false; nitems]
    };

    // Decode elements
    let elem_start = if dataoffset != 0 {
        dataoffset as usize
    } else {
        header_size
    };

    let mut pos = elem_start;
    let mut values = Vec::with_capacity(nitems);

    for i in 0..nitems {
        let is_null = nulls.get(i).copied().unwrap_or(false);
        if is_null {
            values.push(Value::Null);
        } else {
            let (v, _consumed) = decode_element(blob, &mut pos, elem_type)?;
            values.push(v);
        }
    }

    Ok((Value::Array(values), elem_type, ndim))
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DataType;

    #[test]
    fn encode_decode_1d_int_array_empty() {
        let value = Value::Array(vec![]);
        let elem_type = ColumnType::Int;
        let blob = encode_array(&value, elem_type).unwrap();
        let (decoded, decoded_elem_type, ndim) = decode_array(&blob).unwrap();
        assert_eq!(decoded_elem_type, elem_type);
        // Empty array has ndim=0 per PG spec
        assert_eq!(ndim, 0);
        if let Value::Array(elems) = decoded {
            assert!(elems.is_empty());
        } else {
            panic!("expected Array");
        }
    }

    #[test]
    fn encode_decode_1d_int_array_three_elems() {
        let value = Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        let elem_type = ColumnType::Int;
        let blob = encode_array(&value, elem_type).unwrap();
        let (decoded, decoded_elem_type, ndim) = decode_array(&blob).unwrap();
        assert_eq!(decoded_elem_type, elem_type);
        assert_eq!(ndim, 1);
        assert_eq!(decoded, value);
    }

    #[test]
    fn encode_decode_1d_text_array() {
        let value = Value::Array(vec![
            Value::Text("hello".into()),
            Value::Text("world".into()),
            Value::Text("foo".into()),
        ]);
        let elem_type = ColumnType::Text;
        let blob = encode_array(&value, elem_type).unwrap();
        let (decoded, decoded_elem_type, ndim) = decode_array(&blob).unwrap();
        assert_eq!(decoded_elem_type, elem_type);
        assert_eq!(ndim, 1);
        assert_eq!(decoded, value);
    }

    #[test]
    fn encode_decode_2d_int_array() {
        // 2x3 array: [[1,2,3],[4,5,6]]
        let value = Value::Array(vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
            Value::Int(4),
            Value::Int(5),
            Value::Int(6),
        ]);
        let elem_type = ColumnType::Int;
        let blob = encode_array_nd(&value, elem_type, 2, &[2, 3]).unwrap();
        let (decoded, decoded_elem_type, ndim) = decode_array(&blob).unwrap();
        assert_eq!(decoded_elem_type, elem_type);
        assert_eq!(ndim, 2);
        assert_eq!(decoded, value);
    }

    #[test]
    fn encode_decode_null_elements() {
        let value = Value::Array(vec![Value::Int(1), Value::Null, Value::Int(3)]);
        let elem_type = ColumnType::Int;
        let blob = encode_array(&value, elem_type).unwrap();
        let (decoded, decoded_elem_type, ndim) = decode_array(&blob).unwrap();
        assert_eq!(decoded_elem_type, elem_type);
        assert_eq!(ndim, 1);
        assert_eq!(decoded, value);
    }

    #[test]
    fn encode_decode_bool_array() {
        let value = Value::Array(vec![
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(true),
        ]);
        let elem_type = ColumnType::Bool;
        let blob = encode_array(&value, elem_type).unwrap();
        let (decoded, decoded_elem_type, ndim) = decode_array(&blob).unwrap();
        assert_eq!(decoded_elem_type, elem_type);
        assert_eq!(ndim, 1);
        assert_eq!(decoded, value);
    }

    #[test]
    fn encode_decode_bigint_array() {
        let value = Value::Array(vec![Value::BigInt(i64::MAX), Value::BigInt(i64::MIN)]);
        let elem_type = ColumnType::BigInt;
        let blob = encode_array(&value, elem_type).unwrap();
        let (decoded, decoded_elem_type, ndim) = decode_array(&blob).unwrap();
        assert_eq!(decoded_elem_type, elem_type);
        assert_eq!(decoded, value);
    }

    #[test]
    fn encode_decode_real_array() {
        let value = Value::Array(vec![Value::Real(1.5), Value::Real(f64::INFINITY)]);
        let elem_type = ColumnType::Float;
        let blob = encode_array(&value, elem_type).unwrap();
        let (decoded, decoded_elem_type, ndim) = decode_array(&blob).unwrap();
        assert_eq!(decoded_elem_type, elem_type);
        assert_eq!(decoded, value);
    }

    #[test]
    fn encode_decode_decimal_array() {
        let value = Value::Array(vec![Value::Decimal(123456, 2), Value::Decimal(789, 3)]);
        let elem_type = ColumnType::Decimal;
        let blob = encode_array(&value, elem_type).unwrap();
        let (decoded, decoded_elem_type, ndim) = decode_array(&blob).unwrap();
        assert_eq!(decoded_elem_type, elem_type);
        assert_eq!(decoded, value);
    }

    #[test]
    fn encode_decode_date_array() {
        let value = Value::Array(vec![Value::Date(0), Value::Date(-1), Value::Date(365)]);
        let elem_type = ColumnType::Date;
        let blob = encode_array(&value, elem_type).unwrap();
        let (decoded, decoded_elem_type, ndim) = decode_array(&blob).unwrap();
        assert_eq!(decoded_elem_type, elem_type);
        assert_eq!(decoded, value);
    }

    #[test]
    fn encode_decode_timestamp_array() {
        let value = Value::Array(vec![Value::Timestamp(0), Value::Timestamp(1_000_000)]);
        let elem_type = ColumnType::Timestamp;
        let blob = encode_array(&value, elem_type).unwrap();
        let (decoded, decoded_elem_type, ndim) = decode_array(&blob).unwrap();
        assert_eq!(decoded_elem_type, elem_type);
        assert_eq!(decoded, value);
    }

    #[test]
    fn encode_decode_uuid_array() {
        let u1 = [
            0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc,
            0xde, 0xf0,
        ];
        let u2 = [
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01,
        ];
        let value = Value::Array(vec![Value::Uuid(u1), Value::Uuid(u2)]);
        let elem_type = ColumnType::Uuid;
        let blob = encode_array(&value, elem_type).unwrap();
        let (decoded, decoded_elem_type, ndim) = decode_array(&blob).unwrap();
        assert_eq!(decoded_elem_type, elem_type);
        assert_eq!(decoded, value);
    }

    #[test]
    fn encode_decode_bytes_array() {
        let value = Value::Array(vec![
            Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]),
            Value::Bytes(vec![]),
            Value::Bytes(vec![0xFF]),
        ]);
        let elem_type = ColumnType::Bytes;
        let blob = encode_array(&value, elem_type).unwrap();
        let (decoded, decoded_elem_type, ndim) = decode_array(&blob).unwrap();
        assert_eq!(decoded_elem_type, elem_type);
        assert_eq!(decoded, value);
    }

    #[test]
    fn encode_decode_json_array() {
        let value = Value::Array(vec![
            Value::Json(r#"{"a":1}"#.into()),
            Value::Json(r#"[1,2,3]"#.into()),
        ]);
        let elem_type = ColumnType::Json;
        let blob = encode_array(&value, elem_type).unwrap();
        let (decoded, decoded_elem_type, ndim) = decode_array(&blob).unwrap();
        assert_eq!(decoded_elem_type, elem_type);
        assert_eq!(decoded, value);
    }

    #[test]
    fn encode_decode_all_null_int_array() {
        // Array with all nulls (no element data, only null bitmap)
        let value = Value::Array(vec![Value::Null, Value::Null, Value::Null]);
        let elem_type = ColumnType::Int;
        let blob = encode_array(&value, elem_type).unwrap();
        let (decoded, decoded_elem_type, ndim) = decode_array(&blob).unwrap();
        assert_eq!(decoded_elem_type, elem_type);
        assert_eq!(ndim, 1);
        assert_eq!(decoded, value);
    }

    #[test]
    fn encode_nd_mismatched_dims() {
        let value = Value::Array(vec![Value::Int(1), Value::Int(2)]);
        let elem_type = ColumnType::Int;
        // Declare dims [2, 2] which expects 4 elements, but we only have 2 — should fail
        let result = encode_array_nd(&value, elem_type, 2, &[2, 2]);
        assert!(result.is_err());
    }

    #[test]
    fn encode_nd_too_many_dims() {
        let value = Value::Array(vec![Value::Int(1); 10]);
        let elem_type = ColumnType::Int;
        // 7 dimensions exceeds MAX_ARRAY_DIMS=6
        let result = encode_array_nd(&value, elem_type, 7, &[1, 1, 1, 1, 1, 1, 1]);
        assert!(result.is_err());
    }

    #[test]
    fn decode_truncated_blob() {
        let blob = vec![0x10, 0x00, 0x00, 0x00]; // total_len=16 but no rest
        let result = decode_array(&blob);
        assert!(result.is_err());
    }

    #[test]
    fn decode_invalid_elemtype() {
        // Build a valid header but with elemtype=99
        let mut blob = vec![0u8; 100];
        let len = blob.len() as u32;
        blob[0..4].copy_from_slice(&len.to_le_bytes());
        blob[4..8].copy_from_slice(&1i32.to_le_bytes()); // ndim=1
        blob[8..12].copy_from_slice(&0i32.to_le_bytes()); // dataoffset=0
        blob[12] = 99; // invalid elemtype
        blob[13..17].copy_from_slice(&3i32.to_le_bytes()); // dims=[3]
        let result = decode_array(&blob);
        assert!(result.is_err());
    }

    #[test]
    fn data_type_to_column_type_mapping() {
        use crate::types::DataType;
        assert_eq!(data_type_to_column_type(&DataType::Bool), ColumnType::Bool);
        assert_eq!(data_type_to_column_type(&DataType::Int), ColumnType::Int);
        assert_eq!(
            data_type_to_column_type(&DataType::BigInt),
            ColumnType::BigInt
        );
        assert_eq!(data_type_to_column_type(&DataType::Real), ColumnType::Float);
        assert_eq!(data_type_to_column_type(&DataType::Text), ColumnType::Text);
        assert_eq!(
            data_type_to_column_type(&DataType::Bytes),
            ColumnType::Bytes
        );
        assert_eq!(
            data_type_to_column_type(&DataType::Timestamp),
            ColumnType::Timestamp
        );
        assert_eq!(data_type_to_column_type(&DataType::Uuid), ColumnType::Uuid);
        assert_eq!(data_type_to_column_type(&DataType::Json), ColumnType::Json);
        assert_eq!(
            data_type_to_column_type(&DataType::Jsonb),
            ColumnType::Jsonb
        );
        assert_eq!(
            data_type_to_column_type(&DataType::Decimal),
            ColumnType::Decimal
        );
        assert_eq!(data_type_to_column_type(&DataType::Date), ColumnType::Date);
        assert_eq!(
            data_type_to_column_type(&DataType::Array(Box::new(DataType::Int))),
            ColumnType::Array
        );
    }
}
