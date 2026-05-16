# Spec: Integer Types Completeness

Phase: 24 — Complete Types
Task: 24.1 — TINYINT fix, SMALLINT wire type, BIGSERIAL
Spec: specs/fase-24/spec-24.1-integer-types.md
Status: approved

## Context

AxiomDB has three integer data types in its executor: `DataType::Bool` (i8-ish, wire 0x01 TINY), `DataType::Int` (i32, wire 0x03 LONG), `DataType::BigInt` (i64, wire 0x08 LONGLONG). The parser currently maps `TINYINT → DataType::Bool` (wrong — Bool is 0/1 only) and `SMALLINT → DataType::Int` (semantically OK for storage but sends wire type 0x03 LONG instead of 0x02 SHORT, breaking MySQL clients). `BIGSERIAL` is not recognized at all. This spec fixes all three gaps without introducing new runtime value types.

## Goal

Make `TINYINT`, `SMALLINT`, and `BIGSERIAL` behave as MySQL/PostgreSQL clients expect: correct wire type codes (0x01, 0x02, 0x08), correct i8/i16/i64 overflow errors on insert, and BIGSERIAL auto-increment semantics.

## Non-goals

- Not adding `UNSIGNED` integer semantics — MySQL UNSIGNED is out of scope for 24.1; unrecognized `UNSIGNED` modifier is silently accepted (current behavior preserved).
- Not adding `MEDIUMINT` (24.14b handles MySQL-specific aliases).
- Not changing on-disk storage width — TinyInt and SmallInt store as 8-byte i64 (same as Int/BigInt) in the current heap codec. Narrowing on-disk to 1 or 2 bytes is a storage optimization deferred.
- Not changing `DataType::Bool` — Bool remains the boolean type; it is NOT an integer type.
- Not changing the B-Tree key encoder for TinyInt/SmallInt — both use the same i64 encoding as Int/BigInt.

## Behavior

### 1. TINYINT

**Parser fix** (`ddl.rs`):
```rust
// Before (BUG):
Token::Ident(s) if s.eq_ignore_ascii_case("TINYINT") => (DataType::Bool, 0, false)

// After:
Token::Ident(s) if s.eq_ignore_ascii_case("TINYINT") => (DataType::TinyInt, 0, false)
```

**New DataType variant** (`axiomdb-types/src/types.rs`):
```rust
pub enum DataType {
    // ... existing variants ...
    TinyInt,   // i8: -128..=127
    SmallInt,  // i16: -32768..=32767
}
```

**New ColumnType variants** (`axiomdb-catalog/src/schema.rs`):
```rust
pub enum ColumnType {
    // existing 0..18 ...
    TinyInt = 19,
    SmallInt = 20,
}
```

**Overflow check on insert**: when inserting a literal or bound value into a TinyInt column, validate range `-128..=127`. For SmallInt: `-32768..=32767`. Overflow returns `DbError::InvalidValue("value N out of range for TINYINT")`.

**Arithmetic**: TinyInt and SmallInt participate in expressions as `DataType::Int` (promoted). No separate arithmetic type.

**Wire type codes** (`axiomdb-network/src/mysql/result.rs`):
```rust
DataType::TinyInt  => 0x01,  // TINY
DataType::SmallInt => 0x02,  // SHORT
DataType::Int      => 0x03,  // LONG
DataType::BigInt   => 0x08,  // LONGLONG
```

**DataType → ColumnType mapping**:
```
DataType::TinyInt  → ColumnType::TinyInt  = 19
DataType::SmallInt → ColumnType::SmallInt = 20
```

**ColumnType → DataType mapping** (for reads from disk):
```
ColumnType::TinyInt  → DataType::TinyInt
ColumnType::SmallInt → DataType::SmallInt
```

### 2. SMALLINT

Same as TinyInt above but:
- Wire type: 0x02 SHORT
- Range: -32768..=32767
- ColumnType::SmallInt = 20

Parser already emits `DataType::Int` for SMALLINT — change to `DataType::SmallInt`.

### 3. BIGSERIAL

`BIGSERIAL` is syntactic sugar for `BIGINT NOT NULL AUTO_INCREMENT` (MySQL) / `BIGINT GENERATED ALWAYS AS IDENTITY` (PG). Implementation: the parser recognizes `BIGSERIAL` and sets `DataType::BigInt` with `auto_increment = true`. No new type required.

```rust
Token::Ident(s) if s.eq_ignore_ascii_case("BIGSERIAL") => {
    p.advance();
    (DataType::BigInt, 0, true)  // auto_increment = true
}
```

