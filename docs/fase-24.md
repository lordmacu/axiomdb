# Phase 24 — Complete Type System

## Overview

Phase 24 fills gaps in the type system left by earlier phases: integer sub-types
(TINYINT, SMALLINT), auto-increment sugar (BIGSERIAL), and eventually CITEXT,
BYTEA/BLOB, BIT(n), INTERVAL, INET/CIDR, and domain types.

---

## 24.1 — Integer Types Completeness (2026-05-16)

### Problem

Three integer-type issues inherited from Phase 5 (initial type system):

1. **TINYINT mapped to `DataType::Bool`** in the parser (`ddl.rs:1223`).
   Semantically wrong: Bool is 0/1, TINYINT is a signed i8 numeric.
2. **SMALLINT sent wire type code `0x03` (LONG)** instead of `0x02` (SHORT).
   MySQL/MariaDB clients expect 2-byte encoding for SMALLINT.
3. **BIGSERIAL not recognized** — the parser had no `BIGSERIAL` token handling.

### Solution

#### New type variants

`DataType::TinyInt` and `DataType::SmallInt` added to `axiomdb-types`. Both
store as `Value::Int(i32)` at runtime — no new Value variant needed. Type
distinction matters only at insert (range check) and wire protocol (byte width).

`ColumnType::TinyInt = 19` and `ColumnType::SmallInt = 20` added to
`axiomdb-catalog`. A local copy of `ColumnType` in `axiomdb-types/src/array_codec.rs`
(kept to break the cyclic dependency) was updated in sync.

#### Range-checked coercion

In `axiomdb-types/src/coerce_api.rs`:
- `(Value::Int(n), DataType::TinyInt)` → error if `n < -128 || n > 127`
- `(Value::Int(n), DataType::SmallInt)` → error if `n < -32768 || n > 32767`
- Same checks for `Value::BigInt` and `Value::Text` (parse then range-check) inputs

Error format: `DbError::InvalidValue { reason }` with value and type name in message.

#### Wire protocol

In `axiomdb-network/src/mysql/result.rs`:
- `DataType::TinyInt` → column type `0x01` (TINY), 1-byte binary encoding
- `DataType::SmallInt` → column type `0x02` (SHORT), 2-byte LE encoding
- Display lengths: TinyInt=4 (`"-128"`), SmallInt=6 (`"-32768"`)

#### BIGSERIAL

Parsed in `parse_column_def` before the normal type dispatch: if the column ident
token is `BIGSERIAL`, the column type is synthesized as `DataType::BigInt` and
`ColumnConstraint::AutoIncrement` is pushed automatically. This is pure sugar with
no new AST nodes or ColumnType variants.

#### Parser fixes

- `TINYINT` token: `(DataType::Bool, 0, false)` → `(DataType::TinyInt, 0, false)`
- `SMALLINT` token: `(DataType::Int, 0, false)` → `(DataType::SmallInt, 0, false)`

### Files changed

| Crate | File | Change |
|-------|------|--------|
| axiomdb-types | `src/types.rs` | `DataType::TinyInt`, `DataType::SmallInt` variants + `name()` |
| axiomdb-types | `src/codec.rs` | decode, validate_type, skip, col_type_to_data_type arms |
| axiomdb-types | `src/coerce_api.rs` | range-checked coercion arms (Int, BigInt, Text → TinyInt/SmallInt) |
| axiomdb-types | `src/field_patch.rs` | `fixed_encoded_size` (same 4 bytes as Int) |
| axiomdb-types | `src/array_codec.rs` | local ColumnType copy + encode/decode/data_type_to_column_type arms |
| axiomdb-types | `src/array_io.rs` | text-parse arm |
| axiomdb-catalog | `src/schema_database.rs` | `ColumnType::TinyInt=19`, `SmallInt=20` + TryFrom |
| axiomdb-catalog | `src/schema.rs` | roundtrip test + invalid discriminant updated |
| axiomdb-catalog | `src/schema_aggregate.rs` | data_type_tag/from_tag |
| axiomdb-sql | `src/parser/ddl.rs` | TINYINT/SMALLINT fix, BIGSERIAL synthesis, FDW arms |
| axiomdb-sql | `src/executor/ddl_show.rs` | `column_type_to_sql_name`, `scalar_type_to_sql_name` |
| axiomdb-sql | `src/executor/shared.rs` | datatype↔column_type conversion |
| axiomdb-sql | `src/executor/information_schema_exec.rs` | IS data type strings |
| axiomdb-sql | `src/executor/fdw_http.rs` | JSON→AxiomDB value mapping |
| axiomdb-sql | `src/planner_select.rs` | literal coercion |
| axiomdb-sql | `src/table.rs` | 4 conversion/coercion arms |
| axiomdb-sql | `src/table_ctx.rs` | ZoneMap numeric check |
| axiomdb-sql | `src/json_table.rs` | datatype_to_column_type |
| axiomdb-sql | `src/values_clause.rs` | datatype_to_column_type |
| axiomdb-sql | `tests/integration_integer_types.rs` | 11 new integration tests |
| axiomdb-network | `src/mysql/result.rs` | wire type codes + binary encoding |
| tools | `wire-test.py` | 13 new 24.1 assertions |

