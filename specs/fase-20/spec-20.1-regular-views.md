# Spec: 20.1 — Regular Views

Phase: 20 — Types + import/export
Task: 20.1 Regular views
Status: approved

## Context

AxiomDB already has materialized views (13.1) which store data physically.
Regular views are the complementary feature: they store only a SQL query in
the catalog and expand it transparently at analysis time, with no physical
storage. The analyzer already has `expand_ctes()` which rewrites
`FromClause::Table → FromClause::Subquery` — views follow the exact same
pattern. `TableDef` already has `relation_kind: RelationKind` and
`defining_query: Option<String>` which materialized views use; regular views
reuse both fields.

## Goal

Deliver `CREATE [OR REPLACE] VIEW`, `DROP VIEW`, and transparent query
expansion so that views are usable in any `FROM` position without physical
storage.

## Non-goals

- Not implementing updatable views (INSERT/UPDATE/DELETE through a view) — deferred
- Not implementing `WITH CHECK OPTION` / `WITH CASCADED CHECK OPTION` — deferred
- Not implementing `CREATE RECURSIVE VIEW` — deferred
- Not implementing `ALTER VIEW` — deferred
- Not implementing `information_schema.VIEW_COLUMN_USAGE` — deferred
- Not optimizing repeated view re-parsing (parse cache) — not needed in MVP

## Behavior

### Public SQL surface

```sql
-- Create
CREATE VIEW v AS SELECT ...;
CREATE VIEW v (col1, col2) AS SELECT ...;
CREATE OR REPLACE VIEW v AS SELECT ...;

-- Drop
DROP VIEW v;
DROP VIEW IF EXISTS v;
DROP VIEW v1, v2;                 -- multi-name
DROP VIEW IF EXISTS v1, v2;

-- Use (transparent — no new syntax)
SELECT * FROM v;
SELECT * FROM v JOIN t ON v.id = t.id;
SELECT * FROM v WHERE v.x > 0;
SELECT * FROM (SELECT * FROM v) sub;

-- Introspection
SHOW CREATE VIEW v;
SHOW FULL TABLES;                 -- shows Table_type = 'VIEW'
SELECT * FROM information_schema.TABLES WHERE TABLE_TYPE = 'VIEW';
SELECT * FROM information_schema.VIEWS;
```

### Semantics

**CREATE VIEW:**
- Parses and stores the defining SELECT as raw SQL text in `defining_query`
- `relation_kind = RelationKind::View`
- No physical pages allocated (`root_page_id = 0`)
- Column names list is optional; if provided, must match SELECT output width
- `CREATE OR REPLACE`: if view exists, overwrites `defining_query`; if a base
  table with the same name exists, error
- Duplicate name without `OR REPLACE` → error

**DROP VIEW:**
- Removes catalog entry
- `IF EXISTS`: silences missing-view error, returns `Affected(0)`
- Multi-name: drops each in order; first error aborts (no partial drop)
- Dropping a base table with `DROP VIEW` → error (wrong object type)

**Query expansion (analyzer):**
- `expand_views()` runs before `expand_ctes()` in `analyze_stmt`
- For each `FromClause::Table(tref)` with no schema qualifier or schema
  `"public"`, look up name in catalog; if `RelationKind::View`, re-parse
  `defining_query` and substitute `FromClause::Subquery { query, alias }`
- Column-name list override: if view has column names, wrap subquery in
  a projection that renames columns
- Schema-qualified view references (`schema.view`) are resolved against the
  matching schema
- Circular views (v1 → v2 → v1) are detected by a name set passed through
  the recursive expansion; error: `"circular view reference: v1"`
- Expansion is recursive: a view that references another view expands both

**SHOW CREATE VIEW:**
- Returns two columns: `View`, `Create View`
- `Create View` is the canonical `CREATE VIEW name AS <defining_query>`
- If name is a base table → error

**information_schema.VIEWS:**
- Columns: `TABLE_CATALOG`, `TABLE_SCHEMA`, `TABLE_NAME`, `VIEW_DEFINITION`,
  `CHECK_OPTION` (always `'NONE'`), `IS_UPDATABLE` (always `'NO'`)

### Error cases

