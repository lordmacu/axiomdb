# Spec: Database catalog and CREATE/DROP DATABASE (Phase 22b.3a)

## Reviewed first

These AxiomDB files were reviewed before writing this spec:

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-sql/src/ast.rs`
- `crates/axiomdb-sql/src/parser/mod.rs`
- `crates/axiomdb-sql/src/parser/ddl.rs`
- `crates/axiomdb-sql/src/executor/ddl.rs`
- `crates/axiomdb-sql/src/session.rs`
- `crates/axiomdb-network/src/mysql/handler.rs`
- `crates/axiomdb-network/src/mysql/database.rs`
- `crates/axiomdb-network/src/mysql/session.rs`
- `crates/axiomdb-catalog/src/reader.rs`
- `crates/axiomdb-catalog/src/writer.rs`
- `crates/axiomdb-catalog/src/schema.rs`

## Research synthesis

### What AxiomDB already does today

The current server exposes database-shaped behavior at the wire layer, but not
in the SQL/catalog layer:

- `COM_INIT_DB` / `USE db` updates `ConnectionState.current_database`
- `SELECT DATABASE()` and `current_database()` read that connection state
- `SHOW DATABASES` is currently hardcoded to a single row: `axiomdb`
- SQL DDL supports `CREATE TABLE`, `DROP TABLE`, `CREATE INDEX`, `DROP INDEX`,
  `ALTER TABLE`, `TRUNCATE`, `ANALYZE`
- SQL DDL does **not** parse `CREATE DATABASE` or `DROP DATABASE`
- catalog table definitions store `schema_name` and `table_name`, but there is
  no persisted database catalog and no persisted per-table database namespace

So the user-visible gap is not only parser support. The server currently lacks a
real source of truth for databases.

### Constraints that shape this subphase

- The current table catalog format already exists on disk and should not be
  broken casually for this subphase.
- The parser today supports at most `schema.table`, not `database.schema.table`.
- The design in `db.md` expects true multi-database support later, including
  `CREATE DATABASE`, `DROP DATABASE`, `USE`, and eventually cross-database
  queries.
- Existing installations already contain tables under the implicit default
  schema `public` and the server advertises a current database of `axiomdb`.

### Borrow / reject / adapt

#### MySQL
- Borrow: `CREATE DATABASE`, `DROP DATABASE [IF EXISTS]`, `USE db`,
  `SHOW DATABASES`, and refusing operations on unknown databases.
- Reject: introducing MySQL's full privilege model or filesystem-per-database
  layout in this subphase.
- Adapt: keep MySQL-like user-visible syntax and errors, but back it with
  AxiomDB's existing catalog/WAL transaction model.

#### PostgreSQL
- Borrow: treat database metadata as explicit catalog state rather than
  transport-only session state.
- Reject: PostgreSQL's process-per-database and separate-cluster semantics.
- Adapt: one AxiomDB server/process, one catalog, multiple logical databases.

### AxiomDB-first decision

This subphase should add a real persisted database catalog and route the
existing wire/session behaviors through it, but must stop short of
cross-database name resolution.

The chosen scope is:

- add persisted database metadata
- add SQL support for `CREATE DATABASE` and `DROP DATABASE`
- validate `USE db` / `COM_INIT_DB` against the persisted database catalog
- make `SHOW DATABASES` read from the catalog instead of returning a hardcoded row
- preserve compatibility by treating existing objects as belonging to the
  default database `axiomdb`

The chosen non-goal is:

- no `database.schema.table` resolution yet
- no cross-database queries yet
- no `CREATE SCHEMA` yet

## What to build (not how)

Add first-class persisted database objects so AxiomDB supports:

- `CREATE DATABASE db_name`
- `DROP DATABASE db_name`
- `DROP DATABASE IF EXISTS db_name`
- `USE db_name`
- `SHOW DATABASES`

with real catalog-backed semantics rather than wire-protocol stubs.

The implementation must make database existence durable and observable across
server restarts. It must also define how pre-existing tables are interpreted:
all legacy objects currently stored without an explicit database namespace are
considered part of the default database `axiomdb`.

`DROP DATABASE` must be destructive at the logical catalog level: after the
drop commits, the database must no longer appear in `SHOW DATABASES`, `USE db`
must fail, and all tables/indexes/constraints/stats that belong to that
database must be unreachable from SQL/catalog readers.

## Inputs / Outputs

- Input:
  - SQL strings:
    - `CREATE DATABASE name`
    - `DROP DATABASE name`
    - `DROP DATABASE IF EXISTS name`
    - `USE name`
    - `SHOW DATABASES`
  - MySQL wire `COM_INIT_DB` with a database name
  - existing catalog contents, including legacy tables created before
    multi-database support
- Output:
  - persisted database catalog rows
  - `QueryResult::Affected` for `CREATE DATABASE` / `DROP DATABASE`
  - `QueryResult::Rows` for `SHOW DATABASES`
  - updated per-connection current database for `USE` / `COM_INIT_DB`
- Errors:
  - create existing database: duplicate-object style error
  - drop missing database without `IF EXISTS`: undefined-object style error
  - `USE` missing database: undefined-object style error
  - dropping the database currently selected by the same connection: explicit
    user-facing error
  - no new silent fallbacks to the hardcoded `axiomdb` rowset

## Use cases

1. Create a database and switch into it:
   - `CREATE DATABASE ventas`
   - `USE ventas`
   - `SELECT DATABASE()` returns `ventas`

2. Show databases from a real catalog:
   - `SHOW DATABASES`
   - returns `axiomdb` plus any additional created databases

3. Drop an existing database:
   - `CREATE DATABASE tempdb`
   - `DROP DATABASE tempdb`
   - `SHOW DATABASES` no longer lists `tempdb`

4. Drop a missing database with `IF EXISTS`:
   - `DROP DATABASE IF EXISTS ghostdb`
   - succeeds without error

5. Protect the current connection from self-invalidating state:
   - `USE ventas`
   - `DROP DATABASE ventas`
   - fails with a clear error instead of leaving the session in a broken state

6. Preserve existing installations:
   - A server opened on an old data directory still exposes `axiomdb`
   - existing legacy tables remain reachable exactly as before

## Acceptance criteria

- [ ] `CREATE DATABASE name` parses, analyzes, executes, and persists the new database.
- [ ] `DROP DATABASE name` parses, analyzes, executes, and removes the database from the catalog.
- [ ] `DROP DATABASE IF EXISTS name` succeeds when the database does not exist.
- [ ] `USE name` and wire-level `COM_INIT_DB` reject unknown databases.
- [ ] `SHOW DATABASES` is catalog-backed and lists all persisted databases.
- [ ] The default database `axiomdb` exists on fresh startup and on upgraded
      legacy databases.
- [ ] Legacy tables created before this subphase remain accessible without data loss.
- [ ] Dropping a database logically removes all of its tables, columns, indexes,
      constraints, foreign keys, and stats from catalog readers.
- [ ] Dropping the database currently selected by the same connection is rejected.
- [ ] Server restart preserves created and dropped databases correctly.

## Out of scope

- Cross-database queries
- `database.schema.table` syntax
- `CREATE SCHEMA` / `DROP SCHEMA`
- Per-database compat mode, collation, encryption, quotas, or ownership
- Privileges / ACLs / users
- Physical file-per-database layout

## Dependencies

- Existing catalog bootstrap and system-table WAL logging
- Existing DDL execution pipeline (`parse → analyze → execute_with_ctx`)
- Existing `COM_INIT_DB`, `SHOW DATABASES`, and `SELECT DATABASE()` wire hooks
- Existing schema cache invalidation/versioning machinery

## ⚠️ DEFERRED

- Fully-qualified `database.schema.table` name resolution → later multi-database subphase
- Cross-database JOIN / SELECT / DML → later multi-database subphase
- Database-local schemas beyond the current `public` convention → later schema namespacing subphase
- Per-database runtime configuration (`COMPAT`, encryption, quotas) → later platform subphases
