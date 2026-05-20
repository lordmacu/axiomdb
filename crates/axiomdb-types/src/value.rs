//! SQL value — the in-memory representation of a single cell.
//!
//! [`Value`] is what the executor produces and consumes. The row codec
//! in [`codec`] converts between `&[Value]` and `&[u8]` for heap storage.
//!
//! [`codec`]: crate::codec

use std::fmt;

// ── Value ─────────────────────────────────────────────────────────────────────

/// A typed SQL value held in memory by the executor.
///
/// ## NaN constraint
///
/// `Value::Real(f64::NAN)` is a valid Rust value but is **forbidden** by
/// [`encode_row`]. The encoder rejects NaN with [`DbError::InvalidValue`].
/// Code that creates `Value::Real` values must ensure they are not NaN before
/// calling the codec.
///
/// `PartialEq` follows IEEE 754: `Value::Real(NAN) != Value::Real(NAN)`.
/// This is correct — NaN values never appear in encoded rows.
///
/// [`encode_row`]: crate::codec::encode_row
/// [`DbError::InvalidValue`]: axiomdb_core::error::DbError::InvalidValue
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// SQL NULL — absent value; valid for any column type.
    Null,
    /// SQL BOOLEAN.
    Bool(bool),
    /// SQL INT / INTEGER — 32-bit signed integer.
    Int(i32),
    /// SQL BIGINT — 64-bit signed integer.
    BigInt(i64),
    /// SQL REAL / DOUBLE PRECISION — 64-bit IEEE 754 float. NaN is forbidden.
    Real(f64),
    /// SQL DECIMAL / NUMERIC — `mantissa × 10^(-scale)`.
    ///
    /// - `scale` must be ≤ 38.
    /// - Example: `Decimal(123456, 2)` = 1234.56
    Decimal(i128, u8),
    /// SQL TEXT / VARCHAR — UTF-8 string. Max 16,777,215 bytes in the codec.
    Text(String),
    /// SQL BLOB / BYTEA — raw bytes. Same length limit as Text.
    Bytes(Vec<u8>),
    /// SQL DATE — days since 1970-01-01. Negative = before epoch.
    Date(i32),
    /// SQL TIMESTAMP — microseconds since 1970-01-01 00:00:00 UTC.
    Timestamp(i64),
    /// SQL TIMESTAMPTZ — microseconds since 1970-01-01 00:00:00 UTC, displayed with +00 suffix.
    TimestampTz(i64),
    /// SQL UUID — 128-bit identifier in big-endian byte order.
    Uuid([u8; 16]),
    /// SQL JSON — validated UTF-8 JSON text (Phase 11.4).
    /// Always NFC-normalized and syntax-validated on INSERT.
    Json(String),
    /// SQL JSONB — binary pre-parsed JSONB blob (Phase 11.16).
    /// Allows O(log k) key access without re-parsing UTF-8 text.
    Jsonb(std::sync::Arc<Vec<u8>>),
    /// SQL array value — ordered collection of values (Phase 20.4).
    /// All elements share the same array element type (enforced at schema level).
    Array(Vec<Value>),
    /// SQL range value (Phase 20.13). Boxed to keep Value size stable.
    Range(Box<crate::range_value::RangeValue>),
    /// SQL MONEY — exact decimal amount + ISO 4217 currency code (Phase 20.17).
    /// Stored as `mantissa × 10^(-scale)` plus a 3-byte ASCII currency code.
    /// Example: `Money(10050, 2, *b"USD")` = 100.50 USD.
    Money(i128, u8, [u8; 3]),
    /// SQL composite (user-defined) type value (Phase 20.18).
    /// Fields are in declaration order; a NULL field slot is `Value::Null`.
    /// Displays as `(v1,v2,…)` — parenthesized, comma-separated.
    Composite(Vec<Value>),
    /// SQL ltree — hierarchical label path (Phase 20.19).
    /// Stored as a validated `[A-Za-z0-9_]+` dot-separated path string.
    Ltree(String),
    /// SQL XML / XMLTYPE — UTF-8 XML text (Phase 20.20).
    /// Guaranteed well-formed at construction time (validated via roxmltree).
    Xml(String),
}

