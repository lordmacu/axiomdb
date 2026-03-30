# Spec: Cross-database name resolution and queries (Phase 22b.3b)

## Reviewed first

These AxiomDB files were reviewed before writing this spec:

- `db.md`
- `docs/progreso.md`
- `docs/fase-22.md`
- `crates/axiomdb-sql/src/ast.rs`
- `crates/axiomdb-sql/src/parser/mod.rs`
- `crates/axiomdb-sql/src/parser/dml.rs`
- `crates/axiomdb-sql/src/parser/expr.rs`
- `crates/axiomdb-sql/src/analyzer.rs`
- `crates/axiomdb-sql/src/executor/shared.rs`
- `crates/axiomdb-sql/src/session.rs`
- `crates/axiomdb-sql/src/schema_cache.rs`
- `crates/axiomdb-catalog/src/resolver.rs`
- `crates/axiomdb-catalog/src/reader.rs`
- `crates/axiomdb-network/src/mysql/database.rs`

## Research synthesis

### What AxiomDB already does today

Phase `22b.3a` added a real persisted database catalog, `USE`, `SHOW DATABASES`,
and per-table ownership bindings. The engine already has:

- a durable `axiom_databases` catalog
- session-level `current_database`
- catalog lookups by `(database, schema, table)`
- session cache keys in the form `database.schema.table`

So the storage/catalog layer is already database-aware.

### Current gap

The remaining limitation is in SQL name resolution:

- `TableRef` currently supports only `table` or `schema.table`
- the parser does not accept `database.schema.table`
- the analyzer resolves every table against a single default database
- expression names and qualified wildcards only understand `table.column` and `table.*`

This means multi-database catalogs exist, but a single SQL statement cannot
address tables from two databases at once.

### Constraints that shape this subphase

- `22b.4` will add real schema namespacing, so this subphase must not overload
  `schema` semantics or invent shortcuts that would conflict with it later.
- Existing single-database SQL must remain unchanged:
  - `table` means `current_database.public.table`
  - `schema.table` means `current_database.schema.table`
- Two-part names must continue to mean `schema.table`, not `database.table`.
- The current expression binder only supports up to `table.column`, so this
  subphase should use aliases for cross-database joins rather than trying to
  add 4-part column references at the same time.

### Borrow / reject / adapt

#### MySQL
- Borrow: explicit multi-part object names override the current database.
- Reject: treating two-part names as `database.table`; in AxiomDB that would
  block real schema namespacing.
- Adapt: keep MySQL-like `USE db` session defaults, but require three-part
  names for explicit cross-database references.

#### PostgreSQL
- Borrow: `database.schema.table` semantics as an explicit fully-qualified path.
- Reject: PostgreSQL's cluster/database separation and inability to join across
  databases without FDW.
- Adapt: permit cross-database reads and writes inside one AxiomDB server
  because all logical databases live in one catalog and one storage engine.

### AxiomDB-first decision

This subphase should add explicit three-part table resolution:

- `table`
- `schema.table`
- `database.schema.table`

and make every existing table-reference position resolve against the correct
logical database.

It should stop short of:

- `CREATE SCHEMA`
- `search_path`
- `database.table` shorthand
- `database.schema.table.column`
- `database.schema.table.*`

## What to build (not how)

Add explicit cross-database table resolution so a single SQL statement can
address tables from multiple logical databases in the same server.

The accepted table-name forms are:

- `table`
- `schema.table`
- `database.schema.table`

Resolution rules:

- `table` resolves to `session.current_database_or_default.public.table`
- `schema.table` resolves to `session.current_database_or_default.schema.table`
- `database.schema.table` resolves to that exact table and does not depend on `USE`

Every existing statement form that already accepts a table reference must apply
those rules consistently. This includes read paths (`SELECT`, `JOIN`,
`INSERT ... SELECT`) and write paths (`INSERT`, `UPDATE`, `DELETE`) as well as
table-oriented DDL/DML that already uses `TableRef`.

When a query uses explicit cross-database table names, column references inside
the statement continue to use the existing forms:

- unqualified `column`
- qualified `alias.column`
- qualified `table.column`

The subphase does not add 3-part or 4-part column qualification. Cross-database
queries should therefore rely on aliases for clarity.

## Inputs / Outputs

- Input:
  - SQL statements containing table references in one of these forms:
    - `table`
    - `schema.table`
    - `database.schema.table`
  - a session with or without a selected current database
  - a catalog containing multiple databases and table ownership bindings
- Output:
  - parsed `TableRef` values that preserve optional database and schema parts
  - analyzed statements whose table bindings point at the intended database
  - successful cross-database reads/writes against existing tables
- Errors:
  - explicit missing database: `DatabaseNotFound`
  - explicit existing database but missing table: `TableNotFound`
  - ambiguous unqualified columns in multi-table queries: `AmbiguousColumn`
  - no silent fallback from an explicit unknown database to the current database

## Use cases

1. Resolve an explicit table outside the current database:
   - `USE ventas`
   - `SELECT COUNT(*) FROM analytics.public.events`
   - reads `analytics.public.events`, not `ventas.public.events`

2. Join across databases in one statement:
   - `SELECT v.id, a.score`
   - `FROM ventas.public.orders v`
   - `JOIN analytics.public.scores a ON v.id = a.order_id`

3. Write into one database from another:
   - `INSERT INTO analytics.public.order_scores`
   - `SELECT id, total FROM ventas.public.orders`

4. Preserve current behavior for one-part and two-part names:
   - `USE analytics`
   - `SELECT * FROM events`
   - `SELECT * FROM public.events`
   - both still resolve inside `analytics`

5. Explicit database names override `USE`:
   - `USE analytics`
   - `UPDATE ventas.public.orders SET status = 'done' WHERE id = 7`
   - updates `ventas.public.orders`

6. Error precedence is correct:
   - `SELECT * FROM ghost.public.events`
   - returns `DatabaseNotFound`, not `TableNotFound`

## Acceptance criteria

- [ ] `database.schema.table` parses successfully everywhere AxiomDB already accepts a table reference.
- [ ] `table` still resolves to `effective_database.public.table`.
- [ ] `schema.table` still resolves to `effective_database.schema.table`.
- [ ] `database.schema.table` resolves to the explicit database and ignores `USE`.
- [ ] Cross-database `SELECT` and `JOIN` work in one statement.
- [ ] Cross-database `INSERT ... SELECT` works when source and destination are in different databases.
- [ ] Cross-database `UPDATE` and `DELETE` work against explicitly qualified target tables.
- [ ] Table-reference caching remains isolated by database as well as schema/table.
- [ ] Explicit unknown databases return `DatabaseNotFound`.
- [ ] Single-database legacy queries keep their existing behavior unchanged.

## Out of scope

- `CREATE SCHEMA`, `DROP SCHEMA`, or schema catalog objects
- `search_path`
- `database.table` shorthand
- `database.schema.table.column`
- `database.schema.table.*`
- cross-database foreign keys or constraints that reference parent tables by database name
- privilege checks between logical databases

## Dependencies

- `22b.3a` database catalog and per-table database ownership bindings
- existing `USE` / `current_database` session machinery
- existing schema cache keyed by `database.schema.table`
- existing analyzer / executor name-binding pipeline

## ⚠️ DEFERRED

- Real schema objects and `CREATE SCHEMA` → `22b.4`
- `search_path` and schema-resolution policy beyond the current explicit/default behavior → `22b.4`
- 3-part wildcard and 4-part column qualification (`db.schema.table.*`, `db.schema.table.col`) → later namespacing work after `22b.4`
- Cross-database FK references and schema-level privilege boundaries → later platform subphases
