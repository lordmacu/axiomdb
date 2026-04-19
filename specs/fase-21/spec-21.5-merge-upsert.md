# Spec: 21.5 — MERGE / UPSERT

Phase: 21 — Advanced SQL
Task: 21.5 MERGE / UPSERT
Status: completed (2026-04-19)

## Context

Phase 21 already has CTEs, recursive CTEs, RETURNING, LATERAL joins,
multi-table DML, CHECK constraints, and MySQL upsert variants
(`REPLACE INTO`, `INSERT IGNORE`, `INSERT ... ON DUPLICATE KEY UPDATE`).
This task closes the PostgreSQL/standard SQL upsert gap: PostgreSQL
`INSERT ... ON CONFLICT` and SQL-standard `MERGE`.

The implementation must reuse existing heap-table insert/update/delete
constraint paths so UNIQUE, CHECK, FK, partial indexes, GIN maintenance,
RETURNING, and affected-row reporting remain consistent with previous
Phase 21 work.

## Goal

Add PostgreSQL-compatible `INSERT ... ON CONFLICT` and SQL-standard
`MERGE` for heap tables, with clear `NotImplemented` errors for clustered
table conflict-resolution paths already deferred by existing MySQL upsert
support.

## Non-goals

- Native clustered-table `ON CONFLICT`, `REPLACE`, ODKU, or `MERGE`
  mutation support. Clustered conflict-resolution paths return a clear
  `DbError::NotImplemented`.
- PostgreSQL `ON CONSTRAINT name` conflict targets. Column-list targets
  are required for `DO UPDATE`; target-less `DO NOTHING` is accepted.
- PostgreSQL partial-index conflict target predicate syntax
  `ON CONFLICT (cols) WHERE pred`. Existing partial unique indexes still
  participate when the proposed row satisfies their stored predicate.
- `MERGE ... WHEN NOT MATCHED BY SOURCE`. This is useful for sync/delete
  workloads but can be a follow-up because it requires target anti-join
  bookkeeping.
- `MERGE RETURNING`. DML RETURNING interaction is required for
  `INSERT ... ON CONFLICT`; MERGE can return affected counts only in this
  subphase.
- Triggers. The trigger system is Phase 16 work.
- New on-disk formats.

## Behavior

### Public API

The SQL AST should have room for the new syntax without overloading the
MySQL ODKU fields:

```rust
pub struct InsertStmt {
    pub on_conflict: Option<OnConflictClause>,
    // existing fields unchanged
}

pub enum OnConflictAction {
    DoNothing,
    DoUpdate {
        assignments: Vec<Assignment>,
        where_clause: Option<Expr>,
    },
}

pub struct OnConflictClause {
    pub target_columns: Vec<String>,
    pub action: OnConflictAction,
}

pub enum Expr {
    ExcludedValue { col_idx: usize, name: String },
    // existing variants unchanged
}

pub struct MergeStmt {
    pub target: TableRef,
    pub source: FromClause,
    pub on: Expr,
    pub actions: Vec<MergeAction>,
}
```

Exact type names may differ if the implementation finds a better local
pattern, but the exposed behavior must keep PostgreSQL `EXCLUDED.col`
distinct from MySQL `VALUES(col)`.

### INSERT ... ON CONFLICT grammar

Supported forms:

```sql
INSERT INTO t [(cols...)] VALUES (...)
ON CONFLICT DO NOTHING;

INSERT INTO t [(cols...)] VALUES (...)
ON CONFLICT (key_col [, ...]) DO NOTHING;

INSERT INTO t [(cols...)] VALUES (...)
ON CONFLICT (key_col [, ...]) DO UPDATE
SET col = expr [, ...]
[WHERE expr]
[RETURNING select_item [, ...]];

INSERT INTO t [(cols...)] SELECT ...
ON CONFLICT (key_col [, ...]) DO UPDATE
SET col = EXCLUDED.col
RETURNING *;
```

Rules:

- `ON CONFLICT` is mutually exclusive with `REPLACE INTO` and
  `ON DUPLICATE KEY UPDATE`.
- `ON CONFLICT DO UPDATE` requires a non-empty column-list target.
- `ON CONFLICT DO NOTHING` may omit the target. If omitted, any PRIMARY
  KEY or UNIQUE conflict is skipped.
- Conflict target matching is case-insensitive by column name and must
  match a PRIMARY KEY or UNIQUE index column set.
- `EXCLUDED.col` in a `DO UPDATE` expression reads the proposed row after
  defaults, auto-increment, coercion, and text constraints.
- Unqualified target-column references in `DO UPDATE` expressions read
  the existing conflicting row.
