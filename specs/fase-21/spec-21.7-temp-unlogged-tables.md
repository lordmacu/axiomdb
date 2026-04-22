# Spec: 21.7 — TEMP and UNLOGGED tables

Phase: 21 — Advanced SQL
Task: 21.7 TEMP and UNLOGGED tables
Status: completed

## Context

Phase 21 still has one major table-lifecycle gap: session-local temporary
tables and PostgreSQL-style `UNLOGGED` tables. The current engine only knows
permanent catalog tables plus the `immutable` table flag; every table lives in
the shared catalog, every connection resolves through `current_schema()`, and
startup always opens the same catalog namespace after WAL recovery.

This task extends that model without inventing a second executor or storage
engine. TEMP tables should reuse normal catalog/DML/DDL/index paths but remain
session-scoped and auto-cleaned. UNLOGGED tables should reuse normal runtime
write paths but lose contents after a dirty reopen.

## Goal

Implement first-class `CREATE TEMP[TORARY] TABLE` and `CREATE UNLOGGED TABLE`
support with defensible visibility, cleanup, and restart semantics.

## Non-goals

- `ON COMMIT DELETE ROWS` / `ON COMMIT DROP` / `ON COMMIT PRESERVE ROWS`.
- `DROP TEMPORARY TABLE` syntax; plain `DROP TABLE` is sufficient for 21.7.
- TEMP or UNLOGGED views, sequences, indexes, or databases.
- Qualified TEMP table names (`db.schema.t` or `schema.t`) on CREATE.
- WAL-bypass runtime writes for UNLOGGED tables; 21.7 may still use normal WAL
  write/rollback machinery during a live process.
- Foreign keys involving TEMP tables.
- Foreign keys involving UNLOGGED tables.
- Cross-session visibility of TEMP tables through `information_schema` or
  `SHOW TABLE STATUS`.

## Behavior

### Public API

```rust
pub enum TablePersistence {
    Permanent,
    Temporary,
    Unlogged,
}

pub struct CreateTableStmt {
    pub if_not_exists: bool,
    pub table: TableRef,
    pub columns: Vec<ColumnDef>,
    pub table_constraints: Vec<TableConstraint>,
    pub immutable: bool,
    pub persistence: TablePersistence,
}

pub struct CreateTableLikeStmt {
    pub if_not_exists: bool,
    pub new_table: TableRef,
    pub source_table: TableRef,
    pub persistence: TablePersistence,
}

pub struct CreateTableAsSelectStmt {
    pub new_table: TableRef,
    pub select: SelectStmt,
    pub persistence: TablePersistence,
}

pub struct TableDef {
    pub id: TableId,
    pub root_page_id: u64,
    pub storage_layout: TableStorageLayout,
    pub schema_name: String,
    pub table_name: String,
    pub schema_version: u64,
    pub immutable: bool,
    pub persistence: TablePersistence,
}

pub struct SessionContext {
    // existing fields...
    pub temp_schema: Option<String>,
}
```

Exact helper names may vary, but the system must expose:

- one AST/catalog enum that distinguishes permanent vs temporary vs unlogged;
- one session field that records the current connection's hidden temp schema;
- one startup/shutdown metadata flag used to decide whether UNLOGGED tables
  must be truncated after reopen.

### Semantics

#### TEMP tables

- Accepted grammar:

```sql
CREATE TEMP TABLE t (id INT);
CREATE TEMPORARY TABLE t (id INT PRIMARY KEY, name TEXT);
CREATE TEMP TABLE t AS SELECT 1 AS id;
CREATE TEMP TABLE t LIKE base_table;
```

- `TEMP` and `TEMPORARY` are synonyms.
- TEMP CREATE requires an unqualified table name. Supplying an explicit schema
  or database returns `DbError::InvalidValue`.
- On the first TEMP CREATE in a session, the engine allocates a hidden schema
  name such as `__axiom_temp_<token>` and stores it in `SessionContext`.
- The session search path becomes `[temp_schema, "public"]`, so unqualified
  references resolve TEMP tables first and then fall back to `public`.
- TEMP tables are stored in the regular catalog with
  `TablePersistence::Temporary` and `schema_name = temp_schema`.