### Tests

Integration: 11 tests in `crates/axiomdb-sql/tests/integration_integer_types.rs`:
- TINYINT insert/select, boundary values (-128, 127), overflow (+128, -129), NULL
- SMALLINT insert/select, boundary values (-32768, 32767), overflow (40000, -40000)
- BIGSERIAL auto-increment (3 rows, ids 1/2/3)
- SHOW COLUMNS reports `tinyint`/`smallint` type names

Wire smoke: 13 assertions in `tools/wire-test.py` under `[24.1 int_types]`.

---

## 24.2 — REAL/FLOAT4 vs DOUBLE/FLOAT8 (2026-05-16)

### Problem

Before this phase, both `REAL` and `DOUBLE` mapped to the single `DataType::Real`
(f64, 8 bytes on disk). SQL standard defines REAL as 4-byte IEEE 754 single-precision,
while DOUBLE PRECISION / FLOAT8 is 8-byte double-precision.

### Solution

#### New type variant

`DataType::Float` added to `axiomdb-types` — the SQL REAL / FLOAT4 / FLOAT type
(f32 precision, 4-byte LE on disk). At runtime both `Float` and `Real` use
`Value::Real(f64)`; distinction only matters for encode/decode and wire protocol.

`ColumnType::Float32 = 21` added to `axiomdb-catalog` (and the local copy in
`array_codec.rs`).

#### Parser changes

`Token::TyReal` and `Token::TyFloat` → `DataType::Float` (REAL and FLOAT are
both f32 per standard). `Token::TyDouble` → `DataType::Real` (f64).

New ident handlers: `FLOAT4` → Float, `FLOAT8` → Real,
`DOUBLE PRECISION` → Real (consumes optional `PRECISION` token).

#### Codec changes

- `encode_row` loop now zips with schema: `Value::Real(f)` on a `Float` column
  writes `(*f as f32).to_le_bytes()` (4 bytes); on Real column writes 8 bytes.
- `decode_row` / `decode_row_masked`: new `DataType::Float` arm reads 4-byte f32
  and widens to `Value::Real(f as f64)`.
- Skip section: `DataType::Float` added to the 4-byte skip arm.

#### Wire protocol

- `DataType::Float` → column type `0x04` (FLOAT), 4-byte binary, display_len=12
- `DataType::Real` → column type `0x05` (DOUBLE), 8-byte binary (unchanged)

#### Precision truncation

`coerce_api.rs`: Int/BigInt/Text → Float arms cast through f32 to apply f32
precision at insert time. `field_patch.rs` updated for Float (4-byte fixed-size
in-place patch). `batch.rs` SIMD path added Float arm.

### Files changed

