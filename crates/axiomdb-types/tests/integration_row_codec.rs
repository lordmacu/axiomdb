//! Integration tests for the row codec (subfase 4.0).
//!
//! Covers: roundtrip for all 10 non-Null types, NULL handling, bitmap bit
//! positions, empty rows, 9-column rows (2-byte bitmap), error cases.

use axiomdb_core::DbError;
use axiomdb_types::{decode_row, encode_row, encoded_len, DataType, Value};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn roundtrip(values: &[Value], schema: &[DataType]) -> Vec<Value> {
    let encoded = encode_row(values, schema).expect("encode failed");
    assert_eq!(encoded.len(), encoded_len(values), "encoded_len mismatch");
    decode_row(&encoded, schema).expect("decode failed")
}

// ── Single-type roundtrips ────────────────────────────────────────────────────

#[test]
fn test_roundtrip_bool_true() {
    let v = vec![Value::Bool(true)];
    let s = &[DataType::Bool];
    assert_eq!(roundtrip(&v, s), v);
}

#[test]
fn test_roundtrip_bool_false() {
    let v = vec![Value::Bool(false)];
    let s = &[DataType::Bool];
    assert_eq!(roundtrip(&v, s), v);
}

#[test]
fn test_roundtrip_int_positive_negative() {
    for n in [0i32, 1, -1, i32::MAX, i32::MIN] {
        let v = vec![Value::Int(n)];
        assert_eq!(roundtrip(&v, &[DataType::Int]), v, "failed for Int({n})");
    }
}

#[test]
fn test_roundtrip_bigint() {
    for n in [0i64, i64::MAX, i64::MIN, -999_999_999_999] {
        let v = vec![Value::BigInt(n)];
        assert_eq!(roundtrip(&v, &[DataType::BigInt]), v);
    }
}

#[test]
fn test_roundtrip_real() {
    for f in [0.0f64, 1.0, -1.0, 3.14159, f64::INFINITY, f64::NEG_INFINITY] {
        let v = vec![Value::Real(f)];
        assert_eq!(roundtrip(&v, &[DataType::Real]), v, "failed for Real({f})");
    }
}

#[test]
fn test_roundtrip_decimal() {
    let cases = [
        (123456i128, 2u8), // 1234.56
        (0, 0),
        (-999, 3), // -0.999
        (i128::MAX, 38),
        (i128::MIN, 0),
    ];
    for (m, s) in cases {
        let v = vec![Value::Decimal(m, s)];
        assert_eq!(roundtrip(&v, &[DataType::Decimal]), v);
    }
}

#[test]
fn test_roundtrip_text() {
    let v = vec![Value::Text("hello, world".into())];
    assert_eq!(roundtrip(&v, &[DataType::Text]), v);
}

#[test]
fn test_roundtrip_text_empty() {
    let v = vec![Value::Text(String::new())];
    assert_eq!(roundtrip(&v, &[DataType::Text]), v);
}

#[test]
fn test_roundtrip_text_unicode() {
    let v = vec![Value::Text("こんにちは 🦀".into())];
    assert_eq!(roundtrip(&v, &[DataType::Text]), v);
}

#[test]
fn test_roundtrip_bytes() {
    let v = vec![Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF])];
    assert_eq!(roundtrip(&v, &[DataType::Bytes]), v);
}

#[test]
fn test_roundtrip_bytes_empty() {
    let v = vec![Value::Bytes(vec![])];
    assert_eq!(roundtrip(&v, &[DataType::Bytes]), v);
}

#[test]
fn test_roundtrip_date_positive_and_negative() {
    for d in [0i32, 1, -1, 19722, -365] {
        let v = vec![Value::Date(d)];
        assert_eq!(roundtrip(&v, &[DataType::Date]), v);
    }
}

#[test]
fn test_roundtrip_timestamp() {
    for t in [0i64, 1_704_067_200_000_000, -86_400_000_000] {
        let v = vec![Value::Timestamp(t)];
        assert_eq!(roundtrip(&v, &[DataType::Timestamp]), v);
    }
}

#[test]
fn test_roundtrip_uuid() {
    let u = [
        0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde,
        0xf0,
    ];
    let v = vec![Value::Uuid(u)];
    assert_eq!(roundtrip(&v, &[DataType::Uuid]), v);
}

