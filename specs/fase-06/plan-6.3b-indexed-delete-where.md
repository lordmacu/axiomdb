# Plan: Indexed DELETE WHERE fast path (Phase 6.3b)

## Files to create / modify

### Modify
- `crates/axiomdb-sql/src/planner.rs` — add DELETE-specific candidate planner
  that can use PRIMARY KEY indexes and skips the SELECT-oriented stats cost
  gate
- `crates/axiomdb-sql/src/executor.rs` — replace full-heap candidate discovery
  in DELETE paths with access-method-driven RID discovery + row fetch + full
  `WHERE` recheck
- `crates/axiomdb-sql/src/table.rs` — add batch row fetch helpers by
  `RecordId`, including masked decode support for mutation paths
- `crates/axiomdb-storage/src/heap_chain.rs` — add visible-row batch read
  primitive grouped by page to avoid repeated heap page reads
- `crates/axiomdb-sql/tests/integration_executor.rs` — add DELETE WHERE access
  path regressions and correctness tests
- `crates/axiomdb-sql/tests/integration_fk.rs` — add indexed DELETE WHERE
  coverage when parent FKs exist
- `benches/comparison/local_bench.py` or the comparison bench entrypoint it
  invokes — add explicit before/after coverage for `delete_where`

## Reviewed first

These AxiomDB files were reviewed before writing this plan:
- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-sql/src/executor.rs`
- `crates/axiomdb-sql/src/planner.rs`
- `crates/axiomdb-sql/src/table.rs`
- `crates/axiomdb-storage/src/heap_chain.rs`
- `crates/axiomdb-sql/src/index_maintenance.rs`
- `crates/axiomdb-sql/src/fk_enforcement.rs`
- `benches/comparison/local_bench.py`

These research files were reviewed for guidance:
- `research/sqlite/src/delete.c`
- `research/sqlite/src/update.c`
- `research/postgres/src/backend/executor/README`
- `research/postgres/src/backend/executor/nodeModifyTable.c`
- `research/postgres/src/backend/access/table/tableam.c`
- `research/mariadb-server/sql/sql_delete.cc`
- `research/duckdb/src/planner/binder/statement/bind_delete.cpp`
- `research/duckdb/src/execution/operator/persistent/physical_delete.cpp`
- `research/oceanbase/src/sql/das/ob_das_delete_op.cpp`
- `research/oceanbase/src/sql/engine/dml/ob_table_delete_op.cpp`
- `research/datafusion/datafusion/core/src/physical_planner.rs`
- `research/datafusion/datafusion/core/tests/custom_sources_cases/dml_planning.rs`

## Research synthesis

### Why the change belongs here

The main bottleneck is not the physical B+Tree delete path anymore. After Phase
5.17, AxiomDB already avoids part of the leaf/internal churn on writes. The
remaining problem is that DELETE still discovers rows with a full heap scan,
even when the predicate is indexable.

### Borrow / reject / adapt

#### SQLite — `research/sqlite/src/delete.c`, `research/sqlite/src/update.c`
- Borrow: collect row identities first, mutate later.
- Reject: VM opcode orchestration.
- Adapt: `Vec<RecordId>` materialized before delete.

#### PostgreSQL — `research/postgres/src/backend/executor/README`,
#### `research/postgres/src/backend/executor/nodeModifyTable.c`,
#### `research/postgres/src/backend/access/table/tableam.c`
- Borrow: row identity is produced by the access path, not rediscovered by the
  delete loop.
- Reject: full `ModifyTable` executor tree and table-AM abstraction.
- Adapt: a mutation-specific planner entrypoint that produces an AxiomDB access
  method for DELETE.

#### MariaDB — `research/mariadb-server/sql/sql_delete.cc`
- Borrow: keep full-table delete fast path and planned `DELETE ... WHERE` path
  separate.
- Reject: server/handler layering.
- Adapt: preserve Phase 5.16 bulk-empty path unchanged and add a new indexed
  candidate path only for WHERE deletes.

#### DuckDB — `research/duckdb/src/planner/binder/statement/bind_delete.cpp`,
#### `research/duckdb/src/execution/operator/persistent/physical_delete.cpp`
- Borrow: plan which columns are needed in the mutation pipeline.
- Reject: vectorized delete operator / delete-index design.
- Adapt: masked row decode for only the columns needed by `WHERE`, FK checks,
  maintained indexes, and partial-index predicates.

#### OceanBase — `research/oceanbase/src/sql/das/ob_das_delete_op.cpp`,
#### `research/oceanbase/src/sql/engine/dml/ob_table_delete_op.cpp`
- Borrow: stage candidate production separately from row deletion.
- Reject: DAS/distributed execution.
- Adapt: local RID materialization feeding the current AxiomDB delete path.

#### DataFusion — `research/datafusion/datafusion/core/src/physical_planner.rs`,
#### `research/datafusion/datafusion/core/tests/custom_sources_cases/dml_planning.rs`
- Borrow: DML planning decisions must be explicit and testable.
- Reject: provider interface as an implementation template.
- Adapt: dedicated planner/executor tests asserting when DELETE uses an indexed
  candidate path.

## Algorithm / Data structures

### 1. DELETE-specific candidate planner

Do not call `plan_select()` directly for DELETE.

Add a new planner entrypoint:

```rust
pub struct DeleteCandidatePlan {
    pub access: AccessMethod,
    pub recheck_full_where: bool,
}