| Crate | File | Change |
|-------|------|--------|
| axiomdb-types | `src/types.rs` | `DataType::Float` + `name()` |
| axiomdb-types | `src/codec.rs` | encode (schema-aware), decode, skip, col_type_to_data_type |
| axiomdb-types | `src/coerce_api.rs` | Float coerce arms (Int/BigInt/Text→Float) |
| axiomdb-types | `src/coerce_helpers.rs` | `value_matches_type`: Float identity |
| axiomdb-types | `src/field_patch.rs` | Float 4-byte fixed size + encode/decode |
| axiomdb-types | `src/array_codec.rs` | Float32=21, encode/decode arms |
| axiomdb-catalog | `src/schema_database.rs` | `ColumnType::Float32=21` + TryFrom |
| axiomdb-catalog | `src/schema_aggregate.rs` | data_type_tag/from_tag |
| axiomdb-sql | `src/parser/ddl.rs` | REAL/FLOAT→Float, DOUBLE→Real, FLOAT4/FLOAT8 idents, DOUBLE PRECISION |
| axiomdb-sql | `src/executor/ddl_show.rs` | Float32→"REAL", Float→"DOUBLE" |
| axiomdb-sql | `src/executor/shared.rs` | datatype↔column_type conversions |
| axiomdb-sql | `src/executor/information_schema_exec.rs` | IS type names + precision |
| axiomdb-sql | `src/executor/fdw_http.rs` | JSON→Float32 arm |
| axiomdb-sql | `src/eval/batch.rs` | SIMD batch path Float arm, fixed_size |
| axiomdb-sql | `src/planner_select.rs` | ColumnType→DataType |
| axiomdb-sql | `src/table.rs` | 4 ColumnType→DataType conversion arms |
| axiomdb-sql | `src/table_write.rs` | Float 4-byte encode arm |
| axiomdb-sql | `src/json_table.rs` | Float→ColumnType::Float32 |
| axiomdb-sql | `src/values_clause.rs` | value_type Float |
| axiomdb-sql | `tests/integration_float_types.rs` | 8 new integration tests (NEW FILE) |
| axiomdb-network | `src/mysql/result.rs` | wire 0x04/0x05 + binary encode |
| axiomdb-embedded | `tests/integration.rs` | REAL precision tolerance fix |
| tools | `wire-test.py` | 4 new 24.2 assertions |

### Tests

Integration: 8 tests in `crates/axiomdb-sql/tests/integration_float_types.rs`:
- REAL insert/select (f32 round-trip), REAL maps to `DataType::Float`
- DOUBLE insert/select (full f64 precision), DOUBLE maps to `DataType::Real`
- FLOAT4 / FLOAT8 aliases parse to correct DataType
- DOUBLE PRECISION alias → Real
- f32 precision truncation verified (π stored as REAL ≠ π at f64 precision)

Wire smoke: 4 assertions in `tools/wire-test.py` under `[24.2 float]`.

---

## 24.3 — Exact DECIMAL(p,s) Precision (2026-05-17)

### Problem

DECIMAL columns accepted values without enforcing precision or scale:
- `DECIMAL(10,2)` would store 7-digit fractional values unchanged
- No rounding on insert
- Division produced truncated integer mantissa (e.g., `10/3 = 3`)
- ROUND() and TRUNC() had no Decimal arm (fell through to TypeMismatch)
- SHOW COLUMNS/SHOW CREATE TABLE returned bare "DECIMAL" instead of "decimal(10,2)"
- A second SHOW COLUMNS dispatch path in `exec_entry.rs` bypassed `column_sql_type_display`

### Solution

**Parser** (`parser/ddl.rs`): `parse_decimal_params` captures `DECIMAL(p,s)` and
validates `1 ≤ p ≤ 38`, `0 ≤ s ≤ p`. Parameters are encoded as
`type_len = (precision << 8) | scale` in `ColumnDef.type_len`. Bare `DECIMAL` →
`(10, 0)`. The field already existed and was already persisted to disk (flags bit2).

**Enforcement at insert** (`table.rs`): `enforce_decimal_precision(value, p, s)` uses
`rust_decimal::RoundingStrategy::MidpointAwayFromZero` (ROUND_HALF_UP) to round to `s`
decimal places, then rejects if the integer part exceeds `p - s` digits.
Called from `coerce_values` and `coerce_values_with_ctx` for Decimal columns.

**Division** (`eval/ops.rs`): `decimal_arith` Div arm now scales the numerator by
`10^(s2 + extra)` where `extra = min(6, 38 - s1)`, producing up to 6 extra fractional
digits instead of integer-division truncation.

