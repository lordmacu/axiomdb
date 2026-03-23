# Row Codec

The row codec converts between `&[Value]` (the in-memory representation used by
the executor) and `&[u8]` (the on-disk binary format stored in heap pages). The codec
is in `axiomdb-types::codec`.

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

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Packed Null Bitmap</span>
1 bit per column instead of 1 byte. For a table with 16 nullable columns, that is 15 bytes saved per row vs. a byte-per-column scheme. At 100M rows, that is 1.5 GB of disk savings — plus proportionally faster range scans (fewer bytes to read per row).
</div>
</div>

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

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — u24 Length Prefix</span>
Saving 1 byte per text/bytes column is significant at scale: a table with 10 text columns × 100M rows saves 1 GB of disk and proportionally faster I/O. The 16 MB per-value ceiling is intentional — values above ~16 KB will use TOAST (Phase 6) long before reaching it, making the u32 range unused in practice.
</div>
</div>

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
struct overhead and a dependency. The AxiomDB codec stores the i128 mantissa and
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

---

## Type Coercion (axiomdb-types::coerce)

The `axiomdb-types::coerce` module implements implicit type conversion. It is
separate from the codec: the codec only serializes well-typed `Value`s; coercion
happens before encoding, at expression evaluation and column assignment time.

### Two entry points

#### `coerce(value, target: DataType, mode: CoercionMode) -> Result<Value, DbError>`

Used by the executor on INSERT and UPDATE to convert a supplied value to the
declared column type. Examples:

- `coerce(Text("42"), DataType::Int, Strict)` → `Ok(Int(42))`
- `coerce(Int(7), DataType::BigInt, Strict)` → `Ok(BigInt(7))`
- `coerce(Date(1), DataType::Timestamp, Strict)` → `Ok(Timestamp(86_400_000_000))`
- `coerce(Null, DataType::Int, Strict)` → `Ok(Null)` — NULL always passes through

#### `coerce_for_op(l, r) -> Result<(Value, Value), DbError>`

Used by the expression evaluator in `eval_binary` to promote two operands to a
common type before arithmetic or comparison. Does **not** accept a
`CoercionMode` — operator widening is always deterministic and does not attempt
Text→numeric parsing.

- `coerce_for_op(Int(5), Real(1.5))` → `(Real(5.0), Real(1.5))`
- `coerce_for_op(Int(2), Decimal(314, 2))` → `(Decimal(200, 2), Decimal(314, 2))`
  — Int is scaled by `10^scale` so it has the same unit as the Decimal mantissa

### CoercionMode

```rust
pub enum CoercionMode {
    Strict,      // AxiomDB default — '42abc'→INT = error
    Permissive,  // MySQL compat — '42abc'→INT = 42 (stops at first non-digit)
}
```

### Complete conversion matrix

The full set of implicit conversions supported by `coerce()`:

| From | To | Rule |
|---|---|---|
| Any | same type | Identity — returned unchanged |
| `NULL` | any | Returns `NULL` |
| `Int(n)` | `BigInt` | `BigInt(n as i64)` — lossless |
| `Int(n)` | `Real` | `Real(n as f64)` — may lose precision for large values |
| `Int(n)` | `Decimal` | `Decimal(n, 0)` — lossless |
| `BigInt(n)` | `Int` | Range check: error if `n ∉ [i32::MIN, i32::MAX]` |
| `BigInt(n)` | `Real` | `Real(n as f64)` |
| `BigInt(n)` | `Decimal` | `Decimal(n, 0)` |
| `Text(s)` | `Int` | Parse full string as integer (strict) or leading digits (permissive) |
| `Text(s)` | `BigInt` | Same as Int but target is i64 |
| `Text(s)` | `Real` | Parse as f64; NaN/Inf are always rejected |
| `Text(s)` | `Decimal` | Parse as `[-][int][.][frac]`; scale = fraction digit count |
| `Date(d)` | `Timestamp` | `d * 86_400_000_000` µs — midnight UTC |
| `Bool(b)` | `Int/BigInt/Real` | Permissive mode only: `true→1`, `false→0` |
| everything else | | `DbError::InvalidCoercion` (SQLSTATE 22018) |

### Text → integer parsing rules in detail

**Strict mode** (AxiomDB default):
1. Strip leading/trailing ASCII whitespace.
2. Parse the entire remaining string as a decimal integer (optional leading `-`/`+`).
3. Any non-digit character after the optional sign → `InvalidCoercion`.
4. Overflow (value does not fit in target type) → `InvalidCoercion`.

**Permissive mode** (MySQL compat):
1. Strip whitespace.
2. Read optional sign.
3. Consume as many leading ASCII digit characters as possible.
4. If zero digits consumed → return `0` (e.g., `"abc"` → `0`).
5. Parse accumulated digits; overflow → `InvalidCoercion` (not silently clamped).

### Date → Timestamp conversion

`Date` stores days since 1970-01-01 as `i32`. `Timestamp` stores microseconds
since 1970-01-01 UTC as `i64`.

```
Timestamp = Date × 86_400_000_000
          = days × 86400 seconds/day × 1_000_000 µs/second
```

Day 0 = `1970-01-01T00:00:00Z` = Timestamp 0. Negative days produce negative
Timestamps (dates before the Unix epoch). The multiplication uses `checked_mul`
— overflow is impossible for any plausible calendar date but is handled
defensively.

### Int → Decimal scale adoption in coerce_for_op

When `coerce_for_op` promotes an `Int` or `BigInt` to match a `Decimal`, it uses
the **Decimal operand's existing scale** so that the result is expressed in the
same unit:

```
coerce_for_op(Int(5), Decimal(314, 2)):
  factor = 10^2 = 100
  Int(5) → Decimal(5 × 100, 2) = Decimal(500, 2)
  → (Decimal(500, 2), Decimal(314, 2))

eval_arithmetic(Add, Decimal(500, 2), Decimal(314, 2)):
  → Decimal(814, 2)  = 8.14  ✓
```

Without scale adoption, `5 + 3.14` would compute `Decimal(5 + 314, 2) = Decimal(319, 2) = 3.19` — wrong.
