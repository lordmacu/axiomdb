# Plan: Indexed UPDATE candidate fast path (Phase 6.17)

## Files to create / modify

- `crates/axiomdb-sql/src/planner.rs`
  - add `plan_update_candidates(...)` and `plan_update_candidates_ctx(...)`
  - reuse the same indexability rules as `6.3b` where possible

- `crates/axiomdb-sql/src/executor/update.rs`
  - replace initial `scan_table(...)` full discovery with access-method-driven
    RID collection + row fetch + full `WHERE` recheck
  - keep `5.20` stable-RID/fallback write logic intact after candidate staging

- `crates/axiomdb-sql/src/executor/delete.rs`
  - optionally extract shared candidate-collection helpers if that reduces
    duplication cleanly without mixing semantics

- `crates/axiomdb-sql/tests/integration_executor.rs`
  - add indexed UPDATE regressions for PK range/equality and fallback scan path

- `benches/comparison/local_bench.py`
  - benchmark target is `update_range`

## Reviewed first

These AxiomDB files were reviewed before writing this plan:

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-sql/src/executor/update.rs`
- `crates/axiomdb-sql/src/executor/delete.rs`
- `crates/axiomdb-sql/src/planner.rs`
- `crates/axiomdb-sql/src/table.rs`
- `crates/axiomdb-sql/src/index_maintenance.rs`
- `crates/axiomdb-sql/src/fk_enforcement.rs`
- `benches/comparison/local_bench.py`
- `specs/fase-05/spec-5.20-stable-rid-update-fast-path.md`
- `specs/fase-06/spec-6.3b-indexed-delete-where.md`

These research files were reviewed for guidance:

- `research/sqlite/src/where.c`
- `research/sqlite/src/update.c`
- `research/postgres/src/backend/executor/nodeModifyTable.c`
- `research/mariadb-server/sql/sql_update.cc`
- `research/duckdb/src/execution/operator/persistent/physical_update.cpp`
- `research/oceanbase/src/sql/engine/dml/ob_table_update_op.cpp`
- `research/datafusion/datafusion/core/tests/custom_sources_cases/dml_planning.rs`

## Research synthesis

### Why the change belongs here

`UPDATE` already has a much better physical write path after `5.20`, but it
still pays O(n) row discovery for indexed predicates. The performance debt in
`update_range` is therefore architectural, not a bug in stable-RID rewrite.

### Borrow / reject / adapt

#### SQLite / PostgreSQL
- Borrow: use the access path to produce row identity first.
- Reject: VM one-pass and full `ModifyTable` machinery.
- Adapt: local RID vector before mutation.

#### MariaDB / DuckDB / OceanBase
- Borrow: keep row discovery separate from changed-column/update application.
- Reject: engine-specific bulk-update hooks and vectorized sinks.
- Adapt: preserve AxiomDB's current update write path and change only
  candidate discovery.

## Algorithm / data structure

### 1. UPDATE candidate planner

Add:

```rust
pub fn plan_update_candidates(
    where_clause: &Expr,
    indexes: &[IndexDef],
    columns: &[ColumnDef],
) -> AccessMethod

pub fn plan_update_candidates_ctx(
    where_clause: &Expr,
    indexes: &[IndexDef],
    columns: &[ColumnDef],
    collation: SessionCollation,
) -> AccessMethod
```

Planner rules:
- no stats gate for UPDATE candidate discovery
- no `IndexOnlyScan`
- PRIMARY KEY, UNIQUE, secondary, and eligible partial indexes allowed
- same non-binary text-collation guard as DELETE

### 2. Candidate collection

Add a helper in `update.rs` or shared executor code:

```rust
fn collect_update_candidates(...)
    -> Result<Vec<(RecordId, Vec<Value>)>, DbError>
```

Behavior:
1. planner chooses `Scan`, `IndexLookup`, or `IndexRange`
2. materialize all candidate RIDs from the index path before mutation
3. fetch rows by RID
4. re-evaluate the full original `WHERE`
5. return surviving `(rid, row_values)`

Important invariant:
- never mutate the same index while iterating it for candidate discovery

### 3. Integration with existing update path

After `collect_update_candidates(...)`, keep the existing `5.20` flow:

```text
1. build new_values from old row values
2. compute changed_cols / index impact
3. run FK child validation
4. run FK parent enforcement
5. route rows through stable-RID vs fallback paths
6. apply index maintenance exactly as today
```

## Implementation phases

1. Add UPDATE-specific planner entrypoints in `planner.rs`.
2. Implement candidate collection helper.
3. Replace initial full scan in ctx UPDATE path.
4. Replace initial full scan in non-ctx UPDATE path.
5. Add correctness regressions for indexed PK/secondary UPDATE.
6. Benchmark `update_range`.

## Tests to write

- unit:
  - PK equality/range emits indexed UPDATE access
  - partial-index implication works for UPDATE planner
  - non-binary text collation rejects text-index UPDATE candidate plans
- integration:
  - `UPDATE t SET ... WHERE id = 5`
  - `UPDATE t SET ... WHERE id BETWEEN ...`
  - secondary-index UPDATE remains correct
  - non-sargable UPDATE still falls back to scan
- bench:
  - `python3 benches/comparison/local_bench.py --scenario update_range --rows 5000`

## Anti-patterns to avoid

- Do not re-open `5.20` and mix write-path redesign into this subphase.
- Do not mutate an index while using it as the candidate source.
- Do not skip full `WHERE` recheck just because one conjunct was indexable.
- Do not introduce a shared mega-helper that obscures the semantic differences
  between DELETE and UPDATE.

## Risks

- Risk: planner and executor diverge from DELETE semantics.
  Mitigation: mirror `6.3b` structure closely where the semantics are the same.

- Risk: updating the same indexed column used by the predicate causes subtle
  self-interference.
  Mitigation: materialize all candidate RIDs before any heap/index mutation.