**ROUND / TRUNC** (`eval/functions/numeric.rs`): Added `Value::Decimal` arms:
- `round`: uses `rust_decimal::round_dp_with_strategy(MidpointAwayFromZero)`, returns `Decimal(mantissa, new_scale)`
- `truncate`/`trunc`: pure i128 arithmetic — divide mantissa by `10^(s - new_scale)`

**SHOW COLUMNS / SHOW CREATE TABLE**: Fixed both dispatch paths:
- `executor/ddl_show.rs::execute_show_columns` → already used `column_sql_type_display`
- `executor/exec_entry.rs::Stmt::ShowColumns` → was using `column_type_to_sql_name` (returned "DECIMAL"); fixed to use `column_sql_type_display`
- `executor/exec_entry.rs::Stmt::ShowCreateTable` → same fix

### Files changed

| Crate | File | Change |
|-------|------|--------|
| axiomdb-sql | `src/parser/ddl.rs` | `parse_decimal_params` — captures (p,s) into type_len |
| axiomdb-sql | `src/executor/ddl_show.rs` | `column_sql_type_display`: Decimal early-return → "decimal(p,s)" |
| axiomdb-sql | `src/executor/exec_entry.rs` | ShowColumns + ShowCreateTable: use `column_sql_type_display` |
| axiomdb-sql | `src/executor/information_schema_exec.rs` | IS.COLUMNS: NUMERIC_PRECISION/SCALE from type_len |
| axiomdb-sql | `src/table.rs` | `enforce_decimal_precision` + `decimal_precision_scale` helper |
| axiomdb-sql | `src/eval/ops.rs` | `decimal_arith` Div: scale_up = s2 + extra fractional digits |
| axiomdb-sql | `src/eval/functions/numeric.rs` | `round`/`trunc`/`truncate`: Value::Decimal arms |
| axiomdb-sql | `Cargo.toml` | `rust_decimal = "1"` dependency |
| axiomdb-sql | `tests/integration_decimal_precision.rs` | 17 new integration tests (NEW FILE) |
| axiomdb-sql | `tests/integration_date_decimal_columns.rs` | Updated: bare DECIMAL now rounds to scale 0 |
| tools | `wire-test.py` | 11 new [24.3 decimal] assertions |

### Tests

17 integration tests in `crates/axiomdb-sql/tests/integration_decimal_precision.rs`:
- Parser: stores decimal(p,s), rejects precision=0, precision>38, scale>precision
- NUMERIC alias maps to decimal(p,s)  
- Insert: rounds to scale (HALF_UP), rejects integer overflow, handles NULL
- Bare DECIMAL rounds to 0 decimal places
- Division: produces ~6 extra fractional digits
- ROUND(decimal, n): HALF_UP, no-arg form
- TRUNC/TRUNCATE(decimal, n): truncates without rounding

Wire smoke: 11 assertions in `tools/wire-test.py` under `[24.3 decimal]`.

---

## 24.7 — TIMESTAMPTZ (2026-05-20)

### Problem

AxiomDB had no way to store or query timestamps with timezone awareness.
The existing `Timestamp` type (no-tz) stored wall-clock time with no UTC
normalization, making it impossible to compare timestamps from different
timezones without manual arithmetic.

### Solution

#### New type variants

`DataType::TimestampTz` added to `axiomdb-types/src/types.rs`.
`Value::TimestampTz(i64)` added to `axiomdb-types/src/value.rs` — the i64
stores **microseconds since Unix epoch, always UTC**. No timezone info is stored;
normalization happens at insert time.

`ColumnType::TimestampTz = 22` added to both `axiomdb-catalog/src/schema_database.rs`
and the local copy in `axiomdb-types/src/array_codec.rs`.

#### Parser

`crates/axiomdb-sql/src/parser/ddl.rs`:
- `Token::TyTimestamp` arm checks for `WITH TIME ZONE` continuation → `DataType::TimestampTz`
- `Token::Ident(s) if s.eq_ignore_ascii_case("TIMESTAMPTZ")` → `DataType::TimestampTz`

Both `TIMESTAMPTZ` and `TIMESTAMP WITH TIME ZONE` are recognized.

#### Codec

