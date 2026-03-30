# Plan: Schema namespacing and search_path (Phase 22b.4)

## Files to create/modify
- `crates/axiomdb-storage/src/meta.rs` — reserve a meta-page root for the schema catalog heap
- `crates/axiomdb-catalog/src/schema.rs` — add a persisted schema row type scoped to `(database, schema)`
- `crates/axiomdb-catalog/src/bootstrap.rs` — allocate the schema catalog root and bootstrap `public` for the default database
- `crates/axiomdb-catalog/src/reader.rs` — read schemas per database, validate existence, list schemas, and synthesize legacy schemas from existing tables when explicit rows are missing
- `crates/axiomdb-catalog/src/writer.rs` — create schema rows and ensure `public` exists whenever a database is created
- `crates/axiomdb-catalog/src/lib.rs` — export the new schema catalog types/helpers
- `crates/axiomdb-catalog/src/resolver.rs` — resolve unqualified tables by scanning a provided search path instead of hardcoding only `public`
- `crates/axiomdb-sql/src/ast.rs` — add `CreateSchemaStmt`, `ShowSearchPathStmt`, and any small statement nodes needed for `RESET search_path`
- `crates/axiomdb-sql/src/lexer.rs` — tokenize `SCHEMA`, `SEARCH_PATH`, and any missing keywords used by the new statements
- `crates/axiomdb-sql/src/parser/mod.rs` — parse `CREATE SCHEMA`, `SHOW search_path`, and `RESET search_path`
- `crates/axiomdb-sql/src/parser/ddl.rs` — parse `CREATE SCHEMA [IF NOT EXISTS]`
- `crates/axiomdb-sql/src/session.rs` — add session-owned `search_path`, helpers for current/effective schema, and reset-on-database-change behavior
- `crates/axiomdb-sql/src/analyzer.rs` — resolve unqualified names against the ordered search path and preserve explicit schema/database precedence
- `crates/axiomdb-sql/src/schema_cache.rs` — keep cache behavior compatible with per-database/per-schema resolution
- `crates/axiomdb-sql/src/executor/mod.rs` — route new statements and teach `SET search_path` / `RESET search_path`
- `crates/axiomdb-sql/src/executor/shared.rs` — centralize effective schema resolution for readers/executors
- `crates/axiomdb-sql/src/executor/ddl.rs` — execute `CREATE SCHEMA`, `SHOW search_path`, schema-aware `SHOW TABLES`, and unqualified create-table placement
- `crates/axiomdb-sql/src/eval/functions/system.rs` — make `current_schema()` / `schema()` read real session state
- `crates/axiomdb-network/src/mysql/database.rs` — propagate schema-aware session defaults into analysis/execution paths
- `crates/axiomdb-network/src/mysql/session.rs` — persist/reset search-path session state across SQL and database switches
- `crates/axiomdb-network/src/mysql/handler.rs` — ensure `USE db` synchronizes the SQL session and resets search-path state
- `crates/axiomdb-catalog/tests/integration_schema_binding.rs` — add schema catalog and legacy-schema fallback tests
- `crates/axiomdb-sql/tests/integration_dml_parser.rs` — parser coverage for `CREATE SCHEMA`, `SHOW search_path`, and schema-qualified resolution
- `crates/axiomdb-sql/tests/integration_analyzer.rs` — search-path ordering, explicit schema precedence, and legacy-schema fallback
- `crates/axiomdb-sql/tests/integration_executor.rs` — end-to-end schema creation, path changes, current schema, and `SHOW TABLES`
- `crates/axiomdb-network/tests/integration_connection_lifecycle.rs` — wire-visible `USE db` resets search path and `SHOW search_path` works
- `tools/wire-test.py` — smoke/regression coverage for `CREATE SCHEMA`, `SET/RESET search_path`, and schema-aware lookups

## Algorithm / Data structure

Persist a new schema catalog:

```text
axiom_schemas
  row = [database_name_len:1][database_name bytes][schema_name_len:1][schema_name bytes]
```

Session state:

```text
SessionContext {
  current_database: String,
  search_path: Vec<String>,   // ordered, never empty after normalization
}
```

Defaulting rules:

