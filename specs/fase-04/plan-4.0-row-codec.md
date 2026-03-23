# Plan: 4.0 — Row Codec

## Files to create / modify

| File | Action | Description |
|---|---|---|
| `crates/axiomdb-types/src/types.rs` | CREATE | `DataType` enum |
| `crates/axiomdb-types/src/value.rs` | CREATE | `Value` enum + `Display` |
| `crates/axiomdb-types/src/codec.rs` | CREATE | `encode_row`, `decode_row`, `encoded_len` |
| `crates/axiomdb-types/src/lib.rs` | MODIFY | expose all modules + re-exports |
| `crates/axiomdb-core/src/error.rs` | MODIFY | add `ValueTooLarge`, `InvalidValue` |
| `crates/axiomdb-types/tests/integration_row_codec.rs` | CREATE | integration tests |

**No Cargo.toml changes needed** — `axiomdb-types` already depends on
`axiomdb-core`. No new dependencies.

---

## Algorithm / Data structure

### Null bitmap helper (private, used by both encode and decode)

```rust
#[inline]
fn bitmap_len(n_cols: usize) -> usize {
    (n_cols + 7) / 8
}

#[inline]
fn is_null(bitmap: &[u8], col: usize) -> bool {
    (bitmap[col / 8] >> (col % 8)) & 1 == 1
}

#[inline]
fn set_null(bitmap: &mut [u8], col: usize) {
    bitmap[col / 8] |= 1 << (col % 8);
}
```

Using a single function `is_null` / `set_null` ensures encode and decode
use exactly the same bit convention — preventing any mismatch bug.

### encode_row

```
fn encode_row(values, schema) -> Result<Vec<u8>>:
    if values.len() != schema.len():
        return Err(TypeMismatch { expected: schema.len(), got: values.len() })

    n = values.len()
    blen = bitmap_len(n)

    // Phase 1: allocate with capacity hint
    cap = encoded_len(values) + /* safety margin for validation */ 0
    buf = Vec::with_capacity(encoded_len(values))
    buf.resize(blen, 0u8)  // reserve bitmap bytes, zeroed

    // Phase 2: set null bits and validate
    for i, (v, dt) in zip(values, schema):
        if v == Null:
            set_null(&mut buf[0..blen], i)
        else:
            validate_type(v, dt)?   // Err(TypeMismatch) on mismatch

    // Phase 3: encode non-null values
    for i, (v, _dt) in zip(values, schema):
        if is_null(&buf[0..blen], i): continue
        match v:
            Bool(b)       → buf.push(if b { 1 } else { 0 })
            Int(n)        → buf.extend_from_slice(&n.to_le_bytes())
            BigInt(n)     → buf.extend_from_slice(&n.to_le_bytes())
            Real(f)       → if f.is_nan() { return Err(InvalidValue) }
                            buf.extend_from_slice(&f.to_le_bytes())
            Decimal(m, s) → buf.extend_from_slice(&m.to_le_bytes())
                            buf.push(s)
            Text(s)       → let b = s.as_bytes()
                            if b.len() > 0xFF_FFFF { return Err(ValueTooLarge) }
                            write_u24(&mut buf, b.len())
                            buf.extend_from_slice(b)
            Bytes(b)      → if b.len() > 0xFF_FFFF { return Err(ValueTooLarge) }
                            write_u24(&mut buf, b.len())
                            buf.extend_from_slice(b)
            Date(d)       → buf.extend_from_slice(&d.to_le_bytes())
            Timestamp(t)  → buf.extend_from_slice(&t.to_le_bytes())
            Uuid(u)       → buf.extend_from_slice(&u)
    return Ok(buf)
```

### decode_row