pub fn plan_delete_candidates(
    where_clause: &Expr,
    indexes: &[IndexDef],
    columns: &[ColumnDef],
) -> DeleteCandidatePlan
```

Ctx path variant:

```rust
pub fn plan_delete_candidates_ctx(
    where_clause: &Expr,
    indexes: &[IndexDef],
    columns: &[ColumnDef],
    session_collation: SessionCollation,
    stats: Option<&StatsCatalog>,
) -> DeleteCandidatePlan
```

Planner rules:
1. If there is no `WHERE`, this planner is not used. Phase 5.16 bulk-empty path
   remains authoritative.
2. Split the `WHERE` into AND-conjuncts.
3. Try candidate extraction in this priority order:
   - composite equality on eligible index
   - PK / UNIQUE equality
   - PK / UNIQUE range
   - secondary equality
   - secondary range
4. Allow PRIMARY KEY indexes here even though current SELECT-oriented helpers
   exclude them.
5. Reuse the existing partial-index implication guard.
6. Reuse the existing session-collation guard from the ctx planner path.
7. Do not apply `stats_cost_gate()`. For DELETE, avoiding a full-heap candidate
   scan is the primary goal even when many rows match.
8. If no usable index remains, return `AccessMethod::Scan`.

For this subphase, `recheck_full_where` is always `true` for indexed candidate
plans. The index path only narrows candidates; the full original predicate is
always re-evaluated before deletion.

### 2. Candidate discovery helper in executor

Unify candidate discovery for `execute_delete_ctx()` and `execute_delete()`
behind a shared helper:

```rust
fn collect_delete_candidates(...)
    -> Result<Vec<(RecordId, Vec<Value>)>, DbError>
```

Behavior:
1. Ask the DELETE-specific planner for a candidate plan.
2. If the planner returns `AccessMethod::Scan`, keep current full-scan behavior.
3. If it returns `IndexLookup`:
   - perform B-Tree lookup
   - materialize all returned `RecordId`s
4. If it returns `IndexRange`:
   - perform B-Tree range scan
   - materialize all returned `RecordId`s
5. Never mutate heap or index state while iterating the source index.
6. Fetch candidate rows by RID in a second phase.
7. Re-evaluate the full original `WHERE` predicate on fetched row values.
8. Return only surviving `(RecordId, row_values)` pairs.

### 3. Batch heap row fetch by RID

Add a batch read primitive instead of repeated `TableEngine::read_row()` calls:

```rust
HeapChain::read_rows_visible_batch(storage, rids, snapshot)
    -> Result<Vec<(RecordId, Vec<u8>)>, DbError>

TableEngine::read_rows_batch_masked(storage, columns, rids, snapshot, column_mask)
    -> Result<Vec<(RecordId, Vec<Value>)>, DbError>
