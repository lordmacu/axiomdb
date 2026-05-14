# Spec: Schema Namespacing

Phase: 22b — Platform features
Task: 22b.4 — Schema Namespacing
Status: implemented

## Context

AxiomDB already stores a `schema_name: String` on every catalog object (tables,
sequences, views, aggregates, enums) and defaults to `"public"`. The session holds
a `search_path: Vec<String>` that defaults to `["public"]`. `CREATE SCHEMA`,
`schema.table` qualified names in the parser, `SET search_path = '...'`, and
`schema_exists()` / `list_schemas()` in the catalog reader are all implemented.

What is missing are the DDL to remove schemas (`DROP SCHEMA`), the introspection
statement (`SHOW SCHEMAS`), and the `information_schema.SCHEMATA` virtual table
that ORMs use to discover available schemas.

This subphase closes 22b.4 and has no dependencies on other pending work.

## Goal

Add `DROP SCHEMA`, `SHOW SCHEMAS`, and `information_schema.SCHEMATA` to complete
the schema namespacing feature as agreed in the Phase 22b brainstorm.

## Non-goals

- `ALTER SCHEMA name RENAME TO new_name` — deferred; no ORM needs it today
- Schema-level permissions (`GRANT USAGE ON SCHEMA`) — deferred to Phase 28
- `CREATE SCHEMA name AUTHORIZATION user` — deferred; single-user engine
- `SHOW CREATE SCHEMA` — no MySQL equivalent; deferred
- `pg_catalog.pg_namespace` — PostgreSQL system catalog; out of scope

## Behavior

### 1 — DROP SCHEMA

#### SQL syntax

```sql
DROP SCHEMA [IF EXISTS] name [CASCADE | RESTRICT]
-- CASCADE and RESTRICT are also valid with the SCHEMA keyword only:
DROP SCHEMA name CASCADE
DROP SCHEMA IF EXISTS name RESTRICT
```

`SCHEMA` and `DATABASE` are treated as synonyms by MySQL; AxiomDB follows MySQL
for `DROP DATABASE` but `DROP SCHEMA` must also work here. The existing
`DROP DATABASE` is separate and operates at the database level.

#### Semantics

- **RESTRICT** (default when neither CASCADE nor RESTRICT is specified): returns
  `DbError::SchemaNotEmpty { name }` if the schema contains any tables or views.
  A schema that only has sequences or aggregates can be dropped under RESTRICT.
- **CASCADE**: drops every table and view in the schema (in any order, using the
  existing `drop_table_fully` helper), then deletes the schema catalog row.
- `DROP SCHEMA IF EXISTS name`: if schema does not exist, returns `QueryResult::Empty`
  (no error). If schema exists, applies RESTRICT or CASCADE logic as normal.
- `DROP SCHEMA public`: allowed. The next `CREATE TABLE` will re-insert the
  `public` row lazily via `create_schema` (the catalog writer already handles this).
- The `information_schema` schema cannot be dropped — return
  `DbError::InvalidOperation { reason: "cannot drop schema information_schema" }`.

#### New catalog writer method

```rust
/// Deletes the schema row from `axiom_schemas`.
///
/// Precondition: caller has already ensured no tables remain (RESTRICT path)
///               or has already dropped all contained tables (CASCADE path).
/// Does nothing if the schema row does not exist (e.g. `public` never explicitly
/// created).
pub fn delete_schema(
    &mut self,
    database: &str,
    schema: &str,
) -> Result<(), DbError>;
```

The implementation scans `axiom_schemas`, finds the row whose key equals
`"{database}\0{schema}"`, and deletes it via `HeapChain::delete`.

#### New AST node

```rust
pub struct DropSchemaStmt {
    pub name: String,
    pub if_exists: bool,
    pub cascade: bool,   // true = CASCADE, false = RESTRICT (default)
}
```

#### Error cases

| Condition | Error |
|-----------|-------|
| Schema does not exist, no IF EXISTS | `DbError::SchemaNotFound { name }` |
| Schema has tables and RESTRICT mode | `DbError::SchemaNotEmpty { name }` |
| `DROP SCHEMA information_schema` | `DbError::InvalidOperation { reason: "cannot drop schema information_schema" }` |

`DbError::SchemaNotEmpty` is a new variant:
```rust
SchemaNotEmpty { name: String }
```
Wire error code: MySQL 1010 / SQLSTATE `"HY000"`, message `"Can't drop schema '{name}'; schema is not empty"`.

---

### 2 — SHOW SCHEMAS

#### SQL syntax

```sql
SHOW SCHEMAS
SHOW SCHEMAS [LIKE 'pattern']
SHOW SCHEMAS [WHERE expr]
```

MySQL also accepts `SHOW DATABASES` as a synonym. The existing `SHOW DATABASES`
implementation is unaffected; `SHOW SCHEMAS` is a new parser path that reuses
the same executor logic.

`LIKE`/`WHERE` filtering is parsed and passed to the executor but need only
support the LIKE form for now (same as `SHOW TABLES`); WHERE can reject with
`DbError::NotImplemented` if complex.

#### Semantics

- Returns one result column named `"Schema"` (not `"Database"`).
- Lists all schemas in the current database (via `list_schemas(database)`).
- `public` always appears even for legacy databases (already guaranteed by
  `list_schemas`).
- `information_schema` does **not** appear in `SHOW SCHEMAS` output (it is a
  virtual namespace, not a stored schema). Matches MySQL behavior.
- Results are sorted alphabetically by schema name (already guaranteed by
  `list_schemas`).