```
fn decode_row(bytes, schema) -> Result<Vec<Value>>:
    n = schema.len()
    blen = bitmap_len(n)
    if bytes.len() < blen:
        return Err(ParseError { message: "truncated: bitmap" })
    bitmap = &bytes[0..blen]
    pos = blen

    values = Vec::with_capacity(n)
    for i, dt in schema.enumerate():
        if is_null(bitmap, i):
            values.push(Value::Null)
            continue

        need = fixed_size(dt)  // 0 for variable-length
        if need > 0 and pos + need > bytes.len():
            return Err(ParseError { message: "truncated" })

        match dt:
            Bool →
                v = bytes[pos] != 0; pos += 1
                values.push(Bool(v))
            Int →
                v = i32::from_le_bytes(bytes[pos..pos+4]); pos += 4
                values.push(Int(v))
            BigInt →
                v = i64::from_le_bytes(bytes[pos..pos+8]); pos += 8
                values.push(BigInt(v))
            Real →
                v = f64::from_le_bytes(bytes[pos..pos+8]); pos += 8
                values.push(Real(v))
            Decimal →
                m = i128::from_le_bytes(bytes[pos..pos+16]); pos += 16
                s = bytes[pos]; pos += 1
                values.push(Decimal(m, s))
            Text →
                len = read_u24(bytes, pos)?; pos += 3
                if pos + len > bytes.len(): return Err(ParseError("truncated"))
                s = String::from_utf8(bytes[pos..pos+len].to_vec())
                    .map_err(|_| ParseError("invalid UTF-8"))?
                pos += len; values.push(Text(s))
            Bytes →
                len = read_u24(bytes, pos)?; pos += 3
                if pos + len > bytes.len(): return Err(ParseError("truncated"))
                values.push(Bytes(bytes[pos..pos+len].to_vec())); pos += len
            Date →
                v = i32::from_le_bytes(bytes[pos..pos+4]); pos += 4
                values.push(Date(v))
            Timestamp →
                v = i64::from_le_bytes(bytes[pos..pos+8]); pos += 8
                values.push(Timestamp(v))
            Uuid →
                u = bytes[pos..pos+16].try_into()
                    .map_err(|_| ParseError("truncated uuid"))?
                pos += 16; values.push(Uuid(u))
    return Ok(values)
```

### encoded_len (infallible, no schema)

```rust
pub fn encoded_len(values: &[Value]) -> usize {
    let blen = (values.len() + 7) / 8;
    let data: usize = values.iter().map(|v| match v {
        Value::Null       => 0,
        Value::Bool(_)    => 1,
        Value::Int(_) | Value::Date(_) => 4,
        Value::BigInt(_) | Value::Real(_) | Value::Timestamp(_) => 8,
        Value::Decimal(..)=> 17,
        Value::Uuid(_)    => 16,
        Value::Text(s)    => 3 + s.len(),
        Value::Bytes(b)   => 3 + b.len(),
    }).sum();
    blen + data
}
```

### validate_type (private)

```rust
fn validate_type(value: &Value, dt: DataType) -> Result<(), DbError> {
    match (value, dt) {
        (Value::Bool(_),      DataType::Bool)      => Ok(()),
        (Value::Int(_),       DataType::Int)        => Ok(()),
        (Value::BigInt(_),    DataType::BigInt)     => Ok(()),
        (Value::Real(_),      DataType::Real)       => Ok(()),
        (Value::Decimal(..), DataType::Decimal)    => Ok(()),
        (Value::Text(_),      DataType::Text)       => Ok(()),
        (Value::Bytes(_),     DataType::Bytes)      => Ok(()),
        (Value::Date(_),      DataType::Date)       => Ok(()),
        (Value::Timestamp(_), DataType::Timestamp)  => Ok(()),
        (Value::Uuid(_),      DataType::Uuid)       => Ok(()),
        _ => Err(DbError::TypeMismatch {
            expected: format!("{dt:?}"),
            got: format!("{}", value.variant_name()),
        }),
    }
}
// Value::Null is handled before validate_type is called — never reaches here.
```

### u24 helpers (private)

```rust
fn write_u24(buf: &mut Vec<u8>, n: usize) {
    buf.push((n & 0xFF) as u8);
    buf.push(((n >> 8) & 0xFF) as u8);
    buf.push(((n >> 16) & 0xFF) as u8);
}

fn read_u24(bytes: &[u8], pos: usize) -> Result<usize, DbError> {
    if pos + 3 > bytes.len() {
        return Err(DbError::ParseError { message: "truncated: u24 length".into() });
    }
    Ok(bytes[pos] as usize
        | (bytes[pos + 1] as usize) << 8
        | (bytes[pos + 2] as usize) << 16)
}
```

---

## Implementation phases

### Phase 1 — DbError variants (axiomdb-core)
1. Add to `error.rs`:
   ```rust
   #[error("value too large: {len} bytes (maximum {max})")]
   ValueTooLarge { len: usize, max: usize },
   #[error("invalid value: {reason}")]
   InvalidValue { reason: String },
   ```