- A `DO UPDATE WHERE` clause that evaluates to false leaves the existing
  row unchanged and contributes no affected row / RETURNING row.

### INSERT ... ON CONFLICT semantics

- No conflict: insert the row normally.
- `DO NOTHING` conflict: skip the row, no error, affected count unchanged.
- `DO UPDATE` conflict: update the conflicting existing row using the
  same constraint, FK, and index-maintenance pipeline as a normal UPDATE.
- Affected count is PostgreSQL-style: `1` for each inserted row and `1`
  for each row actually updated. No MySQL ODKU `2` count.
- `RETURNING` projects inserted rows and post-update rows. Skipped
  `DO NOTHING` rows and `DO UPDATE WHERE false` rows return nothing.
- Batch source rows are processed in source order.
- If two source rows would update the same existing row in one
  `ON CONFLICT DO UPDATE` statement, return an error instead of updating
  the same row twice.
- NULLs in UNIQUE keys follow the existing AxiomDB/MySQL behavior:
  any NULL key component does not conflict.

### MERGE grammar

Supported forms:

```sql
MERGE INTO target [AS t]
USING source [AS s]
ON condition
WHEN MATCHED [AND condition] THEN UPDATE SET col = expr [, ...]
WHEN MATCHED [AND condition] THEN DELETE
WHEN MATCHED [AND condition] THEN DO NOTHING
WHEN NOT MATCHED [AND condition] THEN INSERT [(cols...)] VALUES (expr [, ...])
WHEN NOT MATCHED [AND condition] THEN DO NOTHING;
```

Rules:

- `source` may be any existing materializable FROM item that the SELECT
  executor can expose as rows: table, subquery, VALUES source, JSON_TABLE,
  or JSONB SRF.
- Action clauses are evaluated in textual order. The first action whose
  match class and optional condition are true is applied.
- `WHEN MATCHED` actions operate on the target row and can reference
  source columns plus target columns.
- `WHEN NOT MATCHED` insert values can reference source columns.
- Multiple source rows matching the same target row return an error.
- If no action matches a candidate row, the row contributes no affected
  count.

### MERGE semantics

- Matched target row + UPDATE action: use the normal UPDATE constraint,
  FK, and index-maintenance pipeline.
- Matched target row + DELETE action: use the normal DELETE constraint,
  FK, and index-maintenance pipeline.
- Not matched + INSERT action: use the normal INSERT pipeline.
- Affected count is one per inserted, updated, or deleted target row.
- For this subphase, MERGE does not emit RETURNING rows.

## Error cases

| Input | Expected error | Message requirement |
|---|---|---|
| `ON CONFLICT DO UPDATE` without target columns | `DbError::ParseError` or semantic error | Mentions that `DO UPDATE` requires a conflict target |
| `ON CONFLICT (missing_col) ...` | `DbError::ColumnNotFound` | Missing target column name |
| `ON CONFLICT (cols)` with no matching unique/PK index | `DbError::InvalidValue` or semantic error | Mentions no matching unique constraint/index |
| `EXCLUDED.missing_col` | `DbError::ColumnNotFound` | Missing EXCLUDED column name |
| `ON CONFLICT` combined with ODKU or REPLACE | `DbError::ParseError` | Mentions mutually exclusive upsert forms |
| Same target row updated twice by one `DO UPDATE` statement | `DbError::InvalidValue` | Mentions cannot affect the same row twice |
| `MERGE` source row matches same target row twice | `DbError::InvalidValue` | Mentions multiple source rows matching one target |
| Clustered target table | `DbError::NotImplemented` | Mentions clustered MERGE/ON CONFLICT follow-up |
| `MERGE ... WHEN NOT MATCHED BY SOURCE` | `DbError::NotImplemented` | Mentions BY SOURCE follow-up |

## Edge cases

- [x] `ON CONFLICT DO NOTHING` without a target skips PK and UNIQUE
      conflicts.
- [x] `ON CONFLICT (col) DO NOTHING` skips only matching target conflicts.
- [x] `ON CONFLICT (col) DO UPDATE` can reference both target columns and
      `EXCLUDED.col`.
- [x] `ON CONFLICT ... DO UPDATE WHERE false` does not mutate, count, or
      return the row.
- [x] Multi-row `VALUES` source mixes inserted, skipped, and updated rows.
- [x] `INSERT ... SELECT ... ON CONFLICT` materializes the SELECT before
      applying per-row conflict handling.
- [x] `RETURNING *` over `ON CONFLICT` returns post-resolution rows only.
- [x] Partial UNIQUE indexes only conflict when their predicate is true
      for the proposed row.