```

Requirements:
- sort/group incoming RIDs by `page_id`
- read each heap page at most once
- skip rows not visible to the statement snapshot
- preserve a documented stable order when returning rows
- support masked decode for:
  - all columns needed for full `WHERE` recheck
  - all columns referenced by maintained indexes
  - all columns referenced by partial-index predicates
  - all parent-key columns needed by FK delete enforcement

Do not fetch entire table rows blindly when a smaller decode mask is sufficient.

### 4. DELETE execution flow

For both ctx and non-ctx paths:

```text
if no WHERE and no parent-FK references:
    keep Phase 5.16 bulk-empty path
else:
    to_delete = collect_delete_candidates(...)
    if has_fk_references:
        enforce_fk_on_parent_delete(to_delete, ...)
    rids = to_delete.rids()
    count = delete_rows_batch(...)
    maintain indexes per row using delete_from_indexes(...)
    update changed roots in catalog and in-memory snapshot
    invalidate ctx cache if roots changed (ctx path)
    return QueryResult::Affected { count, last_insert_id: None }
```

Invariants:
- FK enforcement still happens before heap delete.
- Index maintenance still uses full row values from `to_delete`.
- Root-page updates remain synchronized exactly as they are today.
- Bloom dirtying behavior remains unchanged.
- No bulk index-delete optimization is introduced in this subphase.

## Implementation phases

1. Add `DeleteCandidatePlan` and DELETE-specific planner helpers in
   `planner.rs`.
2. Add heap batch fetch primitive in `heap_chain.rs`.
3. Add `TableEngine` batch decode helpers for visible rows by RID with column
   mask.
4. Add shared executor helper for indexed candidate discovery + full `WHERE`
   recheck.
5. Wire the helper into `execute_delete_ctx()`.
6. Wire the helper into `execute_delete()`.
7. Keep the existing Phase 5.16 no-WHERE fast path untouched.
8. Add correctness tests first, then benchmark/regression coverage.

## Tests to write

- unit:
  - PK equality/range candidate planning
  - partial-index implication in DELETE planner
  - session-collation rejection for text indexes
  - batch RID fetch grouped by page and filtered by snapshot visibility
- integration:
  - `DELETE FROM t WHERE id = 5` uses indexed candidate path with PK
  - `DELETE FROM t WHERE id > 5000` uses indexed range path
  - DELETE with UNIQUE secondary index still maintains all indexes correctly
  - DELETE with parent FK references still enforces RESTRICT/CASCADE/SET NULL
    before heap mutation
  - DELETE on partial-index predicate remains correct
  - non-sargable DELETE still falls back to scan
- bench:
  - `delete_where` in `benches/comparison/local_bench.py` on
    `bench_users(id PRIMARY KEY, ...)`
  - compare before/after ops/s and, if practical, heap page-read counts

## Anti-patterns to avoid

- Do not call `plan_select()` directly for DELETE.
  It excludes PK today and applies a SELECT-oriented cost gate.
- Do not delete rows while iterating the same B-Tree range.
  Materialize `RecordId`s first to avoid structural mutation hazards.
- Do not fetch candidate rows with N repeated `read_row()` calls if they cluster
  on the same heap pages.
- Do not weaken FK enforcement or partial-index correctness for speed.
- Do not introduce bulk index delete in this subphase; keep correctness anchored
  in `delete_from_indexes()`.

## Risks

| Risk | Mitigation |
|---|---|
| Planner chooses an index path invalid under non-binary text collation | Reuse the existing session-collation guard logic for DELETE planning |
| Fetching candidate rows one-by-one just moves the bottleneck from scan to random heap reads | Add grouped page-level batch fetch in `HeapChain` and `TableEngine` |
| Using only one conjunct for candidate discovery misses rows or over-deletes | Indexed predicate only narrows candidates; full `WHERE` is always rechecked |
| FK parent enforcement needs columns not present in the first decode mask | Build the mask from `WHERE` + maintained-index columns + partial-index predicates + parent FK key columns |
| Root-page changes during per-row index deletes break later rows | Preserve the current root refresh contract after each row / affected index mutation |

## Assumptions

- Phase 6 still operates under the current single-writer assumptions; no new
  concurrent index-scan/delete correctness model is introduced here.
- The benchmark target is the existing `delete_where` case on
  `bench_users(id PRIMARY KEY, ...)`.
- If profiling after this change still shows index maintenance dominating, that
  becomes a follow-up subphase rather than expanding scope here.