### Phase 2 — DataType (axiomdb-types/src/types.rs)
1. Define `DataType` with 10 variants.
2. Derive `Debug, Clone, Copy, PartialEq, Eq`.

### Phase 3 — Value (axiomdb-types/src/value.rs)
1. Define `Value` with 11 variants.
2. Derive `Clone, Debug, PartialEq`.
3. Implement `Display`:
   - Null → `"NULL"`
   - Bool → `"true"` / `"false"`
   - Int/BigInt → `n.to_string()`
   - Real → `f.to_string()`
   - Decimal(m, s) → `format!("{m}e-{s}")`
   - Text(s) → `s` (the string itself)
   - Bytes(b) → `format!("\\x{}", hex_encode(b))`
   - Date(d) → `format!("date:{d}")`
   - Timestamp(t) → `format!("ts:{t}")`
   - Uuid(u) → UUID standard format string
4. Add `fn variant_name(&self) -> &'static str` for error messages.
5. Inline unit tests for `Display`.

### Phase 4 — Codec (axiomdb-types/src/codec.rs)
1. Implement private helpers: `bitmap_len`, `is_null`, `set_null`,
   `write_u24`, `read_u24`, `validate_type`.
2. Implement `encoded_len` (infallible, no schema).
3. Implement `encode_row`.
4. Implement `decode_row`.
5. Inline unit tests for bitmap helpers and u24 roundtrip.

### Phase 5 — lib.rs
```rust
pub mod codec;
pub mod types;
pub mod value;

pub use codec::{decode_row, encode_row, encoded_len};
pub use types::DataType;
pub use value::Value;
```

### Phase 6 — Integration tests
File: `crates/axiomdb-types/tests/integration_row_codec.rs`

24 tests:
```
test_roundtrip_bool_true / false
test_roundtrip_int (positive + negative)
test_roundtrip_bigint
test_roundtrip_real (positive, negative, infinity)
test_roundtrip_decimal
test_roundtrip_text
test_roundtrip_text_empty
test_roundtrip_bytes
test_roundtrip_bytes_empty
test_roundtrip_date (positive + negative)
test_roundtrip_timestamp
test_roundtrip_uuid
test_roundtrip_null_only
test_roundtrip_all_nulls_10_cols
test_roundtrip_alternating_nulls
test_roundtrip_empty_row (0 columns)
test_roundtrip_9_cols_two_bitmap_bytes
test_encoded_len_equals_actual_encoded_size
test_error_nan_real
test_error_type_mismatch_value_schema
test_error_length_mismatch_values_vs_schema
test_error_truncated_mid_value
test_error_truncated_bitmap
test_error_invalid_utf8
test_null_bitmap_bit_positions_explicit
```

---

## Anti-patterns to avoid

- **DO NOT** call `unwrap()` anywhere in `src/` — use `?` for all fallible operations.
- **DO NOT** use slice indexing without length check before `from_le_bytes` — always verify `pos + size <= bytes.len()`.
- **DO NOT** use `String::from_utf8(...).unwrap()` — map the error to `ParseError`.
- **DO NOT** mix up bit ordering in the null bitmap — use `is_null` / `set_null` helpers consistently in both encode and decode.
- **DO NOT** add `axiomdb-catalog` to `axiomdb-types/Cargo.toml` — use `DataType` (local) not `ColumnType` (catalog).
- **DO NOT** implement `encoded_len` with `Result` — it must be infallible.

---

## Risks

| Risk | Mitigation |
|---|---|
| Bitmap bit ordering differs between encode/decode | Single `is_null`/`set_null` helper used by both |
| `f64::to_le_bytes()` silently encodes NaN | Explicit `is_nan()` check before encoding |
| `i128::from_le_bytes` on slice without length check | Explicit `pos + 16 <= bytes.len()` guard |
| `try_into::<[u8;16]>()` on Uuid slice | `pos + 16 <= bytes.len()` guard before; `try_into` maps err to ParseError |
| Text with valid u24 len but truncated bytes | Two-step check: read len from u24, then check `pos + len <= bytes.len()` |
| `TypeMismatch` error message is opaque | `variant_name()` on `Value` gives readable name (`"Int"`, `"Text"`, etc.) |
