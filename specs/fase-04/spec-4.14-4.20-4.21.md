# Spec: 4.14 + 4.20 + 4.21 — LAST_INSERT_ID, SHOW/DESCRIBE, TRUNCATE

## Context

Subfase 4.3c marked as complete but only implemented the **parser** — the executor
never generated AUTO_INCREMENT IDs. This spec closes that gap as part of 4.14.

---

## 4.14 — LAST_INSERT_ID() / lastval() / AUTO_INCREMENT execution

### What to build

1. **AUTO_INCREMENT column generation** — when a column marked `AUTO_INCREMENT`
   (or `SERIAL`) receives `NULL` or is omitted in an INSERT, the executor assigns
   the next value from a per-table sequence counter.

2. **`LAST_INSERT_ID()` / `lastval()`** — SQL functions that return the last
   auto-generated ID produced by the current session.

3. **`ColumnDef.auto_increment: bool`** — new field in the catalog schema that
   records whether a column is an AUTO_INCREMENT column. Stored on disk so the
   executor can detect AUTO_INCREMENT columns at INSERT time without the original
   CREATE TABLE AST.

### Inputs / Outputs

- `INSERT INTO t (name) VALUES ('Alice')` where `id` is `INT AUTO_INCREMENT`:
  - Input: omitted `id` column
  - Output: `QueryResult::Affected { count: 1, last_insert_id: Some(1) }`
  - Effect: row has `id = 1`

- `SELECT LAST_INSERT_ID()` after the above:
  - Output: `QueryResult::Rows { rows: [[Value::BigInt(1)]] }`

- `INSERT INTO t VALUES (NULL, 'Bob')` where first column is AUTO_INCREMENT:
  - Output: `last_insert_id = Some(2)`, row has `id = 2`

- `INSERT INTO t VALUES (99, 'Carol')` (explicit ID given):
  - Output: `last_insert_id = None`, row has `id = 99`
  - Sequence advances to MAX(99, current_next) on next auto-insert

### Sequence semantics

- Sequence starts at 1 for a new table.
- On first auto-insert into a table: initialize from `MAX(auto_col) + 1`
  over existing rows (handles restarts gracefully; avoids duplicate keys).
- After initialization, increment by 1 per row in the current INSERT.
- For multi-row INSERT (`INSERT INTO t VALUES (...), (...)`): sequence is
  contiguous. `LAST_INSERT_ID()` returns the FIRST generated ID (MySQL semantics).
- Explicit non-NULL value → sequence not advanced, `last_insert_id = None`.
- Sequence state lives in a **thread-local** `HashMap<TableId, u64>`. Initialized
  lazily. This is sufficient for single-connection embedded mode (Phase 4.14).
  Phase 5+ will migrate to per-session state in `SessionContext`.

### `LAST_INSERT_ID()` / `lastval()` semantics

- Returns the last auto-generated ID for the current session/thread.
- Returns `0` (as `BigInt(0)`) if no auto-increment INSERT has occurred.
- MySQL aliases: `last_insert_id()` (no args). PostgreSQL alias: `lastval()`.
- Both functions return `Value::BigInt(id)`.

### ColumnDef changes

Add `auto_increment: bool` as the last field. Serialization: append 1 byte
(`0x01` = true, `0x00` = false) to the existing format. Backward-compatible:
rows written before this change default to `false` on read (check available bytes).

### Acceptance criteria

- [ ] `CREATE TABLE t (id INT AUTO_INCREMENT, name TEXT)` stores `auto_increment=true` for `id` in catalog
- [ ] `INSERT INTO t VALUES (NULL, 'Alice')` generates `id = 1`, returns `last_insert_id = Some(1)`
- [ ] `INSERT INTO t (name) VALUES ('Bob')` generates `id = 2` (column omitted → treated as NULL)
- [ ] Explicit `INSERT INTO t VALUES (10, 'Carol')` uses `id = 10`, does NOT advance sequence
- [ ] Multi-row INSERT: `INSERT INTO t VALUES (NULL, 'A'), (NULL, 'B')` → ids 3 and 4, `last_insert_id = Some(3)` (first)
- [ ] `SELECT LAST_INSERT_ID()` returns last generated ID
- [ ] `SELECT lastval()` returns same result
- [ ] After process restart (fresh thread-local): first auto-insert after existing rows re-scans and correctly continues from MAX+1
- [ ] `SERIAL` columns work identically to `AUTO_INCREMENT`

---

## 4.20 — SHOW TABLES / SHOW COLUMNS / DESCRIBE

### What to build

Execute the three introspection statements already parsed by the parser:

1. `SHOW TABLES [FROM schema]` — lists all tables in the schema.
2. `SHOW COLUMNS FROM table` — lists columns with metadata.
3. `DESCRIBE table` / `DESC table` — alias for SHOW COLUMNS.

### Output format

**`SHOW TABLES [FROM schema]`:**
```
Tables_in_<schema>
users
orders
```
One column: `Tables_in_<schema>` (MySQL-compatible). `<schema>` is `public` if
not specified. Returns `QueryResult::Rows`.