`crates/axiomdb-types/src/codec.rs`:
- `encode_row`: `Value::TimestampTz(t)` shares the 8-byte LE arm with `Value::Timestamp(t)`
- `decode_row`: `DataType::TimestampTz` arm reads 8 bytes → `Value::TimestampTz`

#### Text parsing

`parse_text_to_timestamptz_micros()` in `coerce_helpers.rs` — pure arithmetic
parser (no `chrono` dependency) that handles:
- `YYYY-MM-DD HH:MM:SS[.ffffff]` — bare datetime, assumed UTC
- `YYYY-MM-DD HH:MM:SS[.ffffff][±HH:MM]` — UTC offset applied at parse time
- `YYYY-MM-DD HH:MM:SS[.ffffff]Z` — explicit UTC

All offsets are normalized out; what is stored is always UTC µs.

#### Coercions

`crates/axiomdb-types/src/coerce_api.rs`:
- `Text → TimestampTz` — parse via `parse_text_to_timestamptz_micros`
- `Text → Timestamp` — same parser, strips tz offset, returns wall-clock
- `Timestamp → TimestampTz` — reinterprets as UTC (identity on bits)
- `TimestampTz → Timestamp` — drops tz assumption (identity on bits)
- `Date → TimestampTz` — midnight UTC of that date

#### AT TIME ZONE

Parsed in `src/parser/expr.rs` as a postfix operator; lowered to internal
function `__at_time_zone(expr, tz_string)` at parse time (no new AST node).

`crates/axiomdb-sql/src/eval/functions/datetime.rs`:
- Validates `tz_string` is `"UTC"`, `"+00:00"`, or `"Z"`
- Returns `Value::TimestampTz(t)` unchanged (UTC stub)
- Non-UTC zones produce `DbError::Unsupported` (full tz DB deferred)

#### Comparison

`crates/axiomdb-sql/src/eval/ops.rs`:
- `TimestampTz ↔ TimestampTz` — direct i64 comparison
- `Timestamp ↔ TimestampTz` — cross-type (reinterpret bits as UTC)
- `TimestampTz ↔ Text` — parse text then compare

#### Wire protocol

`crates/axiomdb-network/src/mysql/result.rs`:
- Column type: `0x07` (TIMESTAMP), display length 25
- Text protocol: `format_timestamptz()` → `"YYYY-MM-DD HH:MM:SS+00"`
- Binary protocol: reuses `encode_binary_timestamp`

#### Exhaustive match coverage

13 additional files updated with `TimestampTz` arms to satisfy Rust's
non-exhaustive match requirement — see `specs/fase-24/plan-24.7-timestamptz.md`
for the complete list.

### Files changed