- TEMP tables reuse the normal CREATE TABLE / DML / index / CHECK / exclusion
  flows once resolved.
- TEMP tables may shadow permanent tables with the same name for unqualified
  references. Qualified `public.t` continues to refer to the permanent table.
- TEMP tables are dropped automatically on:
  - connection disconnect;
  - `COM_RESET_CONNECTION`;
  - `COM_CHANGE_USER`.
- Other sessions must not resolve or list a TEMP table they do not own.

#### UNLOGGED tables

- Accepted grammar:

```sql
CREATE UNLOGGED TABLE audit_buffer (id INT, payload TEXT);
CREATE UNLOGGED TABLE audit_buffer AS SELECT * FROM source_rows;
CREATE UNLOGGED TABLE audit_buffer LIKE source_rows;
```

- UNLOGGED tables are regular catalog objects in the requested schema with
  `TablePersistence::Unlogged`.
- During a live process, UNLOGGED tables use the existing runtime DML/DDL
  paths, including rollback and index maintenance.
- The database meta page stores a best-effort `clean_shutdown` bit.
  - On successful open, the bit is set dirty (`0`) before serving queries.
  - On graceful close/drop of `SharedDatabase`, the engine attempts to flush and
    set the bit clean (`1`).
- If the database is reopened while the bit is dirty, every UNLOGGED table is
  truncated to an empty root before the database accepts queries.
- Clean reopen preserves UNLOGGED contents.

#### SHOW / metadata

- `SHOW CREATE TABLE` reconstructs the prefix:
  - `CREATE TEMPORARY TABLE ...`
  - `CREATE UNLOGGED TABLE ...`
  - `CREATE TABLE ...` for permanent tables
- `information_schema.TABLES`, `information_schema.TABLE_CONSTRAINTS`,
  `information_schema.KEY_COLUMN_USAGE`, and `SHOW TABLE STATUS` include:
  - the current session's TEMP tables;
  - all permanent and unlogged tables;
  - no TEMP tables belonging to other sessions.

#### Constraints / restrictions

- Any FK declared on a TEMP table returns `DbError::NotImplemented`.
- Any FK declared on an UNLOGGED table returns `DbError::NotImplemented`.
- Any FK referencing a TEMP or UNLOGGED parent returns `DbError::NotImplemented`.
- `IMMUTABLE` may coexist with TEMP or UNLOGGED.
- `PRIMARY KEY` on TEMP or UNLOGGED still selects clustered storage, exactly as
  for permanent tables.

### Error cases

| Input | Expected error | Message requirement |
|-------|----------------|---------------------|
| `CREATE TEMP TABLE db.t (id INT)` | `DbError::InvalidValue` | Mentions TEMP tables must be unqualified |
| `CREATE TEMP TABLE schema.t (id INT)` | `DbError::InvalidValue` | Mentions TEMP tables must be unqualified |
| `CREATE TEMP TABLE t (...)` when temp `t` already exists in the session | `DbError::TableAlreadyExists` | Names the table |
| `CREATE UNLOGGED TABLE t (...)` when permanent `t` already exists in the schema | existing duplicate-name error | Names schema + table |
| FK declared on TEMP table | `DbError::NotImplemented` | Mentions TEMP-table foreign keys |
| FK declared on UNLOGGED table | `DbError::NotImplemented` | Mentions UNLOGGED-table foreign keys |
| `SHOW CREATE TABLE` on TEMP/UNLOGGED table | success | Prefix matches persistence |
| Reopen after dirty shutdown on UNLOGGED table | success | Table exists but has zero rows |

## Edge cases

- [x] TEMP table shadows a permanent table of the same name for unqualified resolution.
- [x] Qualified `public.t` still reaches the permanent table while `t` resolves to TEMP.
- [x] TEMP table disappears after disconnect / reset / change-user.
- [x] TEMP table created via `LIKE` preserves schema-only copy semantics.
- [x] TEMP table created via `AS SELECT` persists rows only for the session lifetime.
- [x] UNLOGGED table survives a clean reopen with its rows intact.
- [x] UNLOGGED table truncates on dirty reopen.
- [x] Legacy permanent table rows decode as `TablePersistence::Permanent`.
- [x] `IF NOT EXISTS` works for TEMP and UNLOGGED.
- [x] Other sessions cannot see a foreign TEMP table in information schema.

