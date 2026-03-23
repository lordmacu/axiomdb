# Spec: 4.0 — Row Codec

## What to build (not how)

A `Value` enum and a `DataType` enum covering all SQL types supported in
Phase 4, plus a binary codec (`encode_row` / `decode_row`) that converts
between `&[Value]` (typed, in-memory representation used by the executor)
and `&[u8]` (compact binary representation stored in heap pages).

Both types live in `nexusdb-types`, which depends only on `nexusdb-core`.
The executor (`nexusdb-sql`) bridges between `DataType` (nexusdb-types) and
`ColumnType` (nexusdb-catalog) when reading column definitions from the catalog.

---

## DataType enum

Lives in `nexusdb-types/src/types.rs`. Represents a SQL column type as seen
by the executor and the codec. Does NOT carry parameters (precision, scale,
max-length) yet — those come in Phase 4.3 with DDL parsing.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    Bool,
    Int,        // i32
    BigInt,     // i64
    Real,       // f64 IEEE 754
    Decimal,    // i128 mantissa + u8 scale
    Text,       // UTF-8 string
    Bytes,      // raw bytes
    Date,       // i32 days since 1970-01-01
    Timestamp,  // i64 microseconds since 1970-01-01 00:00:00 UTC
    Uuid,       // [u8; 16]
}
```

`DataType` is the codec-facing type descriptor. `ColumnType` in
`nexusdb-catalog` remains the disk-storage discriminant (compact `repr(u8)`).
The executor converts `ColumnType → DataType` via a simple `match`.

---

## Value enum

Lives in `nexusdb-types/src/value.rs`, re-exported from the crate root.

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i32),
    BigInt(i64),
    Real(f64),           // NaN is forbidden in encode_row
    Decimal(i128, u8),   // mantissa × 10^(-scale); scale ∈ [0, 38]
    Text(String),        // UTF-8; encode_row rejects len > 16_777_215
    Bytes(Vec<u8>),      // raw bytes; same limit as Text
    Date(i32),           // days since 1970-01-01; negative = before epoch
    Timestamp(i64),      // microseconds since 1970-01-01 00:00:00 UTC
    Uuid([u8; 16]),      // 128-bit UUID, big-endian byte order
}
```

### PartialEq and NaN

`Value` derives `PartialEq`. For `Value::Real`, this follows IEEE 754:
`Value::Real(f64::NAN) != Value::Real(f64::NAN)`. This is correct and
expected — the encoder forbids NaN, so no `Value::Real(NaN)` ever enters
the system. Tests must not compare NaN values.

### Display

`Value` implements `Display` for error messages and the CLI. Format:

| Variant | Display |
|---|---|
| `Null` | `NULL` |
| `Bool(true)` | `true` |
| `Bool(false)` | `false` |
| `Int(n)` | decimal integer |
| `BigInt(n)` | decimal integer |
| `Real(f)` | Rust's default `f64` display |
| `Decimal(m, s)` | `"{m}e-{s}"` (e.g. `"123456e-2"`) |
| `Text(s)` | the string itself |
| `Bytes(b)` | `"\x{hex}"` (lowercase hex) |
| `Date(d)` | `"date:{d}"` (days, numeric) |
| `Timestamp(t)` | `"ts:{t}"` (microseconds, numeric) |
| `Uuid(u)` | `"{8hex}-{4hex}-{4hex}-{4hex}-{12hex}"` |

Date and Timestamp use numeric display now. ISO 8601 formatting comes with
Phase 4.19 built-in functions (`DATE_FORMAT`, `NOW()`), which will add
`chrono` to the workspace. Adding it now only for Display is premature.

---

## Binary row format

### Layout

```
┌──────────────────────────────────────────────────────────────┐
│ null_bitmap: ⌈n_cols / 8⌉ bytes                             │
│   Bit i = (bitmap[i/8] >> (i%8)) & 1 = 1 → column i NULL   │
│   NULL columns produce no value bytes                        │
├──────────────────────────────────────────────────────────────┤
│ For each non-NULL column in column order:                    │
│   Bool      → 1 byte:  0x00=false, 0x01=true                │
│   Int       → 4 bytes: i32 little-endian                    │
│   BigInt    → 8 bytes: i64 little-endian                    │
│   Real      → 8 bytes: f64 little-endian (IEEE 754)         │
│   Decimal   → 17 bytes: i128 LE (16B) + scale u8 (1B)      │
│   Text      → u24 LE (3B) len + UTF-8 bytes                 │
│   Bytes     → u24 LE (3B) len + raw bytes                   │
│   Date      → 4 bytes: i32 little-endian                    │
│   Timestamp → 8 bytes: i64 little-endian                    │
│   Uuid      → 16 bytes: as-is                               │
└──────────────────────────────────────────────────────────────┘
```