**`SHOW COLUMNS FROM table` / `DESCRIBE table`:**
```
Field      | Type    | Null | Key | Default | Extra
-----------+---------+------+-----+---------+------
id         | INT     | NO   | PRI | NULL    | auto_increment
name       | TEXT    | YES  |     | NULL    |
```
Six columns (MySQL-compatible):
- `Field`: column name
- `Type`: SQL type as string (`INT`, `TEXT`, `BIGINT`, `BOOL`, etc.)
- `Null`: `YES` or `NO`
- `Key`: `PRI` if the column is the primary key, `UNI` if unique, empty otherwise (stub — always empty for Phase 4.20)
- `Default`: `NULL` (stub — Phase 4.20 returns NULL for all)
- `Extra`: `auto_increment` if the column has AUTO_INCREMENT, empty otherwise

### Inputs / Outputs

- Input: `SHOW TABLES`
- Output: `QueryResult::Rows { columns: [ColumnMeta("Tables_in_public")], rows: [...] }`

- Input: `SHOW TABLES FROM myschema`
- Output: same, column name `Tables_in_myschema`

- Input: `SHOW COLUMNS FROM users` or `DESCRIBE users`
- Output: 6-column rows, one per column

### Errors

- `SHOW TABLES FROM unknown_schema` → empty result set (no error; MySQL behavior)
- `SHOW COLUMNS FROM unknown_table` → `DbError::TableNotFound`

### Acceptance criteria

- [ ] `SHOW TABLES` returns all user tables in `public` schema
- [ ] `SHOW TABLES FROM public` returns same result
- [ ] `DESCRIBE users` returns one row per column with correct Field/Type/Null
- [ ] `SHOW COLUMNS FROM users` is identical to `DESCRIBE users`
- [ ] `Type` column uses correct SQL type name (`INT`, `TEXT`, `BIGINT`, `BOOL`, `REAL`, `DOUBLE`, `DECIMAL`, `TIMESTAMP`, `DATE`, `BYTES`, `UUID`)
- [ ] `Null` is `YES` for nullable, `NO` for NOT NULL
- [ ] `Extra` is `auto_increment` for AUTO_INCREMENT columns (depends on 4.14)
- [ ] `DESCRIBE nonexistent` → `TableNotFound` error

---

## 4.21 — TRUNCATE TABLE

### What to build

`TRUNCATE TABLE t` removes all rows from `t` atomically, similar to
`DELETE FROM t` (without WHERE) but:
- Faster for large tables (no per-row WHERE evaluation).
- Resets the AUTO_INCREMENT sequence counter to 1 (MySQL semantics).
- Does NOT fire row-level triggers (not relevant for Phase 4.21, no triggers yet).

### Implementation scope (Phase 4.21)

Implement as **DELETE-all** using the same `TableEngine::delete_row` path:
collect all `RecordId`s, then delete in batch. This is O(n) rows. The WAL still
gets per-row entries. True single-WAL-entry truncation (heap reset) is deferred
to a storage optimization phase.

The key difference from `DELETE FROM t`: resets the AUTO_INCREMENT sequence counter.

### Inputs / Outputs

- Input: `TRUNCATE TABLE users`
- Output: `QueryResult::Affected { count: 0, last_insert_id: None }`
  (MySQL returns 0 rows affected for TRUNCATE, not the actual count)
- Effect: all rows deleted, auto_increment counter reset to 1

### Errors

- `TRUNCATE TABLE nonexistent` → `DbError::TableNotFound`
- Must be inside an active transaction (autocommit wraps it automatically)

### Acceptance criteria

- [ ] `TRUNCATE TABLE t` deletes all rows
- [ ] After TRUNCATE, `SELECT COUNT(*) FROM t` returns 0
- [ ] After TRUNCATE, next auto-insert starts from 1 again
- [ ] Returns `QueryResult::Affected { count: 0, ... }` (not the row count)
- [ ] `TRUNCATE TABLE nonexistent` → `TableNotFound`
- [ ] TRUNCATE on empty table is a no-op (returns Empty/Affected{0})

---

## Out of scope

- Multi-table TRUNCATE (`TRUNCATE TABLE a, b`) — Phase N
- TRUNCATE with CASCADE (FK dependency handling) — Phase N
- Persistent sequences across process restarts using a catalog table — Phase 5+
- `ALTER SEQUENCE` — Phase N
- Key column detection for `SHOW COLUMNS` (`Key` column fully populated) — Phase 5+
- Default value display in `SHOW COLUMNS` — Phase 5+

## ⚠️ DEFERRED

- Persistent AUTO_INCREMENT sequence (survives restart) → Phase 5+ (SessionContext)
- `Key` and `Default` columns in SHOW COLUMNS are stubs → Phase 5+
- True O(1) TRUNCATE (page reset + single WAL entry) → Phase N (storage optimization)

## Dependencies

- 4.14 requires `ColumnDef.auto_increment` field (catalog format change)
- 4.20 `Extra` column uses `ColumnDef.auto_increment` — depends on 4.14
- 4.21 uses the AUTO_INCREMENT thread-local reset — depends on 4.14
- All three use existing `CatalogReader` API — no new catalog methods needed