// ── NULL handling ─────────────────────────────────────────────────────────────

#[test]
fn test_roundtrip_single_null() {
    let v = vec![Value::Null];
    let s = &[DataType::Int];
    assert_eq!(roundtrip(&v, s), v);
}

#[test]
fn test_roundtrip_all_nulls_5_cols() {
    let v = vec![Value::Null; 5];
    let s = vec![DataType::Int; 5];
    let encoded = encode_row(&v, &s).unwrap();
    // All nulls → only bitmap (1 byte for 5 cols) + no value bytes.
    assert_eq!(encoded.len(), 1);
    assert_eq!(encoded[0], 0b0001_1111); // bits 0–4 set
    let decoded = decode_row(&encoded, &s).unwrap();
    assert_eq!(decoded, v);
}

#[test]
fn test_roundtrip_alternating_nulls() {
    // 6 columns alternating NULL / non-NULL
    let v = vec![
        Value::Int(1),
        Value::Null,
        Value::Int(3),
        Value::Null,
        Value::Int(5),
        Value::Null,
    ];
    let s = vec![DataType::Int; 6];
    let decoded = roundtrip(&v, &s);
    assert_eq!(decoded, v);
}

// ── Empty row ─────────────────────────────────────────────────────────────────

#[test]
fn test_roundtrip_empty_row() {
    let v: Vec<Value> = vec![];
    let s: &[DataType] = &[];
    let encoded = encode_row(&v, s).unwrap();
    assert_eq!(encoded.len(), 0);
    let decoded = decode_row(&encoded, s).unwrap();
    assert_eq!(decoded, v);
}

// ── 9-column row (2-byte bitmap) ─────────────────────────────────────────────

#[test]
fn test_roundtrip_9_cols_two_bitmap_bytes() {
    let v: Vec<Value> = (0..9).map(|i| Value::Int(i as i32)).collect();
    let s: Vec<DataType> = vec![DataType::Int; 9];
    let encoded = encode_row(&v, &s).unwrap();
    // 2-byte bitmap (all zeros, no NULLs) + 9 × 4 bytes = 2 + 36 = 38
    assert_eq!(encoded[0], 0); // no nulls in byte 0
    assert_eq!(encoded[1], 0); // no nulls in byte 1
    assert_eq!(encoded.len(), 2 + 36);
    let decoded = decode_row(&encoded, &s).unwrap();
    assert_eq!(decoded, v);
}

#[test]
fn test_null_bitmap_bit_positions_9_cols() {
    // col 0 and col 8 are NULL
    let mut v: Vec<Value> = (0..9).map(|i| Value::Int(i as i32)).collect();
    v[0] = Value::Null;
    v[8] = Value::Null;
    let s = vec![DataType::Int; 9];
    let encoded = encode_row(&v, &s).unwrap();
    // byte 0: bit 0 set (col 0 NULL)
    assert_eq!(encoded[0] & 1, 1, "bit 0 of byte 0 must be set for col 0");
    // byte 1: bit 0 set (col 8 NULL, since 8 % 8 = 0)
    assert_eq!(encoded[1] & 1, 1, "bit 0 of byte 1 must be set for col 8");
    let decoded = decode_row(&encoded, &s).unwrap();
    assert_eq!(decoded, v);
}

// ── encoded_len consistency ───────────────────────────────────────────────────

#[test]
fn test_encoded_len_equals_actual_for_all_types() {
    let values = vec![
        Value::Bool(true),
        Value::Int(-7),
        Value::BigInt(i64::MIN),
        Value::Real(2.718),
        Value::Decimal(999, 3),
        Value::Text("axiomdb".into()),
        Value::Bytes(vec![1, 2, 3]),
        Value::Date(100),
        Value::Timestamp(42),
        Value::Uuid([0u8; 16]),
    ];
    let schema = vec![
        DataType::Bool,
        DataType::Int,
        DataType::BigInt,
        DataType::Real,
        DataType::Decimal,
        DataType::Text,
        DataType::Bytes,
        DataType::Date,
        DataType::Timestamp,
        DataType::Uuid,
    ];
    let predicted = encoded_len(&values);
    let actual = encode_row(&values, &schema).unwrap().len();
    assert_eq!(predicted, actual);
}