## On-disk format

### `axiom_tables` row trailer

`TableDef` keeps the existing v3 row and appends one persistence byte:

```text
legacy v3:
  [table_id:4][root_page_id:8][schema_len:1][schema][name_len:1][name]
  [layout:1][schema_version:8][immutable:1]

v4:
  legacy v3 bytes
  [persistence:1]   // 0 = Permanent, 1 = Temporary, 2 = Unlogged
```

Compatibility rules:

- v0–v3 rows decode as `TablePersistence::Permanent`.
- New readers accept v0–v4.
- New writers always emit v4.

### Meta page clean-shutdown flag

`page 0` gets one new byte:

```text
body[128] clean_shutdown: u8   // 1 = clean close, 0 = dirty/open-or-crashed
```

Compatibility rules:

- Missing byte on legacy databases is treated as `0` (dirty/unknown).
- New opens may conservatively truncate UNLOGGED tables after the first upgrade
  reopen if the legacy database has no clean flag yet.

## Performance budget

| Operation | Target | Max acceptable |
|-----------|--------|----------------|
| CREATE TEMP TABLE | Same order as CREATE TABLE | No more than 10% slower |
| Session temp cleanup on disconnect | O(number of temp tables) | Must not scan unrelated schemas |
| Dirty-open UNLOGGED truncation | O(number of unlogged tables + their indexes) | < 100 ms for 20 small tables |

Reference: existing CREATE TABLE / TRUNCATE bulk-empty paths in Phase 5/21.

## Dependencies

- Depends on current CREATE TABLE / CREATE TABLE LIKE / CTAS executor plumbing.
- Depends on connection lifecycle hooks in `crates/axiomdb-network/src/mysql/handler.rs`.
- Depends on catalog `TableDef` row compatibility rules.
- Blocks closing Phase 21.7 in `docs/progreso.md`.

## Open questions

None. The chosen scope is:

- TEMP tables via hidden session schema + automatic cleanup.
- UNLOGGED tables via catalog persistence flag + dirty-open truncation.
- No WAL-bypass runtime or ON COMMIT actions in 21.7.

## Done criteria

- [ ] AST and parser accept `CREATE TEMP[TORARY] TABLE` and `CREATE UNLOGGED TABLE`.
- [ ] `CreateTableStmt`, `CreateTableLikeStmt`, and `CreateTableAsSelectStmt`
      carry table persistence explicitly.
- [ ] Catalog `TableDef` persists table persistence backward-compatibly.
- [ ] Session context tracks a hidden temp schema for the current connection.
- [ ] TEMP table resolution shadows permanent tables for unqualified names and
      falls back to `public` when absent.
- [ ] TEMP tables auto-drop on disconnect, `COM_RESET_CONNECTION`, and
      `COM_CHANGE_USER`.
- [ ] Other sessions cannot see or resolve TEMP tables they do not own.
- [ ] UNLOGGED tables survive clean reopen and truncate on dirty reopen.
- [ ] `SHOW CREATE TABLE` reconstructs TEMPORARY / UNLOGGED prefixes.
- [ ] Information schema / SHOW metadata include current-session TEMP tables
      only and all UNLOGGED tables.
- [ ] FK attempts involving TEMP/UNLOGGED tables fail explicitly with
      `DbError::NotImplemented`.
- [ ] `cargo test -p axiomdb-catalog` passes.
- [ ] `cargo test -p axiomdb-sql` passes.
- [ ] `cargo test -p axiomdb-network` passes.
- [ ] `cargo clippy -p axiomdb-sql -- -D warnings` passes.
- [ ] `cargo clippy -p axiomdb-network -- -D warnings` passes.

## References

- `docs/progreso.md` — Phase 21.7 gap entry.
- `research/postgres/src/backend/commands/tablecmds.c` — TEMP/UNLOGGED
  persistence and temp-schema placement.
- `research/sqlite/src/build.c` — TEMP-table create restrictions on qualified names.
- Existing CREATE TABLE executor: `crates/axiomdb-sql/src/executor/ddl_create_table.rs`.
- Existing TRUNCATE bulk-empty path: `crates/axiomdb-sql/src/executor/ddl_analyze.rs`.
