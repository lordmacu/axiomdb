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

## Subphases planned

| ID | Status | Description |
|----|--------|-------------|
| 24.1 | ✅ Done | TINYINT, SMALLINT, BIGSERIAL |
| 24.1b | ✅ Done | SERIAL / SMALLSERIAL aliases |
| 24.1c | ⏳ | GENERATED ALWAYS AS IDENTITY |
| 24.2 | ✅ Done | REAL/FLOAT4 distinct from DOUBLE |
| 24.3 | ⏳ | Exact DECIMAL (rust_decimal) |
| 24.4 | ⏳ | CITEXT |
| 24.5 | ⏳ | BYTEA/BLOB with TOAST |
| 24.6 | ⏳ | BIT(n) / VARBIT(n) |
| 24.7 | ⏳ | TIMESTAMPTZ |
| 24.8 | ⏳ | INTERVAL |
| 24.10 | ⏳ | INET, CIDR, MACADDR |
| 24.13 | ⏳ | Domain types |
| 24.14b | ⏳ | MySQL type aliases (TINYTEXT, MEDIUMTEXT, etc.) |
