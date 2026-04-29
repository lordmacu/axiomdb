# Phase 20 — Types + import/export

## 20.1 — Regular views (2026-04-27)

### What was built

**Regular SQL views** — `CREATE VIEW`, `CREATE OR REPLACE VIEW`, `DROP VIEW [IF EXISTS]`, `SHOW CREATE VIEW`, and transparent read-only access through views via `SELECT`.

### Architecture

#### Catalog layer (`axiomdb-catalog`)

- `RelationKind::View` (tag = 2) added to the existing enum alongside `Table` and `MaterializedView`.
- `TableDef::is_view()` helper.
- `CatalogWriter::create_view(schema, name, defining_query)` — allocates a `TableDef` with `root_page_id = 0` (no physical pages) and `RelationKind::View`; stores the raw SQL text in `defining_query`.
- `CatalogWriter::replace_view_query(table_id, new_query)` — used by `CREATE OR REPLACE VIEW` to update the stored SQL without changing the catalog entry ID.
- `CatalogReader` already reads all `TableDef` rows generically, so no changes were needed there.

#### Parser / AST (`axiomdb-sql`)

New AST nodes in `ast.rs`:
- `CreateViewStmt { or_replace, view, columns, query_sql, select }` — `query_sql` is the raw SQL text stored in the catalog.
- `DropViewStmt { if_exists, views }` — supports multi-name `DROP VIEW v1, v2, v3`.
- `ShowCreateViewStmt { view }`

Parser (`parser/ddl.rs`):
- `parse_create_view` — parses `CREATE [OR REPLACE] VIEW name [(col, ...)] AS SELECT ...`, captures the raw SQL text from the token stream.
- `parse_drop_view`, `parse_show_create_view`.

#### View expansion (transparent reads)

The key design is **analysis-time expansion** — views are rewritten into subqueries before any execution, matching the existing CTE expansion pattern.

In `analyzer_stmt.rs`:
- `expand_views(stmt, ...)` — called on every `SelectStmt` before `expand_ctes()`.
- `substitute_view_ref(from, ...)` — when a `FromClause::Table(tref)` resolves to a `RelationKind::View` in the catalog, it:
  1. Checks for circular view references via a `HashSet<String>` called `expanding`.
  2. Re-parses the stored `defining_query` SQL text.
  3. Recursively calls `expand_views()` on the parsed inner select to handle nested views.
  4. Returns `FromClause::Subquery { query: inner_select, alias, lateral: false }`.

This means the executor never sees view references — they have been transparently rewritten into subqueries.

#### Executor changes

`executor/ddl_view.rs` (new file, included via `include!` into `mod.rs`):
- `execute_create_view` — validates existence, calls `CatalogWriter::create_view` or `replace_view_query`.
- `execute_show_create_view` — reads `TableDef`, reconstructs `CREATE VIEW \`name\` AS <query>`.
- `execute_drop_view` — multi-name; respects `IF EXISTS`; checks `is_view()` to reject base tables.

`executor/select_core.rs`:
- `execute_select_derived` extended: after materializing the inner subquery rows, if `stmt.joins` is non-empty, routes to `execute_select_with_joins_first_materialized` — the same shared join-loop entry point used by `JSON_TABLE`. This fixes the `FromClause::Subquery + JOIN` path that was previously unhandled.

`executor/exec_entry.rs` (read-only path):
- Added `Stmt::ShowCreateView` case — `SHOW CREATE VIEW` starts with "SHOW" so it goes through the read-only executor path, which now handles it.

`executor/exec_dispatch.rs`:
- Dispatches `Stmt::CreateView`, `Stmt::DropView`, `Stmt::ShowCreateView`.

#### SHOW TABLES and information_schema

- `show_table_type_name()` updated to return `"VIEW"` when `table.is_view()`.
- `information_schema.VIEWS` new virtual table:
  - Columns: `TABLE_CATALOG`, `TABLE_SCHEMA`, `TABLE_NAME`, `VIEW_DEFINITION`, `CHECK_OPTION` (always `NONE`), `IS_UPDATABLE` (always `NO`).
  - `generate_is_views_rows()` filters catalog tables by `is_view()`.
- `is_table_cols("views")` added to `information_schema.rs`.
- `information_schema.TABLES` already reports `TABLE_TYPE = 'VIEW'` via the updated `show_table_type_name()` function.

### Coverage

- `crates/axiomdb-sql/tests/integration_views.rs` — 16 integration tests:
  - CREATE VIEW persists catalog entry
  - Duplicate view error
  - CREATE OR REPLACE VIEW updates definition
  - CREATE VIEW on existing table returns error
  - DROP VIEW removes catalog entry
  - DROP VIEW IF EXISTS on missing view succeeds
  - DROP VIEW on missing view returns error
  - DROP VIEW on base table returns error
  - DROP VIEW multi-name
  - SELECT from view expands transparently
  - View in JOIN (resolved via subquery expansion)
  - Nested view expansion (view on view)
  - Circular view reference error
  - SHOW CREATE VIEW returns DDL
  - SHOW CREATE VIEW on table returns error
  - information_schema.VIEWS returns view rows
- Wire smoke block `[20.1 regular views]` in `tools/wire-test.py` — 8 checks (473/473 total).

### Deferred to later phases

- Updatable views (INSERT/UPDATE/DELETE through a view) — requires write-path integration.
- `WITH CHECK OPTION` — requires updatable views.
- Column-name alias list (`CREATE VIEW v (a, b, c) AS SELECT ...`) — parser accepts it; executor stores but does not remap column names at query time.
- Security-definer/invoker views.
- `SHOW FULL TABLES WHERE Table_type = 'VIEW'` filtering.

## 20.2 — Sequences (2026-04-29)

### What was built

Standalone SQL sequences: `CREATE SEQUENCE`, `DROP SEQUENCE`, `NEXTVAL(text)`,
and `CURRVAL(text)`.

### Architecture

- `SequenceDef` stores schema/name plus `last_value`, `start_value`,
  `increment`, `min_value`, `max_value`, `cycle`, `cache_size`, and `is_called`.
- `axiom_sequences` is a new catalog heap root stored in the meta page and
  lazily initialized for legacy databases.
- `NEXTVAL` advances sequence state through a short internal transaction that
  commits immediately, so user rollback does not reuse consumed values.
- `CURRVAL` is held in `SessionContext.sequence_currvals`, keyed by lowercase
  `schema.sequence`.
- `SELECT` without `FROM` now uses the real session context in ctx execution so
  session functions like `CURRVAL` see state created by previous statements.

### Coverage

- `crates/axiomdb-sql/tests/integration_sequences.rs` — 12 integration tests:
  create/drop, `IF EXISTS`, duplicate create, invalid options, `NEXTVAL`,
  per-output-row advancement, `CURRVAL`, rollback gaps, and exhaustion.
- `crates/axiomdb-sql/tests/integration_ddl_parser.rs` — sequence parser tests.
- `tools/wire-test.py` — block `[20.2 sequences]` (476/476 total).
- Closeout gates: `cargo test --workspace`, `cargo clippy --workspace -- -D warnings`,
  and `cargo fmt --check`.

### Deferred to later phases

- `ALTER SEQUENCE`, `SETVAL`, `OWNED BY`, and sequence privileges.
- Wiring `SERIAL` / identity columns to standalone sequence objects.
- Sequence cache preallocation beyond `CACHE 1`.
