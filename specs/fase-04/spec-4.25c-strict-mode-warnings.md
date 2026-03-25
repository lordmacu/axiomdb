# Spec: 4.25c — Strict Mode + Warnings System

## These files were reviewed before writing this spec

- `db.md`
- `docs/progreso.md`
- `specs/fase-04/spec-4.18b.md`
- `specs/fase-04/spec-4.25.md`
- `crates/axiomdb-sql/src/session.rs`
- `crates/axiomdb-sql/src/table.rs`
- `crates/axiomdb-sql/src/executor.rs`
- `crates/axiomdb-network/src/mysql/session.rs`
- `crates/axiomdb-network/src/mysql/handler.rs`
- `crates/axiomdb-network/src/mysql/database.rs`
- `crates/axiomdb-sql/tests/integration_table.rs`
- `crates/axiomdb-sql/tests/integration_executor.rs`
- `tools/wire-test.py`
- `research/mariadb-server/sql/sys_vars.cc`
- `research/mariadb-server/mysql-test/suite/innodb/r/alter_not_null.result`
- `research/mariadb-server/mysql-test/suite/innodb/r/innodb-autoinc.result`
- `research/sqlite/src/insert.c`

## What to build (not how)

Add a session-scoped strict assignment mode for AxiomDB DML.

`strict_mode` is **ON by default**. When it is ON, `INSERT` and `UPDATE`
assignment coercion keeps the current behavior: if a value cannot be coerced
under `CoercionMode::Strict`, the statement errors.

When `strict_mode` is OFF, `INSERT` and `UPDATE` column assignment must:

1. Try the existing Phase 4.18b strict coercion rules first.
2. If strict coercion fails, try the existing Phase 4.18b permissive coercion.
3. If permissive coercion succeeds, store the coerced value and append a SQL
   warning to the session.
4. If permissive coercion also fails, return the coercion error and do not turn
   it into a warning.

Warnings must reuse the existing warning pipeline that already feeds:

- `SessionContext.warnings`
- MySQL OK packet `warning_count`
- `SHOW WARNINGS`

This subphase only changes **column assignment on `INSERT` and `UPDATE`**.
Expression evaluation, `CAST`, arithmetic coercion, DDL row rewrites, and other
internal storage rewrites do not change behavior in 4.25c.

## Inputs / Outputs

### Session inputs

Supported ways to control the mode:

- `SET strict_mode = ON`
- `SET strict_mode = OFF`
- `SET strict_mode = 1`
- `SET strict_mode = 0`
- `SET strict_mode = TRUE`
- `SET strict_mode = FALSE`
- `SET strict_mode = DEFAULT`
- `SET sql_mode = 'STRICT_TRANS_TABLES'`
- `SET sql_mode = 'STRICT_ALL_TABLES'`
- `SET sql_mode = ''`
- `SET sql_mode = 'ANSI_QUOTES,STRICT_TRANS_TABLES'`
- `SET sql_mode = DEFAULT`

Rules:

- `strict_mode` is the AxiomDB-native knob described in `db.md`.
- `sql_mode` is the MySQL-compatibility alias.
- In AxiomDB, only the presence or absence of `STRICT_TRANS_TABLES` or
  `STRICT_ALL_TABLES` affects strict assignment behavior in this subphase.
- Unknown `sql_mode` tokens are preserved as text but have no execution effect
  in 4.25c.

### DML inputs

- `INSERT ... VALUES (...)`
- `INSERT ... SELECT ...`
- `UPDATE ... SET ...`

using `execute_with_ctx(...)` / real session execution paths.

### Outputs

- Existing `QueryResult` behavior is unchanged.
- Existing storage layout is unchanged.
- On successful permissive fallback, the coerced value is stored and one warning
  is appended per affected column.
- The warning is visible through existing `SHOW WARNINGS`.

### Warning format

Permissive assignment warnings use:

- `level = "Warning"`
- `code = 1265`
- `message = "Data truncated for column '{column}' at row {row_num}"`

where:

- `{column}` is the target column name from the catalog
- `{row_num}` is a 1-based row number within the statement-local assignment loop

### Errors

- Invalid `strict_mode` value: `DbError::InvalidValue`
- Invalid `sql_mode` value type for this subphase (for example, non-string /
  non-default in the ctx execution path): `DbError::InvalidValue`
- Strict-mode coercion failure when strict mode is ON: existing coercion error
- Permissive-mode coercion failure when strict mode is OFF: existing coercion
  error

## Use cases

1. Default strict mode rejects lossy assignment

   `INSERT INTO t VALUES ('42abc')` into an `INT` column returns
   `DbError::InvalidCoercion` and stores no row.

2. Strict mode OFF stores permissive numeric prefix and warns

   `SET strict_mode = OFF; INSERT INTO t VALUES ('42abc')` into an `INT`
   column stores `42` and appends:

   `Warning 1265: Data truncated for column 'col' at row 1`

