# Spec: REAL/FLOAT4 separate from DOUBLE (f32 vs f64)

Phase: 24 — Complete type system
Task: 24.2 — Distinct REAL (f32) and DOUBLE (f64) types
Status: approved

## Context

Currently `TyReal | TyDouble | TyFloat` all collapse to `DataType::Real` (f64,
8-byte LE on disk, wire 0x05 DOUBLE). MySQL and PostgreSQL distinguish FLOAT/REAL
(4-byte f32, wire 0x04) from DOUBLE PRECISION (8-byte f64, wire 0x05). This causes
MySQL clients to receive wrong wire types for FLOAT/REAL columns. This subphase
adds `DataType::Float` (f32-precision) as a distinct type while keeping `DataType::Real`
as the f64/DOUBLE type.

## Goal

Add `DataType::Float` (REAL/FLOAT4/FLOAT — f32 precision, 4-byte on disk, wire 0x04)
as a type distinct from `DataType::Real` (DOUBLE — f64, 8-byte on disk, wire 0x05).

## Non-goals

- New `Value::Float(f32)` variant — deferred; runtime uses `Value::Real(f64)` for both
- `FLOAT(n)` precision-based dispatch (n ≤ 24 → f32, n > 24 → f64) — not implemented;
  bare FLOAT → f32, bare DOUBLE → f64
- NaN / Infinity handling changes — not in scope
- Unsigned float types — not standard SQL

## Behavior

### Type mapping

| SQL keyword(s) | DataType | ColumnType | On-disk | Wire type | MySQL name |
|----------------|----------|-----------|---------|-----------|------------|
| `REAL`, `FLOAT`, `FLOAT4` | `DataType::Float` | `ColumnType::Float32 = 21` | 4 bytes LE f32 | 0x04 FLOAT | FLOAT |
| `DOUBLE`, `DOUBLE PRECISION`, `FLOAT8` | `DataType::Real` | `ColumnType::Float = 4` | 8 bytes LE f64 | 0x05 DOUBLE | DOUBLE |

`DataType::Real` behavior is **unchanged** — existing tables with DOUBLE/REAL columns
use `ColumnType::Float = 4` and continue to decode as f64.

### Semantics

- **Precision truncation:** Inserting a f64 value into a `DataType::Float` column
  silently rounds to f32 precision at encode time. No error — this is standard SQL
  behavior for narrowing float conversions.
- **Null:** `Value::Null` is accepted in any nullable Float column.
- **Runtime representation:** `Value::Real(f64)` is used for both Float and Double at
  runtime. Float values are decoded from 4-byte f32 bytes, widened to f64.
- **SHOW COLUMNS:** Float → `"real"`, Real (Double) → `"double"`.
- **Arithmetic:** All arithmetic operates on `Value::Real(f64)` regardless of column
  type — no change to expression evaluation.

### Error cases

| Input | Expected | Notes |
|-------|----------|-------|
| `NaN` inserted into Float column | `DbError::InvalidValue` | same as existing Real behavior |
| Text that doesn't parse as float | `DbError::InvalidValue` | same as existing Real behavior |

## Edge cases

- [ ] NULL in nullable REAL column round-trips
- [ ] f32 precision boundary: `3.1415927` inserted into REAL → rounded to f32 on select
- [ ] `FLOAT` keyword (no size) → DataType::Float (f32)
- [ ] `FLOAT4` keyword → DataType::Float (f32)
- [ ] `DOUBLE` keyword → DataType::Real (f64, unchanged)
- [ ] `DOUBLE PRECISION` (two tokens) → DataType::Real (f64, unchanged)
- [ ] `FLOAT8` keyword → DataType::Real (f64)
- [ ] `SHOW COLUMNS` reports `real` for Float, `double` for Real
- [ ] Existing DOUBLE columns (ColumnType::Float=4) still decode correctly

## On-disk format

```
REAL / FLOAT column (ColumnType::Float32 = 21):
  offset  size  field
  0       4     value   f32 little-endian IEEE 754

DOUBLE column (ColumnType::Float = 4) — UNCHANGED:
  offset  size  field
  0       8     value   f64 little-endian IEEE 754
```

Compatibility: Float32 columns are new; Float (f64) columns are unaffected.

## Performance budget

No meaningful impact — encode/decode changes only swap 4-byte vs 8-byte paths.

## Dependencies

- Depends on: 24.1 (array_codec local ColumnType pattern established, next tag = 21)
- Blocks: nothing

## Open questions

None — all resolved in brainstorm.

## Done criteria

- [ ] `CREATE TABLE t (x REAL)` stores 4 bytes per row; `SELECT x` returns f32-rounded value
- [ ] `CREATE TABLE t (x DOUBLE)` unchanged — 8 bytes per row
- [ ] Wire type for REAL column is 0x04 (FLOAT); for DOUBLE is 0x05 (DOUBLE)
- [ ] SHOW COLUMNS reports `real` for Float columns, `double` for Real/Double columns
- [ ] NULL in REAL column works
- [ ] f32 precision test: `3.1415927` inserted and retrieved is f32-rounded
- [ ] `FLOAT`, `FLOAT4`, `REAL` all parse as DataType::Float
- [ ] `DOUBLE`, `DOUBLE PRECISION`, `FLOAT8` parse as DataType::Real
- [ ] Existing behavior of `DataType::Real` (DOUBLE) unchanged — regression test
- [ ] 8 integration tests in `integration_float_types.rs`
- [ ] 4 wire assertions under `[24.2 float]`
- [ ] `cargo nextest run --workspace` clean
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo fmt --check` clean

## References

- Implemented pattern: `specs/fase-24/spec-24.1-integer-types.md` (same codec/wire structure)
- PostgreSQL: REAL = float4, DOUBLE PRECISION = float8 (PG docs §8.1.3)
- MySQL: FLOAT (4-byte 0x04), DOUBLE (8-byte 0x05) (MySQL 8.0 §13.1.20)
- local ColumnType copy: `crates/axiomdb-types/src/array_codec.rs` — must be updated in sync
