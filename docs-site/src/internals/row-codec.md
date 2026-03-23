# Row Codec

The row codec converts between `&[Value]` (the in-memory representation used by
the executor) and `&[u8]` (the on-disk binary format stored in heap pages). The codec
is in `nexusdb-types::codec`.

---

## Binary Format

```text
┌──────────────────────────────────────────────────────────────────┐
│ null_bitmap: ceil(n_cols / 8) bytes                              │
│   bit i = (bitmap[i/8] >> (i%8)) & 1 == 1  →  column i is NULL  │
├──────────────────────────────────────────────────────────────────┤
│ For each non-NULL column, in column declaration order:           │
│   Bool             →  1 byte   (0x00 = false, 0x01 = true)      │
│   Int, Date        →  4 bytes  little-endian i32                │
│   BigInt, Real     →  8 bytes  little-endian i64 / f64          │
│   Timestamp        →  8 bytes  little-endian i64 (µs UTC)       │
│   Decimal          → 16 bytes  little-endian i128 mantissa       │
│                    +  1 byte   u8 scale                          │
│   Uuid             → 16 bytes  as-is (big-endian by convention)  │
│   Text, Bytes      →  3 bytes  u24 LE length prefix              │
│                    + length bytes  raw UTF-8 / raw bytes         │
└──────────────────────────────────────────────────────────────────┘
```

NULL columns are indicated only in the null bitmap. No bytes are written for
NULL values in the payload section. This means:

- A row with all columns NULL (and a null bitmap) encodes to `ceil(n_cols/8)` bytes.
- A row with no NULL columns encodes to `ceil(n_cols/8)` bytes (all zero bitmap) plus
  the sum of each column's fixed width or variable-length payload.

---

## Column Type Sizes

| Value variant | SQL type          | Encoded size            |
|---------------|-------------------|-------------------------|
| `Bool`        | BOOL, BOOLEAN     | 1 byte                  |
| `Int`         | INT, INTEGER      | 4 bytes                 |
| `BigInt`      | BIGINT            | 8 bytes                 |
| `Real`        | REAL, DOUBLE      | 8 bytes (f64, IEEE 754) |
| `Decimal(m,s)`| DECIMAL, NUMERIC  | 17 bytes (16 i128 + 1 scale) |
| `Uuid`        | UUID              | 16 bytes                |
| `Date`        | DATE              | 4 bytes (i32 days)      |
| `Timestamp`   | TIMESTAMP         | 8 bytes (i64 µs UTC)    |
| `Text`        | TEXT, VARCHAR, CHAR | 3 + len bytes          |
| `Bytes`       | BYTEA, BLOB       | 3 + len bytes           |

---

## Null Bitmap

The null bitmap occupies `ceil(n_cols / 8)` bytes at the start of every encoded row.
The bits are packed little-endian: bit 0 of byte 0 corresponds to column 0, bit 1 of
byte 0 to column 1, ..., bit 0 of byte 1 to column 8, and so on.

```
n_cols = 5  →  1 byte  (bits 5–7 are unused and always 0)
n_cols = 8  →  1 byte  (all 8 bits used)
n_cols = 9  →  2 bytes (bit 0 of byte 1 = column 8)
n_cols = 64 →  8 bytes
n_cols = 65 →  9 bytes
```

Reading column `i`:

```rust
let bit = (bitmap[i / 8] >> (i % 8)) & 1;
let is_null = bit == 1;
```

Setting column `i` as NULL:

```rust
bitmap[i / 8] |= 1 << (i % 8);
```