#### New AST node

```rust
pub struct ShowSchemasStmt {
    pub like_pattern: Option<String>,
}
```

---

### 3 — information_schema.SCHEMATA

#### SQL semantics

```sql
SELECT * FROM information_schema.SCHEMATA
SELECT schema_name FROM information_schema.schemata  -- case-insensitive
```

#### Column definition (MySQL-compatible superset)

```
CATALOG_NAME                 TEXT   -- always "def" (MySQL compat)
SCHEMA_NAME                  TEXT   -- schema name
DEFAULT_CHARACTER_SET_NAME   TEXT   -- always "utf8mb4"
DEFAULT_COLLATION_NAME       TEXT   -- always "utf8mb4_general_ci"
SQL_PATH                     TEXT   -- always NULL (MySQL compat)
```

#### Semantics

- Lists all schemas returned by `list_schemas(database)` plus `information_schema`
  itself (so that introspection queries like `SELECT schema_name FROM
  information_schema.schemata` discover it).
- Row ordering: alphabetical by `SCHEMA_NAME`.
- `information_schema.schemata` is recognized by the existing `is_table_cols()`
  dispatch and `make_is_catalog_columns()` via a new `IS_SCHEMATA_COLS` constant.

---

## Edge cases

- [ ] `DROP SCHEMA public` — allowed; next table create re-registers it lazily
- [ ] `DROP SCHEMA IF EXISTS nonexistent` — no error, returns empty result
- [ ] `DROP SCHEMA information_schema` — rejected with InvalidOperation
- [ ] `DROP SCHEMA nonempty_schema` without CASCADE — rejects with SchemaNotEmpty
- [ ] `DROP SCHEMA nonempty_schema CASCADE` — drops all tables then schema
- [ ] `SHOW SCHEMAS` on a fresh database with only the default `public` — returns one row
- [ ] `SHOW SCHEMAS LIKE 'p%'` — returns only schemas matching the pattern
- [ ] `information_schema.schemata` on a fresh database — lists `public` and `information_schema`
- [ ] Schema name with uppercase letters — stored as-is (AxiomDB is case-sensitive for schema names, matching PostgreSQL)
- [ ] Concurrent `DROP SCHEMA CASCADE` while another session has an open transaction on a table in that schema — the transaction isolation layer handles this; no special case needed

## On-disk format

No new disk format. `delete_schema` reuses the existing `axiom_schemas` heap
chain and `HeapChain::delete`. The schema row format is unchanged:

```
[1 byte database_name_len] [database_name bytes] [1 byte name_len] [name bytes]
```

## Performance budget

| Operation | Target |
|-----------|--------|
| DROP SCHEMA RESTRICT | < 1 ms (single catalog scan + optional 1 delete) |
| DROP SCHEMA CASCADE (N tables) | O(N) table drops, same as N × DROP TABLE |
| SHOW SCHEMAS | < 1 ms (single heap scan of axiom_schemas) |
| information_schema.SCHEMATA | < 1 ms |

## Dependencies

- Depends on: existing `create_schema`, `list_schemas`, `schema_exists` in catalog (all done)
- Depends on: existing `drop_table_fully` in executor (done)
- Blocks: nothing (standalone feature)

## Open questions

None — all resolved in brainstorm.

## Done criteria

- [ ] `DROP SCHEMA name` (RESTRICT default) works; rejects non-empty schema with SchemaNotEmpty
- [ ] `DROP SCHEMA name CASCADE` drops all tables in schema then deletes schema row
- [ ] `DROP SCHEMA IF EXISTS name` suppresses SchemaNotFound
- [ ] `DROP SCHEMA information_schema` is rejected
- [ ] `SHOW SCHEMAS` returns one column "Schema" listing all schemas
- [ ] `SHOW SCHEMAS LIKE 'p%'` filters by pattern
- [ ] `SELECT * FROM information_schema.schemata` returns correct rows
- [ ] `information_schema.schemata` includes `information_schema` itself in the result
- [ ] `DbError::SchemaNotEmpty` wired to MySQL error 1010 / SQLSTATE HY000
- [ ] Integration tests in `tests/integration_schema_namespacing.rs` — full flow
- [ ] Wire smoke test section `[22b.4 schema namespacing]` added to `tools/wire-test.py`
- [ ] `cargo nextest run -p axiomdb-sql` passes
- [ ] `cargo nextest run -p axiomdb-catalog` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes
- [ ] `cargo fmt --check` passes

## References

- `crates/axiomdb-catalog/src/writer.rs:523` — `create_schema()`
- `crates/axiomdb-catalog/src/reader.rs:342` — `list_schemas()`
- `crates/axiomdb-catalog/src/reader.rs:308` — `schema_exists()`
- `crates/axiomdb-sql/src/executor/ddl_show.rs:61` — `execute_create_schema()`
- `crates/axiomdb-sql/src/executor/ddl_create_table.rs:380` — `ensure_schema_exists_for_create()`
- `crates/axiomdb-sql/src/parser/mod.rs:1031` — `parse_drop()` dispatch
- `crates/axiomdb-sql/src/information_schema.rs` — IS column schemas
- `crates/axiomdb-sql/src/executor/information_schema_exec.rs` — IS executor
- MariaDB: `sql/sql_table.cc:mysql_rm_db()` — DROP DATABASE/SCHEMA implementation
- PostgreSQL: `src/backend/commands/schemacmds.c:RemoveSchemaById()` — DROP SCHEMA