- [x] NULL key components do not conflict.
- [x] MERGE supports matched UPDATE, matched DELETE, matched DO NOTHING,
      not-matched INSERT, and not-matched DO NOTHING.
- [x] MERGE action order is respected when multiple clauses share a match
      class.
- [x] MERGE rejects repeated updates/deletes of the same target row.

## On-disk format

No on-disk format changes.

## Performance budget

| Operation | Target | Max acceptable |
|---|---:|---:|
| Non-conflicting `INSERT ... ON CONFLICT` | Same order as plain INSERT path | <= 10% slower for single-row heap insert |
| Conflicting `ON CONFLICT DO UPDATE` by unique key | Same order as existing ODKU helper | <= 10% slower than ODKU conflict update |
| MERGE keyed by unique target column | No full target scan per source row when the ON condition is a simple target-key equality | O(source rows * log target rows) expected |

The main project performance budget in `CLAUDE.md` still applies. A
micro-benchmark is optional unless implementation changes shared insert,
update, delete, or index-maintenance hot paths.

## Dependencies

- Depends on: Phase 21.4b RETURNING executor projection.
- Depends on: MySQL ODKU helper for heap conflict lookup/update behavior.
- Depends on: REPLACE helper for unique-index conflict discovery patterns.

## Completion Notes

Implemented in heap storage with explicit `NotImplemented` guards for
clustered `ON CONFLICT` / `MERGE` mutation paths. `MERGE` sources reuse
existing SELECT/FROM materialization, including tables, subqueries, VALUES,
JSON_TABLE, and JSONB SRFs. The simple unique-key MERGE path uses indexed
lookup; other predicates retain a correctness-first nested evaluation path.

Verification completed:

- `cargo fmt --check`
- `cargo test -p axiomdb-sql`
- `cargo clippy -p axiomdb-sql -- -D warnings`
- `python3 tools/wire-test.py` (417/417)
- `cargo test --workspace`
- `cargo clippy --workspace -- -D warnings`
- Depends on: multi-table DML and LATERAL/source materialization paths for
  MERGE source rows.
- Blocks: closing Phase 21.5 and ORM-style PostgreSQL UPSERT parity.

## Open questions

Resolved by user approval (`continua`): `ON CONFLICT ON CONSTRAINT name`,
MERGE `WHEN NOT MATCHED BY SOURCE`, and clustered-table conflict-resolution
support remain out of scope for 21.5.

## Done criteria

- [ ] Parser accepts the supported `ON CONFLICT` forms and rejects
      mutually exclusive upsert combinations.
- [ ] Analyzer resolves conflict targets, `EXCLUDED.col`, assignments,
      `DO UPDATE WHERE`, MERGE source columns, and MERGE target columns.
- [ ] Heap executor implements `ON CONFLICT DO NOTHING`.
- [ ] Heap executor implements `ON CONFLICT DO UPDATE`.
- [ ] `ON CONFLICT ... RETURNING` returns inserted and post-update rows,
      excluding skipped rows.
- [ ] MERGE implements matched UPDATE, matched DELETE, matched DO NOTHING,
      not-matched INSERT, and not-matched DO NOTHING for heap targets.
- [ ] Clustered targets return clear `NotImplemented` errors.
- [ ] Integration tests cover every edge case above.
- [ ] Wire smoke includes at least one `ON CONFLICT DO NOTHING`, one
      `ON CONFLICT DO UPDATE RETURNING`, and one MERGE assertion.
- [ ] `cargo test -p axiomdb-sql --test integration_on_conflict` passes.
- [ ] `cargo test -p axiomdb-sql --test integration_merge` passes.
- [ ] `cargo test -p axiomdb-sql` passes.
- [ ] Closing protocol later runs workspace tests, clippy, fmt, docs,
      memory updates, and commit.

## References

- Design: `db.md` Phase 21 SQL avanzado.
- Existing RETURNING spec: `specs/fase-21/spec-21.4-returning.md`.
- Existing MySQL ODKU spec: `specs/fase-gap-audit/spec-insert-on-duplicate-key-update.md`.
- Existing ODKU helper: `crates/axiomdb-sql/src/executor/odku_helpers.rs`.
- DuckDB reference: `research/duckdb/src/execution/operator/persistent/physical_insert.cpp`.
- PostgreSQL grammar reference via DuckDB libpg_query mirror:
  `research/duckdb/third_party/libpg_query/src_backend_parser_gram.cpp`.
- MariaDB ODKU reference: `research/mariadb-server/sql/sql_insert.cc`.
