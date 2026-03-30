# Plan: Cross-database name resolution and queries (Phase 22b.3b)

## Files to create/modify
- `crates/axiomdb-sql/src/ast.rs` — extend `TableRef` with an optional `database` component and keep helper constructors compatible with one-part names
- `crates/axiomdb-sql/src/parser/mod.rs` — parse `table`, `schema.table`, and `database.schema.table`
- `crates/axiomdb-sql/src/analyzer.rs` — bind `TableRef` against either the session default database or the explicit database from the AST; preserve correct error precedence
- `crates/axiomdb-catalog/src/resolver.rs` — add a database-aware resolution entry point that can take an explicit database override per lookup
- `crates/axiomdb-sql/src/session.rs` — add helper(s) to compute the effective database for a `TableRef`
- `crates/axiomdb-sql/src/executor/shared.rs` — centralize effective `(database, schema, table)` resolution for cached table lookups and resolver construction
- `crates/axiomdb-sql/src/executor/select.rs` — route FROM/JOIN resolution through the explicit database-aware helper
- `crates/axiomdb-sql/src/executor/insert.rs` — support explicit cross-database target/source table resolution
- `crates/axiomdb-sql/src/executor/update.rs` — support explicit cross-database target resolution
- `crates/axiomdb-sql/src/executor/delete.rs` — support explicit cross-database target resolution
- `crates/axiomdb-sql/src/executor/ddl.rs` — make table-oriented DDL paths honor explicit `database.schema.table` when they already consume `TableRef`
- `crates/axiomdb-sql/tests/integration_dml_parser.rs` — parser regression coverage for 1/2/3-part table names
- `crates/axiomdb-sql/tests/integration_analyzer.rs` — semantic binding tests for explicit database overrides and database-not-found precedence
- `crates/axiomdb-sql/tests/integration_executor.rs` — end-to-end cross-database query and DML tests
- `crates/axiomdb-network/tests/integration_connection_lifecycle.rs` — wire-visible regression for explicit cross-database SQL under different `USE` states
- `tools/wire-test.py` — smoke/regression coverage for `database.schema.table` over the MySQL protocol

## Algorithm / Data structure

AST shape:

```text
TableRef {
  database: Option<String>,
  schema:   Option<String>,
  name:     String,
  alias:    Option<String>,
}
```

Parser rules:

```text
parse_table_ref():
  first = ident
  if next != '.':
    return { database: None, schema: None, name: first }

  consume '.'
  second = ident
  if next != '.':
    return { database: None, schema: Some(first), name: second }

  consume '.'
  third = ident
  return {
    database: Some(first),
    schema: Some(second),
    name: third,
  }
```

Resolution rules:

```text
effective_database(table_ref, session):
  table_ref.database.unwrap_or(session.effective_database())

effective_schema(table_ref):
  table_ref.schema.unwrap_or("public")

resolve(table_ref):
  db = effective_database(table_ref, session)
  schema = effective_schema(table_ref)

  if table_ref.database.is_some() and !catalog.database_exists(db):
    return DatabaseNotFound(db)

  return catalog.get_table_in_database(db, schema, table_ref.name)
```

Binding / cache rules:

```text
cache_key = "{db}.{schema}.{table}"

unqualified table:
  db = session.effective_database()
  schema = "public"

two-part name:
  db = session.effective_database()
  schema = explicit schema

three-part name:
  db = explicit database
  schema = explicit schema
```

Column binding remains unchanged in this subphase:

```text
accepted column refs:
  col
  alias.col
  table.col

not accepted yet:
  db.schema.table.col
```

## Implementation phases
1. Extend `TableRef` and the shared table-reference parser to represent 1-part, 2-part, and 3-part names without changing existing 1-part/2-part semantics.
2. Centralize effective database/schema computation in resolver/session helpers so analyzer and executor call sites cannot accidentally ignore explicit database qualifiers.
3. Thread the new `TableRef` semantics through analyzer binding and all executor paths that resolve tables, including DML and existing table-oriented DDL/DML operations.
4. Add parser, analyzer, executor, and wire regressions for cross-database resolution, explicit `DatabaseNotFound`, and no-regression behavior for the current database path.

## Tests to write
- unit: parser coverage for `table`, `schema.table`, and `database.schema.table`
- integration: analyzer resolves explicit database names and keeps `schema.table` bound to the current database
- integration: cross-database `SELECT`, `JOIN`, `INSERT ... SELECT`, `UPDATE`, and `DELETE`
- integration: explicit unknown database returns `DatabaseNotFound`
- integration: changing `USE` does not affect statements that use explicit `database.schema.table`
- wire: query over MySQL protocol using explicit three-part names across two logical databases
- bench: none for this subphase

## Anti-patterns to avoid
- Do not reinterpret `schema.table` as `database.table`.
- Do not flatten `database` and `schema` into one synthetic string field.
- Do not add partial support for 4-part column references in this subphase.
- Do not duplicate explicit-database lookup logic at every executor call site; keep one shared helper for effective table resolution.
- Do not silently downgrade `DatabaseNotFound` into `TableNotFound`.

## Risks
- Missed executor call sites could keep resolving against the current database only
  → mitigate by centralizing table resolution behind a `TableRef`-aware helper and covering DML/DDL integration tests.
- Wrong error precedence could return `TableNotFound` for an unknown explicit database
  → mitigate by checking explicit database existence before table lookup.
- Cache collisions across databases with the same schema/table names
  → mitigate by always keying caches with `database.schema.table`.
- Future `22b.4` schema work could be boxed in by shortcuts
  → mitigate by keeping the AST structure explicit: separate `database` and `schema` fields, no `database.table` shorthand.
