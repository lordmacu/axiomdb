# Spec: 4.G5 — DML Extensions (MySQL Compatibility)

## What to build (not how)

Six MySQL-compatible DML/DDL syntax extensions that are common in real-world
applications and ORM-generated queries, but currently cause parse errors or
missing behavior in AxiomDB.

---

## G5.1 — CALL / DO → Noop

### Behavior
- `CALL proc(args)` parses successfully and returns an empty result with a
  "not implemented" notice (does not error).
- `DO expr` parses successfully and returns an empty result (evaluates the
  expression for side effects; since we have no side-effecting expressions,
  this is a true noop).

### Inputs / Outputs
- Input: any `CALL ident(...)` or `DO expr` SQL string
- Output: `QueryResult::Empty` (0 rows, 0 affected)
- Errors: none (these are noops, not errors)

### Use cases
1. `CALL stored_proc(1, 2)` — app calling a procedure that doesn't exist → silent success
2. `DO SLEEP(0)` — common in MySQL test harnesses → silent success
3. `CALL schema.proc(arg1, arg2, arg3)` — qualified name → same behavior

### Acceptance criteria
- [ ] `CALL proc()` parses as `Stmt::Call` and executes without error
- [ ] `CALL schema.proc(1, 'x')` parses and executes without error
- [ ] `DO 1+1` parses as `Stmt::Do` and executes without error
- [ ] Both return `QueryResult::Empty`

### Out of scope
- Actual stored procedure execution (Phase 17+)
- Error if procedure does not exist

---

## G5.2 — SELECT FOR UPDATE / LOCK IN SHARE MODE

### Behavior
- `SELECT ... FOR UPDATE` and `SELECT ... LOCK IN SHARE MODE` parse without
  error and execute as a normal SELECT (lock semantics deferred to Phase 13.7).
- The lock mode is stored in `SelectStmt` as `lock_mode: Option<LockMode>` but
  ignored by the executor.

### Inputs / Outputs
- Input: any SELECT with trailing `FOR UPDATE` or `LOCK IN SHARE MODE`
- Output: normal SELECT result (identical to the same query without lock clause)
- Errors: none (lock mode is silently accepted)

### Use cases
1. `SELECT * FROM orders WHERE id = 1 FOR UPDATE` — JPA/Hibernate pessimistic lock
2. `SELECT * FROM inventory WHERE sku = ? LOCK IN SHARE MODE` — GORM shared lock
3. `SELECT id, amount FROM accounts WHERE user_id = 1 FOR UPDATE` — transaction pattern

### Acceptance criteria
- [ ] `SELECT ... FOR UPDATE` parses without error
- [ ] `SELECT ... LOCK IN SHARE MODE` parses without error
- [ ] Both return the same rows as without the clause
- [ ] `SelectStmt.lock_mode` carries `Some(LockMode::ForUpdate)` / `Some(LockMode::ShareMode)`

### Out of scope
- Actual row-level locking (Phase 13.7)
- Deadlock detection

---

## G5.3 — INSERT IGNORE

### Behavior
- `INSERT IGNORE INTO t VALUES (...)` silences constraint violations that
  would normally abort the statement:
  - `DuplicateKey` (UNIQUE / PRIMARY KEY conflict) → skip row, increment
    warning count, continue
  - `NotNullViolation` → skip row
  - `FkViolation` → skip row
- Non-constraint errors (e.g., type mismatch, disk full) still abort.
- Returns `QueryResult::Affected { count }` where `count` = rows actually inserted
  (skipped rows are NOT counted).

### Inputs / Outputs
- Input: `INSERT IGNORE INTO t (cols) VALUES (...), (...)`
- Output: `QueryResult::Affected { count: N }` where N = rows inserted (not skipped)
- Errors: only non-constraint errors propagate

### Use cases
1. Single row ignore: `INSERT IGNORE INTO users (email) VALUES ('dup@x.com')` — no error if email exists
2. Multi-row batch: `INSERT IGNORE INTO t VALUES (1), (2), (1)` → inserts 2, skips 1 dup
3. With FK: `INSERT IGNORE INTO orders (user_id) VALUES (999)` → skip if user 999 missing

### Acceptance criteria
- [ ] `INSERT IGNORE` parses with `InsertStmt { ignore: true, .. }`
- [ ] Duplicate PK → row skipped, no error, `affected_rows` does not count it
- [ ] Duplicate UNIQUE key → same behavior
- [ ] NOT NULL violation → row skipped, no error
- [ ] Non-ignored errors (type error) still propagate
- [ ] Multi-row: some rows inserted, some skipped → correct affected count

### Out of scope
- `INSERT IGNORE ... SELECT ...` (deferred — requires SELECT path wiring)
- Warning messages (Phase 5.9+)

---

## G5.4 — DELETE / UPDATE with ORDER BY + LIMIT

### Behavior
- `DELETE FROM t [WHERE ...] ORDER BY col [ASC|DESC] LIMIT N` deletes at most
  N rows matching the WHERE, in the specified order.
- `UPDATE t SET col=val [WHERE ...] ORDER BY col [ASC|DESC] LIMIT N` updates
  at most N rows matching the WHERE, in the specified order.