impl Value {
    /// Short name of the variant, used in error messages.
    pub fn variant_name(&self) -> &'static str {
        match self {
            Self::Null => "NULL",
            Self::Bool(_) => "Bool",
            Self::Int(_) => "Int",
            Self::BigInt(_) => "BigInt",
            Self::Real(_) => "Real",
            Self::Decimal(..) => "Decimal",
            Self::Text(_) => "Text",
            Self::Bytes(_) => "Bytes",
            Self::Date(_) => "Date",
            Self::Timestamp(_) => "Timestamp",
            Self::TimestampTz(_) => "TimestampTz",
            Self::Uuid(_) => "Uuid",
            Self::Json(_) => "Json",
            Self::Jsonb(_) => "Jsonb",
            Self::Array(_) => "Array",
            Self::Range(_) => "Range",
            Self::Money(..) => "Money",
            Self::Composite(_) => "Composite",
            Self::Ltree(_) => "Ltree",
            Self::Xml(_) => "Xml",
        }
    }

    /// Returns `true` if this value is `Value::Null`.
    #[inline]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}

// ── Display ───────────────────────────────────────────────────────────────────

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => write!(f, "NULL"),
            Self::Bool(b) => write!(f, "{b}"),
            Self::Int(n) => write!(f, "{n}"),
            Self::BigInt(n) => write!(f, "{n}"),
            Self::Real(v) => write!(f, "{v}"),
            // "123456e-2" is unambiguous and avoids a float division.
            Self::Decimal(m, s) => write!(f, "{m}e-{s}"),
            Self::Text(s) | Self::Json(s) => write!(f, "{s}"),
            Self::Jsonb(b) => {
                // Display as canonical JSON text
                match crate::jsonb::JsonbDecoder::to_string(b) {
                    Ok(s) => write!(f, "{s}"),
                    Err(_) => write!(f, "<invalid jsonb>"),
                }
            }
            Self::Bytes(b) => {
                write!(f, "\\x")?;
                for byte in b {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
            // Numeric display — ISO 8601 formatting comes with chrono in Phase 4.19.
            Self::Date(d) => write!(f, "date:{d}"),
            Self::Timestamp(t) => write!(f, "ts:{t}"),
            Self::TimestampTz(t) => write!(f, "tstz:{t}"),
            Self::Uuid(u) => write!(
                f,
                "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
                u32::from_be_bytes([u[0], u[1], u[2], u[3]]),
                u16::from_be_bytes([u[4], u[5]]),
                u16::from_be_bytes([u[6], u[7]]),
                u16::from_be_bytes([u[8], u[9]]),
                // last 6 bytes as u64 for the 12-hex segment
                {
                    let mut buf = [0u8; 8];
                    buf[2..].copy_from_slice(&u[10..16]);
                    u64::from_be_bytes(buf)
                }
            ),
            // PG-compatible array text format: {elem1,elem2,...}
            Self::Array(elems) => {
                write!(f, "{{")?;
                for (i, elem) in elems.iter().enumerate() {
                    if i > 0 {
                        write!(f, ",")?;
                    }
                    write!(f, "{elem}")?;
                }
                write!(f, "}}")
            }
            Self::Range(rv) => write!(f, "{}", rv.to_display_string()),
            Self::Composite(fields) => {
                write!(f, "(")?;
                for (i, v) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, ",")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, ")")
            }
            Self::Ltree(s) | Self::Xml(s) => write!(f, "{s}"),
            Self::Money(m, s, c) => {
                let currency = std::str::from_utf8(c)
                    .unwrap_or("???")
                    .trim_end_matches('\0');
                if *s == 0 {
                    write!(f, "{m} {currency}")
                } else {
                    let abs_m = m.unsigned_abs();
                    let divisor = 10u128.pow(*s as u32);
                    let int_part = abs_m / divisor;
                    let frac_part = abs_m % divisor;
                    let sign = if *m < 0 { "-" } else { "" };
                    write!(
                        f,
                        "{sign}{int_part}.{frac_part:0>width$} {currency}",
                        width = *s as usize
                    )
                }
            }
        }
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_real(s: &str) -> f64 {
        s.parse().expect("valid test f64")
    }

    #[test]
    fn test_display_null() {
        assert_eq!(Value::Null.to_string(), "NULL");
    }

    #[test]
    fn test_display_bool() {
        assert_eq!(Value::Bool(true).to_string(), "true");
        assert_eq!(Value::Bool(false).to_string(), "false");
    }

    #[test]
    fn test_display_int() {
        assert_eq!(Value::Int(-42).to_string(), "-42");
        assert_eq!(Value::Int(0).to_string(), "0");
    }

    #[test]
    fn test_display_bigint() {
        assert_eq!(Value::BigInt(i64::MAX).to_string(), i64::MAX.to_string());
    }

    #[test]
    fn test_display_real() {
        assert_eq!(Value::Real(parse_real("3.14")).to_string(), "3.14");
        assert_eq!(Value::Real(f64::INFINITY).to_string(), "inf");
    }

    #[test]
    fn test_display_decimal() {
        assert_eq!(Value::Decimal(123456, 2).to_string(), "123456e-2");
        assert_eq!(Value::Decimal(0, 0).to_string(), "0e-0");
    }

    #[test]
    fn test_display_text() {
        assert_eq!(Value::Text("hello".into()).to_string(), "hello");
        assert_eq!(Value::Text(String::new()).to_string(), "");
    }

    #[test]
    fn test_display_bytes() {
        assert_eq!(
            Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]).to_string(),
            "\\xdeadbeef"
        );
        assert_eq!(Value::Bytes(vec![]).to_string(), "\\x");
    }

    #[test]
    fn test_display_date_timestamp() {
        assert_eq!(Value::Date(0).to_string(), "date:0");
        assert_eq!(Value::Date(-1).to_string(), "date:-1");
        assert_eq!(Value::Timestamp(1_000_000).to_string(), "ts:1000000");
    }

    #[test]
    fn test_display_uuid() {
        let u = [
            0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc,
            0xde, 0xf0,
        ];
        assert_eq!(
            Value::Uuid(u).to_string(),
            "12345678-9abc-def0-1234-56789abcdef0"
        );
    }

    #[test]
    fn test_is_null() {
        assert!(Value::Null.is_null());
        assert!(!Value::Int(0).is_null());
    }

    #[test]
    fn test_variant_name() {
        assert_eq!(Value::Null.variant_name(), "NULL");
        assert_eq!(Value::Int(0).variant_name(), "Int");
        assert_eq!(Value::Text("".into()).variant_name(), "Text");
    }

    #[test]
    fn test_clone_and_partial_eq() {
        let v = Value::Text("hello".into());
        assert_eq!(v.clone(), v);
        assert_ne!(Value::Int(1), Value::Int(2));
        // NaN IEEE 754: NaN != NaN
        assert_ne!(Value::Real(f64::NAN), Value::Real(f64::NAN));
    }

    // ── Phase 20.4 Array tests ────────────────────────────────────────────────

    #[test]
    fn test_array_variant_name() {
        let arr = Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        assert_eq!(arr.variant_name(), "Array");
    }

    #[test]
    fn test_array_variant_basic() {
        let arr = Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        if let Value::Array(elems) = &arr {
            assert_eq!(elems.len(), 3);
            assert_eq!(elems[0], Value::Int(1));
            assert_eq!(elems[1], Value::Int(2));
            assert_eq!(elems[2], Value::Int(3));
        } else {
            panic!("expected Array");
        }
    }

    #[test]
    fn test_array_display() {
        let arr = Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        let s = format!("{}", arr);
        assert_eq!(s, "{1,2,3}");
    }

    #[test]
    fn test_array_display_empty() {
        let arr: Value = Value::Array(vec![]);
        assert_eq!(format!("{}", arr), "{}");
    }

    #[test]
    fn test_array_display_mixed() {
        let arr = Value::Array(vec![
            Value::Text("hello".into()),
            Value::Int(42),
            Value::Null,
        ]);
        let s = format!("{}", arr);
        assert_eq!(s, "{hello,42,NULL}");
    }

    #[test]
    fn test_array_equality() {
        let a = Value::Array(vec![Value::Int(1), Value::Int(2)]);
        let b = Value::Array(vec![Value::Int(1), Value::Int(2)]);
        let c = Value::Array(vec![Value::Int(1), Value::Int(3)]);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