This design saves 7 bytes per nullable column compared to wrapping each value in
`Option<T>` (which adds a full word of overhead in Rust's memory layout).

---

## Why u24 for Variable-Length Fields

The length prefix for `Text` and `Bytes` is 3 bytes (a u24 in little-endian). This
covers strings up to 16,777,215 bytes (~16 MB). The codec enforces this limit with
`DbError::ValueTooLarge`.

**Why not u32 (4 bytes)?**

The codec has two independent size limits:
1. **Codec limit (u24):** Text/Bytes may not exceed 16,777,215 bytes per value.
2. **Storage limit (~16 KB):** An encoded row must fit within `MAX_TUPLE_DATA`, which
   is approximately `PAGE_BODY_SIZE - RowHeader_size - SlotEntry_size`.

In practice, a single row almost never approaches 16 MB (the codec limit). If it did,
it would far exceed the storage limit and be rejected by the heap layer anyway. Using
u24 saves 1 byte per string column — for a table with 10 text columns, every row is
10 bytes smaller. At 100 million rows, that is 1 GB of disk savings.

The u24 also signals that future TOAST (out-of-line storage for large values) will take
over before values approach 16 MB — TOAST is planned for Phase 6.

---

## Why i128 for DECIMAL

DECIMAL values are represented as `(mantissa: i128, scale: u8)`. The actual value is
`mantissa × 10^(-scale)`.

```
Decimal(123456789, 2)  →  1,234,567.89
Decimal(-199, 2)       →  -1.99
Decimal(0, 0)          →  0
```

`i128` provides 38 significant decimal digits, which matches `DECIMAL(38, s)` — the
maximum precision supported by most SQL databases including PostgreSQL and SQL Server.

The alternative, `rust_decimal::Decimal`, packs the same i128 internally but adds
struct overhead and a dependency. The NexusDB codec stores the i128 mantissa and
scale byte directly, with no intermediary struct.

---

## encoded_len — O(n) Without Allocation

`encoded_len(values, types)` computes the exact byte count that `encode_row` would
produce, without allocating a buffer.

```rust
pub fn encoded_len(values: &[Value], types: &[DataType]) -> usize {
    let bitmap_bytes = values.len().div_ceil(8);
    let payload: usize = values.iter().zip(types.iter())
        .filter(|(v, _)| !v.is_null())
        .map(|(v, dt)| fixed_size(dt) + variable_overhead(v))
        .sum();
    bitmap_bytes + payload
}
```

This is used by the heap insertion path to check whether the encoded row fits in the
remaining free space on the target page — without actually encoding it first.

---

## encode_row — Single Pass, No Intermediate Buffer

```rust
pub fn encode_row(values: &[Value], types: &[DataType]) -> Result<Vec<u8>, DbError>;
```

The encoder makes one pass over the columns:

1. Writes the null bitmap (all zero initially).
2. For each column, if the value is `Value::Null`, sets the corresponding bitmap bit.
   Otherwise, type-checks the value against the declared type and appends the encoded
   bytes.
3. Returns the complete `Vec<u8>`.

The type check step catches programmer errors early (e.g., passing `Value::Text` for a
column declared `DataType::Int`). It returns `DbError::TypeMismatch` rather than
writing corrupted bytes.

---

## decode_row — Position-Tracking Cursor

```rust
pub fn decode_row(bytes: &[u8], types: &[DataType]) -> Result<Vec<Value>, DbError>;
```

The decoder walks `bytes` with a position cursor:

1. Reads the null bitmap from the first `ceil(n_cols/8)` bytes.
2. For each column in order:
   - If the corresponding bitmap bit is 1 → push `Value::Null`.
   - Otherwise, read the fixed or variable-length bytes for the declared type,
     construct the `Value`, advance the cursor.
3. Returns `Err(DbError::ParseError)` if the buffer is shorter than expected
   (truncated row — indicates storage corruption).

---

## Example — Encoding a Users Row

```
Schema: users(id BIGINT, name TEXT, age INT, email TEXT, active BOOL)
Values: [BigInt(42), Text("Alice"), Int(30), Null, Bool(true)]

Step 1: null_bitmap = ceil(5/8) = 1 byte
        col 3 (email) is NULL → bit 3 of byte 0 → bitmap = 0b00001000 = 0x08

Step 2: encode non-NULL values:
        col 0 (BigInt(42))     → 8 bytes: 2A 00 00 00 00 00 00 00
        col 1 (Text("Alice"))  → 3 bytes length: 05 00 00
                               + 5 bytes payload: 41 6C 69 63 65
        col 2 (Int(30))        → 4 bytes: 1E 00 00 00
        col 3 (NULL)           → 0 bytes (indicated by bitmap)
        col 4 (Bool(true))     → 1 byte: 01

Final encoding (19 bytes total):
  [08] [2A 00 00 00 00 00 00 00] [05 00 00] [41 6C 69 63 65] [1E 00 00 00] [01]
   ^     bigint 42                  ^len=5    "Alice"            int 30       true
   bitmap: col 3 is NULL
```

`encoded_len` for this row would return 19 without allocating any buffer.

---

## NaN Constraint

`Value::Real(f64::NAN)` is a valid Rust value but is forbidden by the codec.
`encode_row` returns `DbError::InvalidValue` when it encounters `NaN`.

This is enforced because:
- SQL semantics require `NaN <> NaN` to be UNKNOWN, not FALSE.
- Storing NaN in the database would make equality comparisons unpredictable.
- IEEE 754 defines NaN as not-a-number — it is a sentinel, not a data value.

Code that constructs `Value::Real` must ensure the `f64` is not NaN before passing
it to the codec. The executor's arithmetic operations must propagate NaN as NULL.
