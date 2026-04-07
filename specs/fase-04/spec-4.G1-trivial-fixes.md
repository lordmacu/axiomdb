# Spec: 4.G1 — Trivial Bug Fixes

## What to build

Eight small, isolated bug fixes in the evaluator, executor, and parser that each take
1–5 lines to implement. None affect on-disk format or public API. All are confirmed
as missing by code audit and cross-checked with MariaDB/MySQL reference behavior.

---

## Items

### 4.17e — Float division by zero → NULL
**Current**: `1.0 / 0.0` evaluates to `±Infinity` (IEEE 754 passthrough).
**Expected** (MySQL/MariaDB): returns `NULL` (with optional warning in permissive mode).
**File**: `crates/axiomdb-sql/src/eval/ops.rs`, `real_arith()` branch for `Div`.
**Fix**: add `if b == 0.0 { return Ok(Value::Null); }` before float division.

### 4.19e — CONCAT(NULL) must return NULL
**Current**: `CONCAT(NULL, 'a')` returns `'a'` — comment in code says "MySQL behavior"
but that is wrong. MySQL CONCAT returns NULL on any NULL arg; CONCAT_WS skips NULLs.
**File**: `crates/axiomdb-sql/src/eval/functions/string.rs`, `concat()` function.
**Fix**: in the concat accumulator loop, change `Value::Null => {}` to
`Value::Null => return Ok(Value::Null)`.

### 4.19f — SUBSTR negative index must count from end
**Current**: `SUBSTR('hello', -3)` returns `''`. Negative `Int` cast to `usize` wraps
to huge value, clamped to `len`, returns empty.
**Expected** (MySQL/MariaDB): `SUBSTR('hello', -3)` → `'llo'` (start = len − abs(n)).
**File**: `string.rs`, `substr()`.
**Fix**: detect `n < 0`, compute `start = chars.len().saturating_sub(n.unsigned_abs() as usize)`.

### 4.19k — ROUND() must use round-half-up, not banker's rounding
**Current**: Rust `.round()` uses IEEE 754 round-half-to-even (banker's rounding):
`ROUND(2.5)` → 2.
**Expected** (MySQL/MariaDB): `ROUND(2.5)` → 3 (round half away from zero).
**File**: `eval/functions/numeric.rs`, `round()`.
**Fix**: replace `(f * factor).round() / factor` with
`((f * factor + 0.5 * f.signum()).floor()) / factor`.

### 4.25d — CHECK constraints must be evaluated on UPDATE
**Current**: `check_row_constraints()` exists in `executor/insert.rs` and is called
on INSERT but never called in `executor/update.rs`.
**Expected**: any row that violates a CHECK constraint via UPDATE must be rejected.
**File**: `executor/update.rs`, in `execute_update_with_candidates()` before writing rows.
**Fix**: load constraints via `CatalogReader::read_constraints(table_id)` and call
`check_row_constraints()` with the new values before `update_rows_preserve_rid`.

### 4.6d — INSERT column count mismatch must return error
**Current**: `INSERT INTO t (a, b) VALUES (1)` silently pads the missing column with
NULL instead of returning MySQL error 1136.
**Expected**: error 1136 "Column count doesn't match value count at row N".
**File**: `executor/insert.rs`, after the column map is built.
**Fix**: assert `values.len() == col_map.len()` and return
`DbError::ColumnCountMismatch { expected, got }` when they differ.

### 4.5c — SELECT * without FROM must not return NotImplemented
**Current**: `SELECT *` (no FROM clause, wildcard) returns `NotImplemented`.
**Expected** (MySQL/MariaDB): returns a single empty row (zero columns, one row) —
consistent with how MySQL handles `SELECT * FROM dual` with no actual table.
**File**: `executor/select.rs`, around line 783 where wildcard without FROM is rejected.
**Fix**: when `from` is None and the projection is a pure wildcard, return an empty
result set (0 columns, 0 rows) rather than an error. `SELECT 1` / `SELECT expr`
without FROM already works.

### 4.5f — AUTO_INCREMENT with explicit 0 must assign next sequence value
**Current**: `INSERT INTO t (id, name) VALUES (0, 'Alice')` stores literal `0`.
**Expected** (MySQL/MariaDB): value `0` on an AUTO_INCREMENT column is treated the
same as NULL — the next sequence value is assigned.
**File**: `executor/insert.rs`, in the AUTO_INCREMENT resolution path.
**Fix**: in `resolve_auto_increment()`, after checking for NULL, also check for
`Value::Int(0)` or `Value::BigInt(0)` and replace with the next sequence value.

---

## Inputs / Outputs

| Item | Input | Expected output |
|---|---|---|
| 4.17e | `SELECT 1.0 / 0` | NULL |
| 4.19e | `SELECT CONCAT(NULL, 'a')` | NULL |
| 4.19f | `SELECT SUBSTR('hello', -3)` | `'llo'` |
| 4.19k | `SELECT ROUND(2.5)` | 3 |
| 4.25d | `UPDATE t SET price = -1` where CHECK(price > 0) | CheckViolation error |
| 4.6d | `INSERT INTO t (a,b) VALUES (1)` | error 1136 |
| 4.5c | `SELECT *` | empty result (0 cols, 0 rows) |
| 4.5f | `INSERT INTO t (id) VALUES (0)` | id assigned = next AUTO_INC value |

## Errors

- 4.6d → new `DbError::ColumnCountMismatch { expected: usize, got: usize }` variant
  (MySQL error 1136, SQLSTATE `21S01`)
- 4.25d → existing `DbError::CheckViolation` already defined

## Acceptance criteria

- [ ] `SELECT 1.0 / 0` → NULL (not Infinity, not error)
- [ ] `SELECT CONCAT(NULL, 'a')` → NULL
- [ ] `SELECT CONCAT_WS(',', NULL, 'a')` → still `'a'` (CONCAT_WS skips NULLs)
- [ ] `SELECT SUBSTR('hello', -3)` → `'llo'`
- [ ] `SELECT SUBSTR('hello', -1)` → `'o'`
- [ ] `SELECT SUBSTR('hello', -10)` → `'hello'` (clamps to start)
- [ ] `SELECT ROUND(2.5)` → 3
- [ ] `SELECT ROUND(3.5)` → 4
- [ ] `SELECT ROUND(-2.5)` → -3 (round away from zero)
- [ ] `SELECT ROUND(2.45, 1)` → 2.5
- [ ] CHECK violated on UPDATE → CheckViolation error
- [ ] CHECK compliant UPDATE → succeeds
- [ ] `INSERT INTO t (a,b) VALUES (1)` → error 1136
- [ ] `SELECT *` (no FROM) → empty result, no error
- [ ] `INSERT INTO t (id) VALUES (0)` on AUTO_INC table → row with next id

## Out of scope

- Division by zero for INTEGER types is already an error (not changed)
- CONCAT_WS behavior (skips NULLs by design, not changed)
- ROUND for DECIMAL type (separate precision model, not touched here)
- CHAR(n) padding (in 4.G7)
- CHECK in DDL ALTER (in 4.G9)

## Dependencies

- Existing `check_row_constraints()` in `insert.rs` (reused for UPDATE)
- Existing `DbError` enum (new variant added for 4.6d)
- No on-disk format changes
- No catalog schema changes
