# Spec: SERIAL / SMALLSERIAL type shorthands

Phase: 24 — Complete type system
Task: 24.1b — SERIAL and SMALLSERIAL column type sugar
Status: approved

## Context

Phase 24.1 added `TINYINT`, `SMALLINT`, and `BIGSERIAL`. `BIGSERIAL` is already
implemented as syntactic sugar for `BIGINT NOT NULL AUTO_INCREMENT`. This subphase
completes the serial family by adding `SERIAL` (→ `INT AUTO_INCREMENT`) and
`SMALLSERIAL` (→ `SMALLINT AUTO_INCREMENT`), matching PostgreSQL's convention.
`Token::Serial` already exists in the lexer and is used as a trailing constraint
synonym for `AUTO_INCREMENT`; that behavior is preserved.

## Goal

Allow `SERIAL` and `SMALLSERIAL` as standalone column type keywords, expanding to
the appropriate integer type plus `AUTO_INCREMENT` constraint.

## Non-goals

- `GENERATED ALWAYS AS IDENTITY` — deferred to 24.1c
- Sequence-backed serials (PG internal sequences) — AxiomDB uses AUTO_INCREMENT; no sequences
- `TINYSERIAL` — not standard in PG or MySQL; skip

## Behavior

### Expansion rules

| Written | Expands to | Wire type |
|---------|-----------|-----------|
| `col SERIAL` | `col INT NOT NULL AUTO_INCREMENT` | 0x03 LONG (4 bytes) |
| `col SMALLSERIAL` | `col SMALLINT NOT NULL AUTO_INCREMENT` | 0x02 SHORT (2 bytes) |
| `col BIGSERIAL` | `col BIGINT NOT NULL AUTO_INCREMENT` | 0x08 LONGLONG (8 bytes) — already done |

Expansion happens at parse time in `parse_column_def`, before `parse_column_data_type`
is called. The resulting `ColumnDef` is identical to what explicit form produces.

### Semantics

- `SERIAL` columns auto-increment from 1, same as `INT AUTO_INCREMENT`.
- `SMALLSERIAL` columns auto-increment from 1 with SMALLINT range (-32768..=32767).
  Auto-increment sequence itself is i64, but the stored value is range-checked.
- `NOT NULL` is implied (matches PostgreSQL behavior): auto-increment columns never
  store NULL via the sequence path.
- Additional constraints (`PRIMARY KEY`, `UNIQUE`, etc.) may follow the type keyword.
- Trailing `SERIAL` keyword as a constraint (`col INT SERIAL`) continues to work
  (existing behavior from Phase 4.3c).

### Error cases

| Input | Expected | Reason |
|-------|----------|--------|
| `SMALLSERIAL` column overflows SMALLINT range | `DbError::InvalidValue` | same as SMALLINT insert-time check |
| `SERIAL` / `SMALLSERIAL` with explicit `NULL` constraint | parse succeeds, NULL constraint accepted (no error) | consistent with existing BIGSERIAL behavior |

## Edge cases

- [ ] `SERIAL PRIMARY KEY` — extra constraints after the shorthand keyword work
- [ ] `SMALLSERIAL NOT NULL` — explicit NOT NULL after shorthand is redundant but not an error
- [ ] `SHOW COLUMNS FROM t` reports `int` for SERIAL, `smallint` for SMALLSERIAL
- [ ] Trailing `col INT SERIAL` form still works (regression guard)
- [ ] SMALLSERIAL auto-increment wraps at SMALLINT max → DbError (same as manual insert overflow)

## Performance budget

No measurable impact — pure parse-time transformation.

## Dependencies

- Depends on: 24.1 (SmallInt type complete, BIGSERIAL pattern established)
- Blocks: nothing

## Open questions

None — approach agreed in brainstorm.

## Done criteria

- [ ] `CREATE TABLE t (id SERIAL PRIMARY KEY)` parses and inserts with auto-increment ids
- [ ] `CREATE TABLE t (id SMALLSERIAL PRIMARY KEY)` parses and inserts with auto-increment ids
- [ ] SHOW COLUMNS reports `int` for SERIAL, `smallint` for SMALLSERIAL
- [ ] `col INT SERIAL` trailing form still works (regression)
- [ ] 4 new integration tests pass (serial_auto_increments, smallserial_auto_increments, serial_show_columns, serial_trailing_regression)
- [ ] 4 new wire smoke assertions pass
- [ ] `cargo nextest run -p axiomdb-sql` clean
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo fmt --check` clean

## References

- Implemented pattern: `specs/fase-24/spec-24.1-integer-types.md`
- Parser location: `crates/axiomdb-sql/src/parser/ddl.rs` — `parse_column_def`
- PostgreSQL reference: `CREATE TABLE` — SERIAL types section (PG docs §8.1.4)
- MySQL reference: `AUTO_INCREMENT` column attribute (MySQL 8.0 §13.1.20.3)
