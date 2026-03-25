# Plan: 5.5a — Binary Result Encoding by Type

## Files to create/modify

| File | Action | What |
|---|---|---|
| `crates/axiomdb-network/src/mysql/result.rs` | modify | Expand the result serializer to support a prepared-statement binary row path; keep column-definition building shared; align `Bool -> TINY` and `Decimal -> NEWDECIMAL` type codes |
| `crates/axiomdb-network/src/mysql/handler.rs` | modify | Switch only the `COM_STMT_EXECUTE` row-result path from `serialize_query_result()` to `serialize_query_result_binary()`; keep `COM_QUERY` on text protocol |
| `crates/axiomdb-network/src/mysql/mod.rs` | modify | Update module-level docs so `result.rs` is no longer documented as text-only |
| `crates/axiomdb-network/tests/integration_protocol.rs` | modify | Add packet-level tests for binary row encoding, null bitmap offset, fixed-width numerics, and type-code alignment |
| `tools/wire-test.py` | modify | Add a live prepared-statement smoke test using PyMySQL's low-level command path (`_execute_command` / `_read_packet`) and manual row-packet parsing |

## Algorithm / Data structure

### 1. `result.rs` — split result serialization by query protocol

Keep the existing text path unchanged:

```rust
pub fn serialize_query_result(result: QueryResult, seq_start: u8) -> PacketSeq
```

Add a new prepared-statement row path:

```rust
pub fn serialize_query_result_binary(result: QueryResult, seq_start: u8) -> PacketSeq
```

Behavior:

```rust
fn serialize_query_result_binary(result: QueryResult, seq_start: u8) -> PacketSeq {
    let status = SERVER_STATUS_AUTOCOMMIT;
    match result {
        QueryResult::Rows { columns, rows } => {
            serialize_rows_binary(&columns, &rows, seq_start, status)
        }
        QueryResult::Affected { count, last_insert_id } => {
            vec![(seq_start, build_ok_with_status(count, last_insert_id.unwrap_or(0), 0, status))]
        }
        QueryResult::Empty => {
            vec![(seq_start, build_ok_with_status(0, 0, 0, status))]
        }
    }
}
```

`serialize_rows_binary()` uses the same resultset framing AxiomDB already uses:

```rust
fn serialize_rows_binary(
    cols: &[ColumnMeta],
    rows: &[Row],
    seq_start: u8,
    final_status: u16,
) -> PacketSeq {
    // 1. column_count
    // 2. ColumnDefinition41 packets
    // 3. EOF after column defs
    // 4. binary row packets
    // 5. EOF after rows
}
```

### 2. Binary row packet layout

Each row packet payload is built exactly like this:

```rust
fn build_binary_row_packet(cols: &[ColumnMeta], row: &[Value]) -> Vec<u8> {
    debug_assert_eq!(cols.len(), row.len());

    let bitmap_len = (cols.len() + 7 + 2) / 8;
    let mut buf = Vec::with_capacity(1 + bitmap_len + cols.len() * 8);
    buf.push(0x00); // binary row header

    let bitmap_start = buf.len();
    buf.resize(bitmap_start + bitmap_len, 0);

    for (idx, (col, value)) in cols.iter().zip(row.iter()).enumerate() {
        if matches!(value, Value::Null) {
            set_binary_null_bit(&mut buf[bitmap_start..bitmap_start + bitmap_len], idx);
            continue;
        }
        encode_binary_cell(&mut buf, col.data_type, value);
    }

    buf
}
```

Null bitmap helper is fixed, not configurable:

```rust
fn set_binary_null_bit(bitmap: &mut [u8], field_index: usize) {
    let shifted = field_index + 2;
    let byte = shifted / 8;
    let bit = shifted % 8;
    bitmap[byte] |= 1 << bit;
}
```

### 3. Exact cell encoding rules

These rules are fixed in this plan and are not left for implementation-time choice:

```rust
fn encode_binary_cell(buf: &mut Vec<u8>, data_type: DataType, value: &Value) {
    match (data_type, value) {
        (DataType::Bool,      Value::Bool(v))      => buf.push(u8::from(*v)),
        (DataType::Int,       Value::Int(v))       => buf.extend_from_slice(&v.to_le_bytes()),
        (DataType::BigInt,    Value::BigInt(v))    => buf.extend_from_slice(&v.to_le_bytes()),
        (DataType::Real,      Value::Real(v))      => buf.extend_from_slice(&v.to_le_bytes()),
        (DataType::Decimal,   Value::Decimal(m,s)) => write_lenenc_str(buf, format_decimal(*m, *s).as_bytes()),
        (DataType::Text,      Value::Text(s))      => write_lenenc_str(buf, s.as_bytes()),
        (DataType::Bytes,     Value::Bytes(b))     => write_lenenc_str(buf, b),
        (DataType::Date,      Value::Date(days))   => encode_binary_date(buf, *days),
        (DataType::Timestamp, Value::Timestamp(ts)) => encode_binary_timestamp(buf, *ts),
        (DataType::Uuid,      Value::Uuid(u))      => write_lenenc_str(buf, format_uuid(u).as_bytes()),

        // Any mismatch is an internal invariant violation; we do not add a new
        // Result-returning serializer layer in 5.5a.
        (_, other) => unreachable!("QueryResult value/type mismatch: {other:?}"),
    }
}
```

Date/timestamp helpers:

```rust
fn encode_binary_date(buf: &mut Vec<u8>, days_since_epoch: i32) {
    let (year, month, day) = days_to_ymd(days_since_epoch as i64);
    buf.push(4);
    buf.extend_from_slice(&(year as u16).to_le_bytes());
    buf.push(month as u8);
    buf.push(day as u8);
}

fn encode_binary_timestamp(buf: &mut Vec<u8>, micros: i64) {
    let secs = micros.div_euclid(1_000_000);
    let micros_part = micros.rem_euclid(1_000_000) as u32;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = days_to_ymd(days);
    let hour = (rem / 3_600) as u8;
    let min  = ((rem % 3_600) / 60) as u8;
    let sec  = (rem % 60) as u8;

    if micros_part == 0 {
        buf.push(7);
    } else {
        buf.push(11);
    }
    buf.extend_from_slice(&(year as u16).to_le_bytes());
    buf.push(month as u8);
    buf.push(day as u8);
    buf.push(hour);
    buf.push(min);
    buf.push(sec);
    if micros_part != 0 {
        buf.extend_from_slice(&micros_part.to_le_bytes());
    }
}
```

### 4. Shared `ColumnDefinition41` type-code alignment

`build_column_def()` stays the single source of truth for column metadata. Its
type mapping is changed exactly like this:

```rust
fn datatype_to_mysql_type(dt: DataType) -> u8 {
    match dt {
        DataType::Bool      => 0x01, // TINY
        DataType::Int       => 0x03, // LONG
        DataType::BigInt    => 0x08, // LONGLONG
        DataType::Real      => 0x05, // DOUBLE
        DataType::Decimal   => 0xf6, // NEWDECIMAL
        DataType::Text      => 0xfd, // VAR_STRING
        DataType::Bytes     => 0xfc, // BLOB
        DataType::Date      => 0x0a, // DATE
        DataType::Timestamp => 0x07, // TIMESTAMP
        DataType::Uuid      => 0xfd, // VAR_STRING
    }
}
```

This mapping is shared by:
- `COM_QUERY`
- `COM_STMT_PREPARE` result-column metadata
- `COM_STMT_EXECUTE` result-column metadata

That decision is deliberate: AxiomDB already has one column-definition builder,
and leaving text and prepared paths with divergent type codes would create a
second metadata contract.

### 5. `handler.rs` — exact call-site switch

Only one runtime branch changes:

Current `COM_STMT_EXECUTE` success path:

```rust
let packets = serialize_query_result(qr, 1);
```

Replace it with:

```rust
let packets = serialize_query_result_binary(qr, 1);
```

Nothing else in `handler.rs` changes protocol path:
- `COM_QUERY` keeps `serialize_query_result_multi_warn(...)`
- `intercept_special_query()` keeps text result helpers
- `COM_STMT_PREPARE`, `COM_STMT_RESET`, and `COM_STMT_CLOSE` keep current logic

