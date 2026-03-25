# Spec: 5.5a — Binary Result Encoding by Type

## These files were reviewed before writing this spec

### AxiomDB codebase
- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-network/src/mysql/handler.rs`
- `crates/axiomdb-network/src/mysql/result.rs`
- `crates/axiomdb-network/src/mysql/prepared.rs`
- `crates/axiomdb-network/src/mysql/session.rs`
- `crates/axiomdb-network/src/mysql/mod.rs`
- `crates/axiomdb-network/tests/integration_protocol.rs`
- `crates/axiomdb-sql/src/result.rs`
- `crates/axiomdb-types/src/value.rs`
- `tools/wire-test.py`

### Research consulted
- `research/mariadb-server/include/mysql_com.h`
- `research/mariadb-server/tests/mysql_client_test.c`
- `research/oceanbase/deps/oblib/src/rpc/obmysql/ob_mysql_row.cpp`
- `research/oceanbase/deps/oblib/src/rpc/obmysql/ob_mysql_util.cpp`

## What to build (not how)

`COM_STMT_EXECUTE` must stop returning text-encoded row packets and must return
MySQL binary resultset rows, typed according to the actual `QueryResult`
column metadata produced by AxiomDB.

This subphase changes only the prepared-statement result row format:
- `COM_STMT_EXECUTE` + `QueryResult::Rows` → binary row packets
- `COM_QUERY` → remains text protocol
- `COM_STMT_EXECUTE` + `QueryResult::Affected | Empty` → remains OK_Packet
- ERR packets and command sequencing remain unchanged

The resultset framing remains the same shape AxiomDB already uses:
1. column count packet
2. one `ColumnDefinition41` packet per column
3. EOF after column definitions
4. one row packet per row
5. EOF after rows

Only the row packet payload changes from text protocol to binary protocol.

Binary row packet rules:
- First byte is always `0x00`
- Nullability is carried by the binary row null bitmap
- The null bitmap uses MySQL's prepared-statement row offset of `2`
- Non-null values are encoded in column order, without per-cell packet headers

Type-specific result encoding:
- `DataType::Bool` → MySQL `TINY` (`0x01`), one payload byte `0x00` or `0x01`
- `DataType::Int` → MySQL `LONG` (`0x03`), 4-byte little-endian signed integer
- `DataType::BigInt` → MySQL `LONGLONG` (`0x08`), 8-byte little-endian signed integer
- `DataType::Real` → MySQL `DOUBLE` (`0x05`), 8-byte little-endian IEEE-754
- `DataType::Decimal` → MySQL `NEWDECIMAL` (`0xf6`), length-encoded exact ASCII decimal string
- `DataType::Text` → MySQL `VAR_STRING` (`0xfd`), length-encoded UTF-8 bytes
- `DataType::Bytes` → MySQL `BLOB` (`0xfc`), length-encoded raw bytes
- `DataType::Date` → MySQL `DATE` (`0x0a`), binary date payload `[4][year u16 LE][month][day]`
- `DataType::Timestamp` → MySQL `TIMESTAMP` (`0x07`), binary datetime payload:
  - `[7][year u16 LE][month][day][hour][min][sec]` when microseconds are zero
  - `[11][year u16 LE][month][day][hour][min][sec][micros u32 LE]` when microseconds are non-zero
- `DataType::Uuid` → MySQL `VAR_STRING` (`0xfd`), length-encoded canonical UUID string

`Value::Null` is never serialized as text in binary rows. It only sets the
corresponding null-bitmap bit and emits no value bytes.

This subphase also aligns the column type codes emitted by AxiomDB's shared
`ColumnDefinition41` builder with the binary row payloads:
- `Bool` stops advertising `BIT` and advertises `TINY`
- `Decimal` stops advertising legacy `DECIMAL` and advertises `NEWDECIMAL`

## Inputs / Outputs

- Input:
  - `QueryResult`
  - `seq_start: u8`
  - `QueryResult::Rows { columns, rows }` where `rows[i].len() == columns.len()`
  - `rows[i][j]` matches the semantic type described by `columns[j].data_type`
- Output:
  - `PacketSeq = Vec<(u8, Vec<u8>)>` ready for MySQL packet framing
- Errors:
  - No new user-visible SQL errors are introduced by this subphase
  - Existing `DbError -> ERR_Packet` mapping remains unchanged
  - Serializer behavior continues to rely on the existing `QueryResult` invariants;
    this subphase does not introduce a new fallible wire-serialization layer

## Use cases

1. A prepared `SELECT big_col, dec_col, blob_col, day_col, ts_col FROM t`
   returns binary row packets where:
   - `BIGINT` is 8-byte LE
   - `DECIMAL` preserves exact decimal text
   - `BLOB` preserves raw bytes
   - `DATE` is binary Y/M/D, not `"YYYY-MM-DD"`
   - `TIMESTAMP` is binary Y/M/D/H/M/S/(micros), not text

2. A prepared `SELECT a, b, c FROM t` with `b = NULL` sets only the null-bitmap
   bit for column `b`; there is no `0xfb` text-protocol marker in the row payload.

3. A normal `COM_QUERY SELECT ...` still returns text protocol rows exactly as
   before this subphase.

4. A prepared statement with zero parameters still uses the binary result row
   format on `COM_STMT_EXECUTE`; binary output is not gated on having bound params.

## Acceptance criteria

- [ ] `COM_STMT_EXECUTE` with `QueryResult::Rows` emits binary row packets whose first byte is `0x00`
- [ ] The binary row null bitmap uses MySQL's prepared-row offset `2`; a null in column `0` sets bit position `2`, not bit position `0`
- [ ] `Value::Null` in a prepared result row is encoded only through the null bitmap and does not write any cell payload bytes
- [ ] `DataType::BigInt` rows are encoded as signed 8-byte little-endian values, not decimal text
- [ ] `DataType::Decimal` rows are encoded as length-encoded exact ASCII decimals, not as `DOUBLE` and not via lossy float formatting
- [ ] `DataType::Bytes` rows preserve raw bytes exactly and never go through UTF-8 lossless/lossy text conversion
- [ ] `DataType::Date` rows are encoded as binary date payloads `[4][year u16 LE][month][day]`, not as `"YYYY-MM-DD"`
- [ ] `DataType::Timestamp` rows use 7-byte or 11-byte datetime payloads depending on whether microseconds are zero
- [ ] `DataType::Bool` advertises MySQL `TINY` and encodes as one byte `0x00` or `0x01`
- [ ] `DataType::Decimal` advertises MySQL `NEWDECIMAL`, not legacy `DECIMAL`
- [ ] `COM_QUERY` continues to use the existing text serializer; this subphase must not switch all resultsets to binary protocol
- [ ] Prepared `Affected` and `Empty` results still return OK_Packets; only row resultsets change format
- [ ] Protocol tests verify both the metadata type code and the raw row bytes for the same resultset, so a mismatched metadata/payload implementation cannot pass
- [ ] `tools/wire-test.py` contains an end-to-end prepared-statement smoke test that reads the raw `COM_STMT_EXECUTE` row packet and proves the live server is sending binary rows

## Out of scope

- Reworking `COM_STMT_PREPARE` result metadata inference for computed expressions
- Cursor mode / `COM_STMT_FETCH`
- New SQL data types not currently represented in `axiomdb_types::Value`
- Changing warning propagation semantics for prepared statements

## Dependencies

- `crates/axiomdb-network/src/mysql/handler.rs`
- `crates/axiomdb-network/src/mysql/result.rs`
- `crates/axiomdb-network/src/mysql/prepared.rs`
- `crates/axiomdb-network/src/mysql/session.rs`
- `crates/axiomdb-network/tests/integration_protocol.rs`
- `tools/wire-test.py`