#[test]
fn test_encoded_len_with_nulls() {
    let values = vec![Value::Null, Value::Int(1), Value::Null];
    let schema = &[DataType::Int, DataType::Int, DataType::Int];
    let predicted = encoded_len(&values);
    let actual = encode_row(&values, schema).unwrap().len();
    assert_eq!(predicted, actual);
}

// ── Error cases ───────────────────────────────────────────────────────────────

#[test]
fn test_error_nan_real() {
    let v = vec![Value::Real(f64::NAN)];
    let err = encode_row(&v, &[DataType::Real]).unwrap_err();
    assert!(
        matches!(err, DbError::InvalidValue { .. }),
        "expected InvalidValue, got: {err}"
    );
}

#[test]
fn test_error_positive_infinity_is_allowed() {
    let v = vec![Value::Real(f64::INFINITY)];
    assert!(encode_row(&v, &[DataType::Real]).is_ok());
}

#[test]
fn test_error_type_mismatch_value_schema() {
    // Text value for Int column
    let err = encode_row(&[Value::Text("x".into())], &[DataType::Int]).unwrap_err();
    assert!(
        matches!(err, DbError::TypeMismatch { .. }),
        "expected TypeMismatch, got: {err}"
    );
}

#[test]
fn test_error_length_mismatch_values_vs_schema() {
    let err = encode_row(&[Value::Int(1), Value::Int(2)], &[DataType::Int]).unwrap_err();
    assert!(
        matches!(err, DbError::TypeMismatch { .. }),
        "expected TypeMismatch for mismatched lengths, got: {err}"
    );
}

#[test]
fn test_error_truncated_bitmap() {
    // Encoded 3-column row has 1-byte bitmap; provide 0 bytes
    let err = decode_row(&[], &[DataType::Int, DataType::Int, DataType::Int]).unwrap_err();
    assert!(
        matches!(err, DbError::ParseError { .. }),
        "expected ParseError for truncated bitmap, got: {err}"
    );
}

#[test]
fn test_error_truncated_mid_value() {
    let values = vec![Value::Int(42)];
    let mut encoded = encode_row(&values, &[DataType::Int]).unwrap();
    // Remove last byte — Int is 4 bytes, so truncate to 1+3 = 4 (1 bitmap + 3 instead of 4)
    encoded.pop();
    let err = decode_row(&encoded, &[DataType::Int]).unwrap_err();
    assert!(
        matches!(err, DbError::ParseError { .. }),
        "expected ParseError for truncated value, got: {err}"
    );
}

#[test]
fn test_error_invalid_utf8_text() {
    // Manually build a row with invalid UTF-8 bytes for a Text column.
    // Format: 1 bitmap byte (0x00=no null) + 3-byte len + invalid bytes
    let mut bytes = vec![0x00u8]; // bitmap: col 0 not null
    bytes.extend_from_slice(&[3, 0, 0]); // u24 len = 3
    bytes.extend_from_slice(&[0xFF, 0xFE, 0xFD]); // invalid UTF-8
    let err = decode_row(&bytes, &[DataType::Text]).unwrap_err();
    assert!(
        matches!(err, DbError::ParseError { .. }),
        "expected ParseError for invalid UTF-8, got: {err}"
    );
}

// ── Multi-column mixed-type roundtrip ─────────────────────────────────────────

#[test]
fn test_roundtrip_full_row_all_types() {
    let values = vec![
        Value::Bool(false),
        Value::Int(-1),
        Value::BigInt(i64::MAX),
        Value::Real(-0.001),
        Value::Decimal(-123, 4),
        Value::Text("axiomdb row codec".into()),
        Value::Bytes(b"binary\x00data".to_vec()),
        Value::Date(-365),
        Value::Timestamp(-1_000_000),
        Value::Uuid([
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ]),
    ];
    let schema = vec![
        DataType::Bool,
        DataType::Int,
        DataType::BigInt,
        DataType::Real,
        DataType::Decimal,
        DataType::Text,
        DataType::Bytes,
        DataType::Date,
        DataType::Timestamp,
        DataType::Uuid,
    ];
    assert_eq!(roundtrip(&values, &schema), values);
}