| Crate | File | Change |
|-------|------|--------|
| axiomdb-types | `src/value.rs` | `Value::TimestampTz(i64)` variant |
| axiomdb-types | `src/types.rs` | `DataType::TimestampTz` variant |
| axiomdb-types | `src/codec.rs` | encode/decode/validate/infer arms |
| axiomdb-types | `src/coerce_helpers.rs` | `parse_text_to_timestamptz_micros`, `value_matches_type` |
| axiomdb-types | `src/coerce_api.rs` | Text/Timestamp/Date → TimestampTz coerce arms |
| axiomdb-types | `src/array_codec.rs` | `ColumnType::TimestampTz=22`, encode/decode |
| axiomdb-types | `src/array_io.rs` | `format_element_text`, `parse_element_text` |
| axiomdb-types | `src/field_patch.rs` | `fixed_encoded_size` 8-byte arm |
| axiomdb-catalog | `src/schema_database.rs` | `ColumnType::TimestampTz=22` |
| axiomdb-catalog | `src/schema_aggregate.rs` | `data_type_tag/from_tag` discriminant 22 |
| axiomdb-sql | `src/parser/ddl.rs` | `TIMESTAMPTZ` + `TIMESTAMP WITH TIME ZONE` |
| axiomdb-sql | `src/parser/expr.rs` | AT TIME ZONE postfix → `__at_time_zone` |
| axiomdb-sql | `src/eval/ops.rs` | comparison arms |
| axiomdb-sql | `src/eval/functions/datetime.rs` | `__at_time_zone` handler |
| axiomdb-sql | `src/eval/functions/mod.rs` | datetime dispatcher |
| axiomdb-sql | `src/eval/batch.rs` | `fixed_size` arm |
| axiomdb-sql | `src/eval/core.rs` | HashableValue, ArrayElemType arms |
| axiomdb-sql | `src/executor/agg_having.rs` | `value_to_key_bytes` |
| axiomdb-sql | `src/executor/copy_to.rs` | CSV/JSON output |
| axiomdb-sql | `src/executor/ddl_show.rs` | `column_type_to_sql_name`, `column_sql_type_display` |
| axiomdb-sql | `src/executor/exec_entry.rs` | SHOW COLUMNS type display |
| axiomdb-sql | `src/executor/fdw_http.rs` | URL string arm |
| axiomdb-sql | `src/executor/information_schema_exec.rs` | 3 IS arms |
| axiomdb-sql | `src/executor/joins.rs` | hash arm |
| axiomdb-sql | `src/executor/select_into_outfile.rs` | outfile arm |
| axiomdb-sql | `src/executor/shared.rs` | datatype↔column_type |
| axiomdb-sql | `src/executor/union.rs` | dedup key |
| axiomdb-sql | `src/index_maintenance.rs` | `flatten_array_elements` |
| axiomdb-sql | `src/json_table.rs` | `datatype_to_column_type` |
| axiomdb-sql | `src/key_encoding.rs` | `encode_value` |
| axiomdb-sql | `src/planner_select.rs` | literal coercion |
| axiomdb-sql | `src/table.rs` | 4 conversion arms |
| axiomdb-sql | `tests/integration_timestamptz.rs` | 12 integration tests (NEW FILE) |
| axiomdb-network | `src/mysql/result.rs` | wire 0x07, text/binary encode, `format_timestamptz` |
| axiomdb-network | `src/mysql/prepared.rs` | `value_to_sql_literal` |
| axiomdb-embedded | `src/lib.rs` | `value_to_cell` |
| tools | `wire-test.py` | 8 new [24.7 TIMESTAMPTZ] assertions |

### Tests

12 integration tests in `crates/axiomdb-sql/tests/integration_timestamptz.rs`:
- DDL: `TIMESTAMPTZ` and `TIMESTAMP WITH TIME ZONE` parse to `DataType::TimestampTz`
- INSERT/SELECT roundtrip: value stored as positive i64 µs
- UTC offset normalization: `+00:00` and bare datetime store identical bits
- Positive offset normalization: `12:00+05:30` = `06:30+00:00`
- NULL handling
- Comparison / ORDER BY with 3 timestamps
- CAST from Text and from Timestamp column
- AT TIME ZONE 'UTC' conversion
- Codec roundtrip (encode_row → decode_row)
- SHOW COLUMNS returns "timestamptz" in type name

Wire smoke: 8 assertions in `tools/wire-test.py` under `[24.7 TIMESTAMPTZ]`.

---

## Subphases planned

| ID | Status | Description |
|----|--------|-------------|
| 24.1 | ✅ Done | TINYINT, SMALLINT, BIGSERIAL |
| 24.1b | ✅ Done | SERIAL / SMALLSERIAL aliases |
| 24.1c | ⏳ | GENERATED ALWAYS AS IDENTITY |
| 24.2 | ✅ Done | REAL/FLOAT4 distinct from DOUBLE |
| 24.3 | ✅ Done | Exact DECIMAL(p,s) — precision enforcement, rounding, display |
| 24.4 | ⏳ | CITEXT |
| 24.5 | ⏳ | BYTEA/BLOB with TOAST |
| 24.6 | ⏳ | BIT(n) / VARBIT(n) |
| 24.7 | ✅ Done | TIMESTAMPTZ — Value::TimestampTz(i64) µs UTC, AT TIME ZONE stub, wire 0x07 |
| 24.8 | ⏳ | INTERVAL |
| 24.10 | ⏳ | INET, CIDR, MACADDR |
| 24.13 | ⏳ | Domain types |
| 24.14b | ⏳ | MySQL type aliases (TINYTEXT, MEDIUMTEXT, etc.) |