### Null bitmap

For `n` columns the bitmap occupies `⌈n / 8⌉` bytes.
Bit `i` is bit `i % 8` (counting from LSB = 0) of byte `i / 8`.

```
column index:  7 6 5 4 3 2 1 0    15 14 13 12 11 10 9 8
byte 0:        . . . . . . . .    byte 1: . .  .  .  . .  . .
```

Example — 10 columns, columns 0 and 9 are NULL:
```
byte 0 = 0b00000001  (bit 0 set = col 0 NULL)
byte 1 = 0b00000010  (bit 1 set = col 9 NULL, since 9 % 8 = 1)
```

### Text / Bytes length prefix (u24)

3 bytes little-endian allow lengths up to 16,777,215 bytes (~16 MB).
This provides room for future TOAST (Phase 11) without changing the format.

**Two independent size limits:**
1. **Codec limit** — `encode_row` rejects values where `len > 16_777_215`
   (u24 overflow). Returns `Err(ValueTooLarge { len, max: 16_777_215 })`.
2. **Storage limit** — `heap::insert_tuple` rejects a tuple where the
   total encoded row exceeds `MAX_TUPLE_DATA` (~16 KB). Returns
   `Err(HeapPageFull)`. The codec does not know or enforce `PAGE_SIZE`.

These are two separate layers. The codec handles the format limit; the heap
handles the page-size limit.

---

## API

Located in `nexusdb-types/src/codec.rs`.

```rust
/// Encodes `values` into a compact binary row.
///
/// `schema` and `values` must have the same length. A `Value::Null` is
/// valid for any `DataType`. A non-Null `Value` must match its `DataType`.
///
/// # Errors
/// - `DbError::TypeMismatch`  — lengths differ, or Value/DataType mismatch.
/// - `DbError::InvalidValue`  — `Value::Real` contains NaN.
/// - `DbError::ValueTooLarge` — Text or Bytes exceeds 16,777,215 bytes.
pub fn encode_row(values: &[Value], schema: &[DataType]) -> Result<Vec<u8>, DbError>

/// Decodes a binary row back into `Vec<Value>`.
///
/// `schema` must match the schema used when the row was encoded.
///
/// # Errors
/// - `DbError::ParseError` — bytes are truncated or structurally invalid.
/// - `DbError::ParseError` — a Text value contains invalid UTF-8.
pub fn decode_row(bytes: &[u8], schema: &[DataType]) -> Result<Vec<Value>, DbError>

/// Returns the encoded byte length for `values` without allocating.
///
/// Infallible and schema-free: each `Value` variant determines its own size.
/// Use before `insert_tuple` to check whether a row fits in the heap page.
pub fn encoded_len(values: &[Value]) -> usize
```

### Why `encoded_len` needs no schema and no `Result`

`Value` variants are self-typed: `Value::Int(42)` is always 4 bytes,
`Value::Text("hi")` is always `3 + 2` bytes. No schema is needed to compute
the size. No error can occur — the function reads only variant discriminants
and string/byte lengths, which are always valid.

---

## Inputs / Outputs

| Operation | Input | Output | Errors |
|---|---|---|---|
| `encode_row` | `&[Value]`, `&[DataType]` | `Vec<u8>` | TypeMismatch, InvalidValue, ValueTooLarge |
| `decode_row` | `&[u8]`, `&[DataType]` | `Vec<Value>` | ParseError |
| `encoded_len` | `&[Value]` | `usize` | — (infallible) |

---

## New DbError variants

```rust
#[error("value too large: {len} bytes (maximum {max})")]
ValueTooLarge { len: usize, max: usize },

#[error("invalid value: {reason}")]
InvalidValue { reason: String },
```

---

## Use cases

1. **Roundtrip — all non-NULL types**: encode a row with one value per type,
   decode it, get back identical values.

