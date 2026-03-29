# Spec: Schema namespacing and search_path (Phase 22b.4)

## Reviewed first

These AxiomDB files were reviewed before writing this spec:

- `db.md`
- `docs/progreso.md`
- `docs/fase-22.md`
- `specs/fase-22/spec-22b.3b-cross-database-resolution.md`
- `specs/fase-22/plan-22b.3b-cross-database-resolution.md`
- `crates/axiomdb-catalog/src/schema.rs`
- `crates/axiomdb-catalog/src/reader.rs`
- `crates/axiomdb-catalog/src/writer.rs`
- `crates/axiomdb-catalog/src/resolver.rs`
- `crates/axiomdb-sql/src/ast.rs`
- `crates/axiomdb-sql/src/parser/mod.rs`
- `crates/axiomdb-sql/src/analyzer.rs`
- `crates/axiomdb-sql/src/session.rs`
- `crates/axiomdb-sql/src/executor/mod.rs`
- `crates/axiomdb-sql/src/executor/ddl.rs`
- `crates/axiomdb-sql/src/eval/functions/system.rs`
- `crates/axiomdb-network/src/mysql/session.rs`
- `research/postgres/src/backend/catalog/namespace.c`
- `research/postgres/src/backend/utils/adt/name.c`
- `research/postgres/src/backend/utils/misc/guc_parameters.dat`
- `research/duckdb/src/include/duckdb/catalog/catalog_search_path.hpp`
- `research/duckdb/src/planner/binder/statement/bind_create.cpp`
- `research/mariadb-server/sql/item_create.cc`
- `research/mariadb-server/sql/sql_path.cc`
- `research/mariadb-server/mysql-test/main/schema.test`
- `research/mariadb-server/mysql-test/main/schema.result`
- `research/sqlite/src/sqliteInt.h`

## Research synthesis

### What PostgreSQL does

PostgreSQL is the closest reference model for real schemas inside a database:

- `current_schema()` returns the first schema in the active search path
- unqualified relation names are resolved by scanning the active search path in order
- object creation without an explicit schema uses the active creation namespace
- `search_path` is session state, separate from the parser and separate from the catalog rows

This is the right semantic model for AxiomDB `22b.4`.

One PostgreSQL detail is **not** ideal for AxiomDB's first cut:

- PostgreSQL validates only the syntax of `SET search_path`, not the existence
  of every named schema at assignment time

That behavior is flexible, but it also defers errors until later resolution.

### What DuckDB does

DuckDB also has real schemas plus an explicit search-path object owned by the
client/session. It uses that path both for default object creation and for
unqualified lookup.

This reinforces two design choices for AxiomDB:

- the search path must be session state, not a parser trick
- default creation schema and lookup order must come from the same source of truth

### What MariaDB does

MariaDB is **not** a model for this subphase:

- `CREATE SCHEMA` is an alias for `CREATE DATABASE`
- `SCHEMA()` / `SCHEMAS()` are aliases of `DATABASE()`
- its `sql_path.cc` is about Oracle-style routine/package resolution, not
  relational table schemas inside one database

So MariaDB is relevant only as a compatibility surface for `SHOW TABLES` and
wire behavior, not as the semantic model for schema namespacing.

### What SQLite does

SQLite also is not the right model:

- it uses `main`, `temp`, and `ATTACH`ed databases
- it does not have `CREATE SCHEMA` or a relational schema search path

### AxiomDB-first decision

AxiomDB should implement schemas using the PostgreSQL/DuckDB model:

- schemas are explicit catalog objects scoped to a logical database
- each session has a `search_path`
- unqualified names search that path in order
- object creation without an explicit schema uses the first schema in the path
- `current_schema()` reflects the first effective schema in the path

However, AxiomDB should be stricter than PostgreSQL for the first cut:

- `SET search_path = ...` must validate that every named schema exists in the
  current database at assignment time

This keeps the initial implementation simpler and avoids delayed runtime errors.

## What to build (not how)

Add first-class schemas inside each logical database so AxiomDB supports:

- `CREATE SCHEMA name`
- `CREATE SCHEMA IF NOT EXISTS name`
- `SET search_path = schema1, schema2, ...`
- `RESET search_path`
- `SHOW search_path`
- schema-aware `current_schema()` / `schema()`
- schema-aware unqualified name resolution
- schema-aware default creation for unqualified `CREATE TABLE`

Schema scope is **per database**. The same schema name may exist in multiple
databases independently.

Every logical database must have a real `public` schema by default. When a
database is created, its `public` schema must exist immediately.

Resolution rules after this subphase:

- `table` resolves by scanning `search_path` within the effective database
- `schema.table` resolves to that exact schema in the effective database
- `database.schema.table` resolves explicitly and bypasses `search_path`

Creation rules after this subphase:

- `CREATE TABLE t (...)` creates in the first schema of the session search path
- `CREATE TABLE s.t (...)` creates in schema `s`
- `CREATE TABLE db.s.t (...)` is handled by the explicit cross-database path from `22b.3b`

Compatibility/upgrade rule:

- if older data directories contain tables whose `schema_name` exists in
  `TableDef` but has no explicit schema catalog row yet, that schema is treated
  as an implicit legacy schema for reads and validation so upgrades do not
  strand existing tables

`SHOW TABLES` behavior:

- `SHOW TABLES` lists tables only from the session's current schema
  (the first effective `search_path` entry)
- `SHOW TABLES FROM schema` lists tables from the explicitly requested schema

## Inputs / Outputs

- Input:
  - SQL strings:
    - `CREATE SCHEMA name`
    - `CREATE SCHEMA IF NOT EXISTS name`
    - `SET search_path = schema1, schema2, ...`
    - `RESET search_path`
    - `SHOW search_path`
    - table references using `table`, `schema.table`, and `database.schema.table`
  - a session with a selected current database or the default database fallback
  - catalog contents including legacy tables without explicit schema rows
- Output:
  - persisted schema catalog rows
  - session-level `search_path`
  - `current_schema()` / `schema()` results derived from that session state
  - schema-aware resolution and creation behavior
- Errors:
  - creating an existing schema without `IF NOT EXISTS`
  - setting `search_path` to an unknown schema in the current database
  - setting an empty `search_path`
  - creating or referencing objects in an unknown explicit schema
  - no silent fallback from an explicit unknown schema to `public`

## Use cases

1. Create schemas inside the current database:
   - `USE ventas`
   - `CREATE SCHEMA contabilidad`
   - `CREATE SCHEMA inventario`

2. Change unqualified lookup order:
   - `SET search_path = contabilidad, public`
   - `SELECT * FROM facturas`
   - resolves `contabilidad.facturas` first

3. Default creation follows `search_path`:
   - `SET search_path = inventario, public`
   - `CREATE TABLE productos (id INT)`
   - creates `inventario.productos`

4. Explicit schema bypasses the path:
   - `SET search_path = inventario, public`
   - `CREATE TABLE contabilidad.facturas (id INT)`
   - creates in `contabilidad`, not `inventario`

5. `SHOW TABLES` uses the current schema:
   - `SET search_path = inventario, public`
   - `SHOW TABLES`
   - lists only tables from `inventario`

6. Database switching resets schema context safely:
   - `USE analytics`
   - `search_path` becomes `public`
   - `current_schema()` returns `public`

7. Legacy upgrade safety:
   - an older database already contains tables with `schema_name = 'tenant_a'`
   - after upgrade, `SET search_path = tenant_a, public` succeeds
   - those tables remain reachable without migration loss

## Acceptance criteria

- [ ] `CREATE SCHEMA name` parses, executes, and persists a schema in the current database.
- [ ] `CREATE SCHEMA IF NOT EXISTS name` succeeds when the schema already exists.
- [ ] Every logical database has a real `public` schema by default.
- [ ] `SET search_path = a, b, ...` stores an ordered schema list in the session.
- [ ] `RESET search_path` resets the path to `public`.
- [ ] `SHOW search_path` returns the effective path in order.
- [ ] `current_schema()` / `schema()` return the first effective schema in the path.
- [ ] Unqualified table names resolve by scanning `search_path` in order.
- [ ] Unqualified `CREATE TABLE` uses the first schema in `search_path`.
- [ ] `SHOW TABLES` uses the current schema and `SHOW TABLES FROM schema` uses the explicit schema.
- [ ] `USE db` resets `search_path` to `public` for the new database.
- [ ] Legacy schemas derived only from existing table rows remain usable after upgrade.
- [ ] Explicit missing schemas are rejected with a user-facing error instead of silently falling back.

## Out of scope

- `DROP SCHEMA`
- `ALTER SCHEMA`
- `SHOW SCHEMAS`
- database-qualified entries inside `search_path`
- role-based implicit schemas like PostgreSQL's `"$user"`
- temporary schemas (`pg_temp`-style semantics)
- privileges / ACLs per schema
- 4-part column qualification (`db.schema.table.col`)

## Dependencies

- `22b.3a` database catalog and `USE`
- `22b.3b` explicit `database.schema.table` parsing and resolution
- existing session state machinery in SQL executor and MySQL wire session
- existing catalog table ownership model

## ⚠️ DEFERRED

- `DROP SCHEMA` / cascade semantics → later schema-management subphase
- `SHOW SCHEMAS` and richer schema introspection → later metadata/introspection subphase
- `database.schema.table.col` and `database.schema.table.*` → later namespacing follow-up after `22b.4`
- schema-level privileges and ownership → later security/platform subphases