- ORDER BY and LIMIT are optional independently; either may appear alone.
- This is the MySQL syntax used for safe batch processing ("delete the 100
  oldest rows", "update the 10 lowest-priority records").

### Inputs / Outputs
- Input: DELETE or UPDATE with optional `ORDER BY` + `LIMIT`
- Output: `QueryResult::Affected { count }` — rows actually deleted/updated
- Errors: same as regular DELETE/UPDATE (constraint violations, etc.)

### Use cases
1. `DELETE FROM logs ORDER BY created_at ASC LIMIT 1000` — rolling deletion
2. `UPDATE jobs SET status='processing' WHERE status='pending' ORDER BY priority DESC LIMIT 10`
3. `DELETE FROM t WHERE active = 0 LIMIT 50` — LIMIT without ORDER BY

### Acceptance criteria
- [ ] `DeleteStmt` has `order_by: Vec<OrderByExpr>` and `limit: Option<Expr>` fields
- [ ] `UpdateStmt` has `order_by: Vec<OrderByExpr>` and `limit: Option<Expr>` fields
- [ ] `DELETE ... ORDER BY col LIMIT N` parses correctly
- [ ] `UPDATE ... ORDER BY col LIMIT N` parses correctly
- [ ] DELETE with ORDER BY + LIMIT deletes exactly N rows in correct order
- [ ] UPDATE with ORDER BY + LIMIT updates exactly N rows in correct order
- [ ] LIMIT without ORDER BY: limits count but order is implementation-defined
- [ ] ORDER BY without LIMIT: sorts candidates but no row cap

### Out of scope
- ORDER BY with multi-column expressions (only single column needed for compat)
- Subqueries in LIMIT expr

---

## G5.5 — CREATE TABLE ... LIKE

### Behavior
- `CREATE TABLE new_table LIKE source_table` creates a new empty table with
  the same column definitions, constraints, and indexes as `source_table`.
- `IF NOT EXISTS` modifier is supported: `CREATE TABLE IF NOT EXISTS t2 LIKE t1`
- The new table has NO rows — only schema is copied.
- Sequences (AUTO_INCREMENT) start fresh from 1 in the new table.
- The source table must exist; if not, return `TableNotFound`.

### Inputs / Outputs
- Input: `CREATE TABLE [IF NOT EXISTS] t2 LIKE t1`
- Output: `QueryResult::Empty` (DDL)
- Errors: `TableNotFound` if source does not exist; `TableAlreadyExists` if
  target exists and `IF NOT EXISTS` is not specified

### Use cases
1. `CREATE TABLE orders_backup LIKE orders` — staging copy for batch migration
2. `CREATE TABLE IF NOT EXISTS tmp_users LIKE users` — idempotent temp table setup
3. Preserves: columns, NOT NULL, DEFAULT, UNIQUE, PK, FK, CHECK, inline indexes

### Acceptance criteria
- [ ] `CREATE TABLE t2 LIKE t1` parses as `Stmt::CreateTableLike { .. }`
- [ ] New table has same columns (names, types, constraints)
- [ ] New table has same indexes (including non-unique)
- [ ] New table starts empty (0 rows)
- [ ] AUTO_INCREMENT counter starts at 1
- [ ] Error if source table does not exist
- [ ] `IF NOT EXISTS` works correctly
- [ ] Source table data is NOT copied

### Out of scope
- Cross-database LIKE (`db1.t1`)
- Copying table options (ENGINE, CHARSET) — discarded as always

---

## G5.6 — CREATE TABLE ... SELECT (CTAS)

### Behavior
- `CREATE TABLE new_table [col_list] AS SELECT ...` creates a new table whose
  schema is derived from the SELECT result columns, then inserts all selected
  rows into it.
- Column names are taken from SELECT aliases or expressions names.
- Column types are inferred from the Value types returned by the SELECT.
- No indexes are created (plain heap table).
- `IF NOT EXISTS` is NOT supported for CTAS (MySQL restriction).

### Inputs / Outputs
- Input: `CREATE TABLE t AS SELECT ...`
- Output: `QueryResult::Affected { count }` — number of rows inserted
- Errors: `TableAlreadyExists` if target exists; any error from the inner SELECT

### Type inference rules
| Value type | Column type |
|---|---|
| `Value::Int` | `INT` |
| `Value::BigInt` | `BIGINT` |
| `Value::Real` | `REAL` |
| `Value::Text` | `TEXT` |
| `Value::Bool` | `BOOLEAN` |
| `Value::Date` | `DATE` |
| `Value::Timestamp` | `DATETIME` |
| `Value::Null` | `TEXT` (fallback) |

### Use cases
1. `CREATE TABLE summary AS SELECT dept, COUNT(*) AS cnt FROM employees GROUP BY dept`
2. `CREATE TABLE recent AS SELECT * FROM logs WHERE created_at > '2026-01-01'`
3. `CREATE TABLE t2 AS SELECT id, name FROM t1` — simple copy with subset of columns

### Acceptance criteria
- [ ] `CREATE TABLE t AS SELECT ...` parses as `Stmt::CreateTableAsSelect { .. }`
- [ ] New table is created with columns derived from SELECT result
- [ ] All rows from SELECT are inserted into new table
- [ ] `QueryResult::Affected { count }` = number of rows inserted
- [ ] `TableAlreadyExists` error if target table exists
- [ ] Column names from SELECT aliases used when present
- [ ] Works with aggregates, JOINs, WHERE, LIMIT in the inner SELECT

### Out of scope
- `CREATE TABLE t (col_defs) SELECT ...` (explicit column list form)
- Indexes on the new table
- `IF NOT EXISTS` for CTAS

---

## Dependencies

- G5.1, G5.2, G5.4 depend only on parser + AST changes
- G5.3 depends on executor insert path (`executor/insert.rs`, `insert_heap.rs`, `insert_clustered.rs`)
- G5.5 depends on catalog reader (read existing TableDef) + executor DDL path
- G5.6 depends on executor SELECT path + executor DDL path; must run after G5.5 is understood

## Out of scope (all of G5)

- `4.4g` Multi-table DELETE/UPDATE JOIN syntax (deferred — separate gap item)
- Actual stored procedure execution (CALL)
- Row-level locking (FOR UPDATE / LOCK IN SHARE MODE)