## Implementation phases

1. Extend `result.rs` with binary row serialization and the exact helper set above.
2. Update shared MySQL type-code mapping in `build_column_def()`.
3. Switch the `COM_STMT_EXECUTE` row-result call site in `handler.rs` to the binary serializer.
4. Add packet-level protocol tests in `integration_protocol.rs`.
5. Add a live low-level prepared-statement smoke test in `tools/wire-test.py`.

## Tests to write

- unit:
  - binary row header is `0x00`
  - null bitmap uses offset `2`
  - `BIGINT` payload is exact 8-byte LE
  - `DECIMAL` payload is lenenc exact ASCII string
  - `BLOB` payload preserves raw bytes including `0x00`
  - `DATE` payload is `[4][year u16 LE][month][day]`
  - `TIMESTAMP` payload is 7-byte when micros=0 and 11-byte when micros!=0
  - `Bool` advertises `TINY` and writes one byte
  - `Decimal` advertises `NEWDECIMAL`

- integration:
  - `serialize_query_result_binary()` emits:
    - column count
    - N column defs
    - EOF
    - one binary row packet per row
    - final EOF
  - text serializer regression: `serialize_query_result()` still encodes `NULL` as `0xfb` and text cells as lenenc strings

- wire:
  - use PyMySQL low-level internals, not the high-level cursor API:
    - `conn._execute_command(COMMAND.COM_STMT_PREPARE, sql_bytes)`
    - `conn._read_packet()` / `FieldDescriptorPacket` to parse server replies
    - `conn._execute_command(COMMAND.COM_STMT_EXECUTE, execute_payload)`
  - choose a zero-parameter prepared statement for the smoke test so `5.5a`
    validates result encoding without depending on parameter-binding correctness
  - query shape for the live smoke test:
    - create a table with typed columns
    - insert one row with `BIGINT`, `DECIMAL`, `DATE`
    - prepare `SELECT big_col, dec_col, day_col FROM ... WHERE id = 1`
    - execute with zero params
    - read the raw row packet and assert:
      - first byte `0x00`
      - `BIGINT` bytes are LE, not ASCII digits
      - `DECIMAL` is lenenc ASCII
      - `DATE` payload length byte is `4`

- bench:
  - none in this subphase
  - rationale: this is a correctness/compatibility change in wire encoding, not
    a planner/executor/storage performance change

## Anti-patterns to avoid

- Do not switch `COM_QUERY` to binary rows; binary encoding is only for prepared-statement result rows.
- Do not reuse `value_to_text()` for prepared rows; fixed-width numerics must not become ASCII digits.
- Do not run `String::from_utf8_lossy()` on `Value::Bytes`; that is exactly the corruption 5.5a is fixing.
- Do not keep `Bool` advertised as `BIT` while encoding it as one raw byte; metadata and payload must agree.
- Do not leave the wire smoke test at `Cursor.execute()` level; that path does not exercise raw prepared-statement row bytes here.
- Do not introduce a second independent column-definition builder for prepared statements.

## Risks

- Metadata/payload mismatch:
  - Risk: client decodes the row incorrectly even though packets are well-formed.
  - Mitigation: every protocol test asserts the column type byte and the row payload together.

- Calendar off-by-one in `DATE` / `TIMESTAMP`:
  - Risk: days-since-epoch conversion produces wrong Y/M/D.
  - Mitigation: reuse the existing Howard Hinnant date conversion already present in `result.rs`; add epoch and non-epoch tests.

- False confidence from high-level drivers:
  - Risk: a smoke test passes while the raw row packets are still text-like.
  - Mitigation: parse the raw `COM_STMT_EXECUTE` result packets directly in `tools/wire-test.py`.

- `COM_STMT_PREPARE` metadata for computed expressions remains coarse:
  - Risk: some clients may still observe `Text` metadata at prepare time for computed columns.
  - Mitigation: explicitly out of scope for 5.5a; this plan changes execute-time row encoding only and does not rewrite `extract_result_columns()`.