3. Strict mode OFF stores `0` for non-numeric text and warns

   `SET strict_mode = OFF; INSERT INTO t VALUES ('abc')` into an `INT`
   column stores `0` and appends the same `1265` warning format.

4. Multi-row statement reports statement-local row numbers

   `INSERT INTO t VALUES ('42abc'), ('7x')` into an `INT` column appends two
   warnings with `row 1` and `row 2`.

5. UPDATE uses the same assignment policy

   `SET strict_mode = OFF; UPDATE t SET age = '9x' WHERE ...` stores `9` and
   appends `1265` warnings for the matched rows that needed permissive fallback.

6. Non-assignment coercion stays unchanged

   `SET strict_mode = OFF; SELECT CAST('abc' AS INT)` still returns
   `DbError::InvalidCoercion` in 4.25c.

## Acceptance criteria

- [ ] `SessionContext::new()` starts with `strict_mode = true`.
- [ ] Wire sessions start with `@@strict_mode = 'ON'`.
- [ ] Wire sessions start with `@@sql_mode` containing `STRICT_TRANS_TABLES`.
- [ ] `SET strict_mode = OFF` disables strict assignment for subsequent
      `INSERT` and `UPDATE` in the same session.
- [ ] `SET strict_mode = DEFAULT` resets the session to strict mode ON.
- [ ] `SET sql_mode = ''` disables strict assignment in the same way as
      `SET strict_mode = OFF`.
- [ ] `SET sql_mode = DEFAULT` resets strict assignment to ON.
- [ ] `SET sql_mode = 'STRICT_TRANS_TABLES'` and
      `SET sql_mode = 'STRICT_ALL_TABLES'` both enable strict assignment.
- [ ] `SET sql_mode = 'ANSI_QUOTES,STRICT_TRANS_TABLES'` keeps the full string
      visible through `@@sql_mode` while still enabling strict assignment.
- [ ] With strict mode ON, current `INSERT` / `UPDATE` coercion behavior is
      unchanged.
- [ ] With strict mode OFF, assignment fallback uses the existing Phase 4.18b
      `CoercionMode::Permissive` rules only for `INSERT` / `UPDATE`.
- [ ] If strict coercion fails and permissive coercion succeeds, the row is
      stored and one `1265` warning is appended for that column.
- [ ] If both strict and permissive coercion fail, the statement returns the
      coercion error and does not append a warning for that column.
- [ ] Warning order matches column-assignment generation order.
- [ ] Warning row numbering is 1-based and statement-local for multi-row
      `INSERT` and multi-row `UPDATE`.
- [ ] `SHOW WARNINGS` returns the warnings from the previous statement and the
      next non-`SHOW WARNINGS` statement clears them as it does today.
- [ ] `SELECT @@strict_mode` and `SHOW VARIABLES LIKE 'strict_mode'` both
      reflect the current session mode.
- [ ] `SELECT @@sql_mode` and `SHOW VARIABLES LIKE 'sql_mode'` both reflect the
      current session mode string.
- [ ] `CAST`, arithmetic/comparison coercion, CHECK evaluation, FK maintenance,
      and DDL rewrite paths are unchanged by 4.25c.

## ⚠️ DEFERRED

- MySQL-style numeric clamping plus warning `1264` (`Out of range value ...`)
  is not added in 4.25c. Phase 4.18b currently defines overflow as an error in
  both strict and permissive coercion, and 4.25c keeps that contract.
- Relaxed date / timestamp parsing and other `sql_mode`-dependent behaviors
  outside assignment coercion remain deferred to later subphases.
- `SHOW VARIABLES LIKE` wildcard semantics are not expanded in 4.25c; this
  subphase only requires exact-name visibility for `strict_mode` and `sql_mode`.

## Out of scope

- Changing `coerce(...)` to return warnings or session-aware metadata
- New coercion rules beyond what Phase 4.18b already defines
- Changing `CAST`, expression evaluation, or operator widening
- Reworking internal DDL rewrite / FK maintenance paths to use session warnings
- Implementing full MySQL `sql_mode` feature parity
- Multi-assignment `SET a=..., b=...` parsing changes beyond what the current
  session machinery already supports

## Dependencies

- `db.md`
- `specs/fase-04/spec-4.18b.md`
- `specs/fase-04/spec-4.25.md`
- `crates/axiomdb-sql/src/session.rs`
- `crates/axiomdb-sql/src/table.rs`
- `crates/axiomdb-sql/src/executor.rs`
- `crates/axiomdb-network/src/mysql/session.rs`
- `crates/axiomdb-network/src/mysql/handler.rs`
- `crates/axiomdb-network/src/mysql/database.rs`