Behavior identical to `BIGINT AUTO_INCREMENT` — uses the existing SERIAL/SEQUENCE machinery.

### Expression promotion rules

When TinyInt or SmallInt appear in arithmetic expressions, they are promoted to Int for computation:
```
TinyInt op TinyInt  → Int result
SmallInt op SmallInt → Int result
TinyInt op SmallInt  → Int result
TinyInt/SmallInt op Int → Int result
TinyInt/SmallInt op BigInt → BigInt result
```

This matches PostgreSQL/MySQL behavior. The promotion happens in `eval.rs` — no new arithmetic cases needed if `Value::Int(i64)` already handles the promoted result.

### Value representation

No new `Value` variant is required. TinyInt and SmallInt values are stored as `Value::Int(i64)` at runtime. The type information (TinyInt vs SmallInt vs Int) flows through `DataType` in column metadata, not through the value itself. This keeps eval code unchanged.

**Range check on insert only** — enforce the narrow range when writing to a column declared TinyInt/SmallInt, not on every arithmetic result.

### SHOW COLUMNS / INFORMATION_SCHEMA

`SHOW COLUMNS FROM t` must return the declared type name, not `int`:
- TinyInt → `tinyint`
- SmallInt → `smallint`
- Int → `int`
- BigInt → `bigint`
- BigSerial → `bigint` (stored as BigInt after parse)

### Error cases

| Scenario | Expected error | Message |
|---|---|---|
| INSERT 200 into TINYINT col | `DbError::InvalidValue` | `"value 200 out of range for TINYINT"` |
| INSERT -129 into TINYINT col | `DbError::InvalidValue` | `"value -129 out of range for TINYINT"` |
| INSERT 40000 into SMALLINT col | `DbError::InvalidValue` | `"value 40000 out of range for SMALLINT"` |
| INSERT NULL into TinyInt NOT NULL | `DbError::NullConstraintViolation` | existing behavior |
| BIGSERIAL col, insert explicit value | `DbError::InvalidValue` or silently accepted | match SERIAL behavior |

## Edge cases

- [ ] INSERT value at exact boundary: -128, 127 for TINYINT — must succeed
- [ ] INSERT NULL into nullable TINYINT column — must succeed
- [ ] Arithmetic overflow on TinyInt result is NOT checked (promoted to Int)
- [ ] CAST(x AS TINYINT) — should range-check x
- [ ] `CREATE TABLE t (id BIGSERIAL PRIMARY KEY)` — auto_increment + BigInt
- [ ] ALTER TABLE with TINYINT/SMALLINT columns (if ALTER is supported)
- [ ] Wire type codes verified in wire test
- [ ] SHOW COLUMNS returns correct type name

## On-disk format

TinyInt and SmallInt use the same 8-byte i64 little-endian encoding as Int and BigInt in the heap codec. The ColumnType tag (19, 20) stored in the catalog is the only difference. No migration needed for existing tables.

## Performance budget

No performance impact — same value size on disk, same Value::Int runtime representation. No benchmark needed.

## Dependencies

- Depends on: existing `DataType`, `ColumnType`, `Value` machinery (all stable)
- Blocks: 24.14b MySQL type aliases (which also touches TINYINT/MEDIUMINT)

## Open questions

None — all resolved during brainstorm.

## Done criteria

- [ ] `TINYINT` column stores values -128..127; insert of 128 or -129 returns error
- [ ] `SMALLINT` column stores values -32768..32767; overflow returns error
- [ ] Wire type code for TINYINT column = 0x01 (verified via pymysql field descriptor)
- [ ] Wire type code for SMALLINT column = 0x02 (verified via pymysql field descriptor)
- [ ] `BIGSERIAL` recognized as `BIGINT AUTO_INCREMENT`
- [ ] `SHOW COLUMNS` returns `tinyint` / `smallint` type names
- [ ] `cargo nextest run --workspace` (Lima VM) passes — 0 regressions
- [ ] `cargo clippy --workspace -- -D warnings` (Lima VM) passes
- [ ] `cargo fmt --check` passes
- [ ] Wire smoke test with 4+ assertions added to `tools/wire-test.py`

## References

- MySQL 8.0 manual: Integer Types (exact numeric)
- PostgreSQL 17: SMALLINT, BIGSERIAL
- Current ddl.rs: `crates/axiomdb-sql/src/parser/ddl.rs` lines 1220-1250
- Wire encoding: `crates/axiomdb-network/src/mysql/result.rs`
- ColumnType: `crates/axiomdb-catalog/src/schema.rs`
- DataType: `crates/axiomdb-types/src/types.rs`
