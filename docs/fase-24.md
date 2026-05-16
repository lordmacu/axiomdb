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

## Subphases planned

| ID | Status | Description |
|----|--------|-------------|
| 24.1 | ✅ Done | TINYINT, SMALLINT, BIGSERIAL |
| 24.1b | ✅ Done | SERIAL / SMALLSERIAL aliases |
| 24.1c | ⏳ | GENERATED ALWAYS AS IDENTITY |
| 24.2 | ⏳ | REAL/FLOAT4 distinct from DOUBLE |
| 24.3 | ⏳ | Exact DECIMAL (rust_decimal) |
| 24.4 | ⏳ | CITEXT |
| 24.5 | ⏳ | BYTEA/BLOB with TOAST |
| 24.6 | ⏳ | BIT(n) / VARBIT(n) |
| 24.7 | ⏳ | TIMESTAMPTZ |
| 24.8 | ⏳ | INTERVAL |
| 24.10 | ⏳ | INET, CIDR, MACADDR |
| 24.13 | ⏳ | Domain types |
| 24.14b | ⏳ | MySQL type aliases (TINYTEXT, MEDIUMTEXT, etc.) |
