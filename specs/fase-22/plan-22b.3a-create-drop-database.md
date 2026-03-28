# Plan: Database catalog and CREATE/DROP DATABASE (Phase 22b.3a)

## Files to create/modify
- `crates/axiomdb-storage/src/meta.rs` — reserve meta-page offsets for the new catalog heap roots
- `crates/axiomdb-catalog/src/schema.rs` — add `DatabaseDef` and `TableDatabaseDef` row codecs plus `DEFAULT_DATABASE_NAME`
- `crates/axiomdb-catalog/src/bootstrap.rs` — allocate new system heaps and bootstrap the legacy default database `axiomdb`
- `crates/axiomdb-catalog/src/reader.rs` — read databases and per-table database bindings with legacy fallback
- `crates/axiomdb-catalog/src/writer.rs` — create/drop databases and bind/delete table database ownership rows
- `crates/axiomdb-catalog/src/resolver.rs` — resolve tables in the effective database namespace
- `crates/axiomdb-catalog/src/lib.rs` — export the new catalog types/constants
- `crates/axiomdb-sql/src/ast.rs` — add `CREATE DATABASE`, `DROP DATABASE`, `USE`, `SHOW DATABASES` statement nodes
- `crates/axiomdb-sql/src/lexer.rs` — tokenize `USE`, `DATABASE`, `DATABASES`
- `crates/axiomdb-sql/src/parser/mod.rs` — route the new statements at top level
- `crates/axiomdb-sql/src/parser/ddl.rs` — parse create/drop database
- `crates/axiomdb-sql/src/analyzer.rs` — analyze statements against an effective database while preserving the default wrappers
- `crates/axiomdb-sql/src/schema_cache.rs` — key cached tables by `(database, schema, table)`
- `crates/axiomdb-sql/src/session.rs` — store selected database separately from the effective fallback database
- `crates/axiomdb-sql/src/executor/mod.rs` — route the new statements and use the effective database in ctx resolution
- `crates/axiomdb-sql/src/executor/shared.rs` — create schema resolvers bound to the effective database
- `crates/axiomdb-sql/src/executor/ddl.rs` — execute create/drop/use/show databases and create table ownership bindings
- `crates/axiomdb-core/src/error.rs` — add explicit database errors
- `crates/axiomdb-network/src/mysql/error.rs` — map the new database errors to MySQL codes
- `crates/axiomdb-network/src/mysql/database.rs` — analyze with the session’s effective database and expose existence checks
- `crates/axiomdb-network/src/mysql/session.rs` — invalidate prepared plans when the selected database changes
- `crates/axiomdb-network/src/mysql/handler.rs` — validate handshake DB / `COM_INIT_DB`, sync SQL `USE`, and stop hardcoding `SHOW DATABASES`
- `crates/axiomdb-catalog/tests/integration_catalog_rw.rs` — catalog persistence tests for databases/bindings
- `crates/axiomdb-sql/tests/integration_executor.rs` — end-to-end SQL tests for create/drop/use/show databases
- `crates/axiomdb-network/tests/integration_protocol.rs` — wire behavior for `COM_INIT_DB` and `SHOW DATABASES`

## Algorithm / Data structure
Persist two new system heaps:

1. `axiom_databases`
   - row format: `[name_len:1][name bytes]`
   - source of truth for `SHOW DATABASES` and `USE` validation
2. `axiom_table_databases`
   - row format: `[table_id:4 LE][name_len:1][name bytes]`
   - maps a user table to its owning database
   - missing row means legacy table in `axiomdb`

Resolution rules:

```text
selected_db = session.current_database if non-empty else None
effective_db = selected_db.unwrap_or("axiomdb")

lookup(table):
  if explicit schema given:
    match rows in effective_db + schema + table_name
  else:
    match rows in effective_db + default_schema("public") + table_name

table_database(table_id):
  if binding row exists -> binding.database_name
  else -> "axiomdb"
```

DDL flow:

```text
CREATE DATABASE name:
  if database exists -> DatabaseAlreadyExists
  insert DatabaseDef row

DROP DATABASE [IF EXISTS] name:
  if missing and IF EXISTS -> success
  if missing and no IF EXISTS -> DatabaseNotFound
  if session.selected_database == name -> ActiveDatabaseDrop
  scan visible table bindings + legacy tables
  collect every table owned by name
  for each table_id:
    delete stats/FKs/constraints/indexes/columns/table row
    delete table-database binding row if present
  delete database row

CREATE TABLE in ctx path:
  resolve existence in effective_db + schema
  create catalog table row
  if effective_db != "axiomdb" or a selected database exists:
    insert table-database binding for the effective_db
  else legacy table still works via explicit default binding omission
```

Wire/session flow:

```text
handshake database / COM_INIT_DB:
  validate catalog existence
  set conn_state.current_database = name
  set session.current_database = name

SQL USE name:
  execute via SQL path
  on success sync conn_state.current_database from session.current_database

SHOW DATABASES:
  handled by SQL executor, not hardcoded intercept
```

## Implementation phases
1. Add catalog data structures, meta offsets, bootstrap allocation, and catalog reader/writer support for databases plus table ownership bindings.
2. Thread effective database resolution through schema resolver, schema caches, analyzer wrappers, and ctx execution.
3. Add SQL AST/parser/executor support for `CREATE DATABASE`, `DROP DATABASE`, `USE`, and `SHOW DATABASES`.
4. Update MySQL wire handling for handshake database validation, `COM_INIT_DB`, prepared statement invalidation on database changes, and removal of hardcoded `SHOW DATABASES`.
5. Add regression tests for persistence, legacy fallback, destructive drop, and wire validation paths.

## Tests to write
- unit: `DatabaseDef` / `TableDatabaseDef` row codec roundtrips and parser coverage for new statements
- integration: catalog persistence across reopen; create/use/show/drop database; drop database cascades through tables; legacy tables still resolve under `axiomdb`
- integration: `COM_INIT_DB` rejects unknown databases and `SHOW DATABASES` reflects the persisted catalog
- bench: none for this subphase

## Anti-patterns to avoid
- Do not overload `schema_name` to smuggle the database name; that would block real schema namespacing later.
- Do not change the existing `TableDef` binary format for this subphase; preserve upgrade compatibility.
- Do not keep `SHOW DATABASES` as a handler stub once catalog-backed support exists.
- Do not silently map unknown `USE db` requests back to `axiomdb`.

## Risks
- Resolver/cache collisions across databases → key both caches by database as well as schema/table.
- Legacy data upgrade path could hide old tables → missing table-binding rows must resolve to `axiomdb`.
- `DROP DATABASE` could leave orphaned metadata → cascade through every catalog subsystem in one transaction and test restart visibility.
- Prepared statements could reuse a plan compiled in a different database → tie the prepared-plan cache to the selected database and invalidate on change.