2. **Roundtrip — all NULLs**: every column is NULL; bitmap = all-ones; no
   value bytes; decode returns `Vec` of `Value::Null`.

3. **Roundtrip — alternating NULL/non-NULL**: verify bitmap bit positions
   are set for the right columns.

4. **Empty row (0 columns)**: valid; produces 0 bytes (no bitmap, no values).

5. **9-column row**: bitmap occupies 2 bytes; verify byte boundary crossing.

6. **Text roundtrip**: `"hello"` → `[5, 0, 0, h, e, l, l, o]` → `"hello"`.

7. **Text empty string**: `""` → `[0, 0, 0]` → `""`.

8. **NaN Real returns error**: `encode_row([Real(NaN)], [DataType::Real])`
   → `Err(InvalidValue)`.

9. **Truncated input returns error**: `decode_row` on bytes cut mid-value
   → `Err(ParseError)`.

10. **Type mismatch returns error**: `encode_row([Text("x")], [DataType::Int])`
    → `Err(TypeMismatch)`.

11. **Length mismatch returns error**: `values.len() != schema.len()`
    → `Err(TypeMismatch)`.

12. **encoded_len matches encode_row output**: for any valid row,
    `encoded_len(values) == encode_row(values, schema)?.len()`.

13. **Decimal roundtrip**: `Decimal(123456, 2)` (= 1234.56) → 17 bytes
    → same `Decimal(123456, 2)`.

14. **Date and Timestamp roundtrip**: negative values (before epoch) work.

15. **Uuid roundtrip**: 16 bytes preserved byte-for-byte.

---

## Acceptance criteria

- [ ] `DataType` enum has 10 variants (no Null — Null is a Value state, not a type)
- [ ] `Value` enum has 11 variants including Null
- [ ] `Value` implements `Display`, `Clone`, `Debug`, `PartialEq`
- [ ] `encode_row` builds correct null_bitmap (bit i set ↔ values[i] == Null)
- [ ] `encode_row` emits no bytes for NULL columns
- [ ] `encode_row` encodes each non-null type with correct width and endianness
- [ ] `encode_row` returns `Err(TypeMismatch)` when lengths differ
- [ ] `encode_row` returns `Err(TypeMismatch)` on Value/DataType mismatch
- [ ] `encode_row` returns `Err(InvalidValue)` for NaN in `Value::Real`
- [ ] `encode_row` returns `Err(ValueTooLarge)` for Text/Bytes > 16,777,215 bytes
- [ ] `decode_row` reconstructs the original `Vec<Value>` from encoded bytes
- [ ] `decode_row` sets `Value::Null` for bit-set columns in bitmap
- [ ] `decode_row` returns `Err(ParseError)` on truncated input
- [ ] `decode_row` returns `Err(ParseError)` on invalid UTF-8 in Text
- [ ] `encoded_len` is infallible and takes only `&[Value]` (no schema)
- [ ] `encoded_len(values) == encode_row(values, schema)?.len()` for all valid inputs
- [ ] All 10 non-Null type roundtrips pass (one test per type)
- [ ] 9-column row uses 2-byte bitmap correctly
- [ ] `ValueTooLarge` and `InvalidValue` added to `DbError` in nexusdb-core
- [ ] `nexusdb-types` depends only on `nexusdb-core` (no nexusdb-catalog)
- [ ] No `unwrap()` in `src/`

---

## ⚠️ DEFERRED

- ISO 8601 Display for Date/Timestamp → Phase 4.19 (adds `chrono`)
- `DataType` parameters: `Decimal(precision, scale)`, `Varchar(max_len)` → Phase 4.3
- TOAST for values > MAX_TUPLE_DATA → Phase 11
- Row compression → Phase 11
- `ColumnType → DataType` conversion utility → Phase 4.5 (executor)

---

## Out of scope

- `Value` arithmetic / comparison — Phase 4.17 (expression evaluator)
- Type coercion between Value variants — Phase 4.18b
- NOT NULL constraint enforcement — Phase 4.5 (executor)

---

## Dependencies

- `nexusdb-types`: new crate content (currently stub)
- `nexusdb-core`: `DbError` (adds `ValueTooLarge`, `InvalidValue`)
- **`nexusdb-types` depends ONLY on `nexusdb-core`** — no catalog, no storage