| Input | Expected error |
|-------|----------------|
| `CREATE VIEW v AS SELECT ...` when `v` is a base table | `InvalidValue` — object `v` already exists as a table |
| `CREATE VIEW v AS SELECT ...` when `v` is already a view (no OR REPLACE) | `InvalidValue` — view `v` already exists |
| `DROP VIEW t` where `t` is a base table | `InvalidValue` — `t` is not a view |
| `DROP VIEW v` where `v` does not exist | `InvalidValue` — view `v` not found |
| `DROP VIEW IF EXISTS v` where `v` does not exist | `Ok(Affected(0))` — no error |
| `SELECT * FROM v` where `v` does not exist | existing "table not found" error path |
| Circular view `v1 → v2 → v1` | `InvalidValue` — circular view reference |
| Column-name list length ≠ SELECT output width | `InvalidValue` — column count mismatch |
| `SHOW CREATE VIEW t` where `t` is a base table | `InvalidValue` — `t` is not a view |

## Edge cases

- [ ] View referencing another view (nested) — must expand recursively
- [ ] View with column-name override used in `SELECT *` — must project renamed columns
- [ ] `CREATE OR REPLACE VIEW` with different column count — allowed (replaces definition)
- [ ] `DROP VIEW v1, v2` where `v1` exists but `v2` does not (no IF EXISTS) — error on `v2`, `v1` not dropped
- [ ] View name same as CTE name in the same query — CTE shadows the view (CTE wins)
- [ ] View in JOIN's ON clause subquery — expansion handles nested FROM
- [ ] View referenced in DML source (`INSERT INTO t SELECT * FROM v`) — expansion handles
- [ ] `SHOW FULL TABLES` — view rows show `Table_type = 'VIEW'`
- [ ] `information_schema.TABLES` — view rows show `TABLE_TYPE = 'VIEW'`
- [ ] Schema-qualified `SELECT * FROM public.v` — resolves correctly
- [ ] View with `SELECT *` from a table that later has columns added — reflects new columns at query time (no schema cache)

## Catalog storage

Views reuse the existing `TableDef` on-disk format (v5+):

```
relation_kind = RelationKind::View   (new tag = 2)
defining_query = Some("<SQL text>")
root_page_id  = 0                    (no physical pages)
columns       = []                   (empty — schema derived at query time)
```

`RelationKind` byte tags:
```
0 = Table           (existing)
1 = MaterializedView (existing)
2 = View            (new)
```

Compatibility: older readers that don't know tag `2` will fail with a parse
error on the `RelationKind` byte — acceptable since views are a new feature.

## Dependencies

- Depends on: 13.1 (materialized views — catalog pattern to follow)
- Depends on: 21.2 (CTE expansion — `expand_ctes` pattern to mirror)
- Blocks: nothing immediate; 20.1 is self-contained

## Done criteria

- [ ] `CREATE VIEW`, `CREATE OR REPLACE VIEW`, `DROP VIEW`, `DROP VIEW IF EXISTS` parse and execute
- [ ] `SELECT * FROM view_name` expands transparently — no data stored
- [ ] Views in JOINs, subqueries, and CTEs work
- [ ] Circular view reference returns a clear error
- [ ] Column-name override works
- [ ] `SHOW CREATE VIEW` returns correct DDL
- [ ] `SHOW FULL TABLES` shows `VIEW` in `Table_type`
- [ ] `information_schema.TABLES` shows `TABLE_TYPE = 'VIEW'`
- [ ] `information_schema.VIEWS` returns at least: name, definition, check_option=NONE, is_updatable=NO
- [ ] `DROP VIEW` on a base table returns a clear error
- [ ] `DROP VIEW IF EXISTS` on missing name returns `Affected(0)`
- [ ] Integration tests in `crates/axiomdb-sql/tests/integration_views.rs`
- [ ] Wire smoke block `[20.1 regular views]` in `tools/wire-test.py`
- [ ] `cargo nextest run -p axiomdb-sql` passes
- [ ] `cargo nextest run --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes
- [ ] `cargo fmt --check` passes

## References

- Materialized view spec pattern: `crates/axiomdb-catalog/src/schema_table.rs`
- CTE expansion to mirror: `crates/axiomdb-sql/src/analyzer_stmt.rs::expand_ctes`
- PostgreSQL view implementation: `research/postgres/src/backend/commands/view.c`
- MariaDB view implementation: `research/mariadb-server/sql/sql_view.cc`
- Phase doc: `docs/fase-13.md` (materialized views section)