```text
on new session:
  current_database = ""
  effective_database = "axiomdb"
  search_path = ["public"]

on USE db:
  current_database = db
  search_path = ["public"]
```

Schema existence rules:

```text
schema_exists(database, schema):
  true if explicit row exists in axiom_schemas
  true if any visible table in that database has table_def.schema_name == schema
  false otherwise
```

Search-path validation:

```text
SET search_path = s1, s2, ...:
  parse ordered identifier list
  reject empty list
  for each schema:
    if !schema_exists(current_database, schema):
      error
  session.search_path = [s1, s2, ...]

RESET search_path:
  session.search_path = ["public"]
```

Lookup rules:

```text
resolve(table_ref):
  if explicit database + explicit schema:
    use them directly
  else if explicit schema only:
    database = session.effective_database()
    schema = explicit schema
  else:
    database = session.effective_database()
    for schema in session.search_path:
      if table exists in (database, schema, table_name):
        return first match
    table not found
```

Creation rules:

```text
create_table(table_ref):
  if explicit schema:
    require schema_exists(database, schema)
    create there
  else:
    schema = session.search_path.first()
    require schema exists
    create there
```

`SHOW TABLES` rules:

```text
SHOW TABLES:
  schema = session.search_path.first()
  list tables only in that schema

SHOW TABLES FROM schema:
  list tables in explicit schema
```

`current_schema()` / `schema()`:

```text
return session.search_path.first()
```

## Implementation phases
1. Add the persisted schema catalog plus bootstrap behavior so every database gets `public`, while preserving legacy visibility through table-derived implicit schemas.
2. Add session-owned `search_path` state with reset-on-`USE`, strict validation on `SET`, and helpers for current/effective schema.
3. Thread ordered search-path resolution through the catalog resolver, analyzer, and executor so unqualified lookup and unqualified create-table use the same policy.
4. Add new SQL surface (`CREATE SCHEMA`, `SHOW search_path`, `RESET search_path`) and update existing introspection/system-function behavior (`SHOW TABLES`, `current_schema()`).
5. Add wire-visible regressions and upgrade-safety tests, then document the PostgreSQL/DuckDB-inspired model and the deliberate AxiomDB deviation of validating path entries at `SET` time.

## Tests to write
- unit: parser coverage for `CREATE SCHEMA [IF NOT EXISTS]`, `SHOW search_path`, and `RESET search_path`
- integration: `CREATE SCHEMA` persists per database and auto-created `public` exists in every database
- integration: `SET search_path` changes unqualified lookup order
- integration: `RESET search_path` restores `public`
- integration: `SHOW TABLES` uses current schema and `SHOW TABLES FROM schema` uses explicit schema
- integration: `current_schema()` / `schema()` return the first path entry
- integration: unknown schema in `SET search_path` is rejected
- integration: `USE db` resets the path to `public`
- integration: legacy schemas inferred from existing table rows remain resolvable after upgrade
- wire: `CREATE SCHEMA`, `SET search_path`, `SHOW search_path`, and `SHOW TABLES` through the MySQL protocol
- bench: none for this subphase

## Anti-patterns to avoid
- Do not model schemas as aliases of databases; that would reproduce MariaDB semantics, not real namespacing.
- Do not store `search_path` as a single raw string only; keep a normalized ordered vector in session state.
- Do not hardcode `public` in analyzers/executors once `search_path` exists; all unqualified lookup must go through the same helper.
- Do not break upgrades by requiring explicit schema rows for old tables; preserve implicit legacy schemas.
- Do not let `USE db` keep a stale path from a different database.

## Risks
- Legacy upgrades could fail if old schema names have no explicit catalog row
  → mitigate by treating distinct `TableDef.schema_name` values as implicit existing schemas.
- Divergence between lookup and creation defaults could create objects in a different schema than lookup expects
  → mitigate by deriving both from the same `search_path` helper.
- `SET search_path` could become hard to debug if errors are deferred
  → mitigate by validating all schemas eagerly at assignment time.
- Missed call sites could keep using `"public"` directly
  → mitigate by centralizing schema selection in resolver/session helpers and covering parser/analyzer/executor/wire tests.
- `USE db` could leave stale prepared plans or stale schema caches
  → mitigate by reusing the existing session invalidation path when database or search-path state changes.
