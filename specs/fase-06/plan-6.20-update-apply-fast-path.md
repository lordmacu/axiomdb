# Plan: UPDATE apply fast path (Phase 6.20)

## Files to create/modify

- `crates/axiomdb-storage/src/heap_chain.rs`
  - add a batched row-read helper that groups `RecordId`s by `page_id`
  - preserve caller-visible RID order while reading each affected page once

- `crates/axiomdb-sql/src/table.rs`
  - expose `read_rows_batch(...)` on top of the new heap helper
  - update the stable-RID batch path to accumulate `UpdateInPlace` images and
    log them through one WAL batch append

- `crates/axiomdb-wal/src/txn.rs`
  - add `record_update_in_place_batch(...)`
  - preserve the current `UpdateInPlace` entry format, undo ops, and recovery
    semantics while replacing per-row append calls with `reserve_lsns(...) +
    write_batch(...)`

- `crates/axiomdb-sql/src/executor/delete.rs`
  - upgrade `collect_delete_candidates(...)` so `IndexLookup` / `IndexRange`
    use batched row materialization
  - keep DELETE and UPDATE sharing the same candidate-fetch logic

- `crates/axiomdb-sql/src/executor/update.rs`
  - stage UPDATE rows into `noop`, `physically_changed`, and final
    `(old_rid, old_values, new_rid, new_values)` apply pairs
  - add a statement-level "index maintenance can be skipped entirely" bailout
  - keep current FK checks and result semantics unchanged

- `crates/axiomdb-sql/src/index_maintenance.rs`
  - add a grouped single-index insert helper for UPDATE
  - add or expose exact per-index dependency checks so UPDATE can avoid opening
    B-Tree work for unaffected indexes

- `crates/axiomdb-sql/tests/integration_executor.rs`
  - add regressions for no-op UPDATE, PK-only range UPDATE, indexed stable-RID
    UPDATE, and fallback UPDATE

- `crates/axiomdb-storage/tests/` or existing storage integration tests
  - add batch-read ordering and same-page read-count tests if missing

- `benches/comparison/local_bench.py`
  - keep `update_range` as the primary benchmark target
  - optionally add an indexed variant if we need a second measurement for the
    fallback/index-maintenance case

## Reviewed first

These AxiomDB files were reviewed before writing this plan:

- `db.md`
- `docs/progreso.md`
- `docs/fase-06.md`
- `benches/comparison/local_bench.py`
- `crates/axiomdb-sql/src/executor/update.rs`
- `crates/axiomdb-sql/src/executor/delete.rs`
- `crates/axiomdb-sql/src/table.rs`
- `crates/axiomdb-sql/src/index_maintenance.rs`
- `crates/axiomdb-storage/src/heap_chain.rs`
- `crates/axiomdb-wal/src/txn.rs`
- `specs/fase-05/spec-5.20-stable-rid-update-fast-path.md`
- `specs/fase-06/spec-6.17-indexed-update-candidates.md`
- `specs/fase-06/spec-6.18-indexed-multi-row-insert-batch.md`

These research sources were reviewed for the plan:

- `research/mariadb-server/sql/sql_update.cc`
- `research/mariadb-server/storage/innobase/row/row0upd.cc`
- `research/postgres/src/backend/access/heap/heapam.c`
- `research/postgres/src/backend/executor/nodeModifyTable.c`
- `research/sqlite/src/update.c`
- `research/duckdb/src/execution/operator/persistent/physical_update.cpp`
- `research/oceanbase/src/sql/engine/dml/ob_table_update_op.cpp`
- MySQL 8.4 docs on `InnoDB multi-versioning` and `read-ahead`

## Research synthesis

### Why this subphase is broader than "batch the index inserts"

The benchmark that is currently failing is PK-only by default:

- `PRIMARY KEY (id)` always exists
- no secondary indexes are created unless `--indexes ...` is passed
- the UPDATE changes `score`, not `id`

So if stable-RID succeeds, the PK B-Tree should not be touched at all.

That means the remaining default-path gap must be attacked in this order:

1. candidate row materialization
2. no-op filtering
3. stable-RID WAL append cost
4. grouped index work for the cases where index maintenance is truly required

Batching only UPDATE index inserts would leave the common benchmark mostly unchanged.

### Borrow / reject / adapt

- MariaDB / MySQL:
  - borrow no-op skip and clustered-vs-secondary separation
  - adapt into statement-local staging, not handler callbacks
- PostgreSQL:
  - borrow modified-attrs / HOT intuition
  - adapt into stable-RID + exact index dependency checks, not full HOT chains
- SQLite:
  - borrow exact "does this index or predicate depend on changed columns?"
  - adapt into Rust masks/helpers
- DuckDB:
  - borrow coarse "index_update required?" mode switch
  - adapt into a statement-level bailout before index maintenance
- OceanBase:
  - borrow primary/index write-count parity as an invariant
  - adapt into grouped helper checks and tests

## Algorithm / data structure

### 1. Batched candidate row materialization

Closed decision:

- extend the shared candidate-fetch path used by DELETE and UPDATE
- keep planner output untouched
- only change how candidate RIDs become decoded rows

Shape:

```text
if access is Scan:
    existing scan_table path
else if access is IndexLookup / IndexRange:
    candidate_rids = BTree lookup/range
    rows = TableEngine::read_rows_batch(storage, schema_cols, candidate_rids)
    for each row:
        recheck full WHERE
```

`read_rows_batch(...)` must:

- sort/group by `(page_id, slot_id)` for I/O locality
- read each page once
- decode only live/visible rows
- restore the original RID order for the caller

### 2. UPDATE staging and no-op partition

In `execute_update[_ctx](...)`:

```text
matched_count = candidate_rows.len()
for each candidate row:
    evaluate new_values
    if old_values == new_values:
        noop_rows += 1
        continue
    changed_rows.push((old_rid, old_values, new_values))
```

Important:

- `matched_count` stays as today for `QueryResult::Affected`
- FK validation and FK parent checks still see the rows that can actually change
- no-op rows do not enter heap or index apply

### 3. Stable-RID WAL batch logging

The existing heap rewrite flow already groups physical page writes:

- `HeapChain::rewrite_batch_same_slot(...)` reads each page once
- rewrites all fitting rows in memory
- writes each dirty page once

The remaining avoidable overhead is WAL append:

```text
today:
    for each stable row:
        build tuple image
        record_update_in_place(...)

new:
    collect StableWalImage { key, old_tuple_image, new_tuple_image, page_id, slot_id }
    txn.record_update_in_place_batch(table_id, &images)
```

`record_update_in_place_batch(...)` should:

1. reserve `n` LSNs once
2. serialize `n` normal `UpdateInPlace` entries into one scratch buffer
3. call `write_batch(...)` once
4. enqueue the same `UndoUpdateInPlace` ops as today

This keeps WAL/recovery semantics unchanged while eliminating one hot-path
`write_all(...)` call per updated row.

### 4. Statement-level index bailout

Before calling UPDATE index maintenance, compute two coarse facts:

```text
index_relevant_columns = union(index key cols, partial-predicate referenced cols)
statement_might_affect_any_index = any SET/evaluated changed col overlaps that union
all_rows_kept_rid = all(old_rid == new_rid)
```

If:

- `statement_might_affect_any_index == false`, and
- `all_rows_kept_rid == true`,

then skip `apply_update_index_maintenance(...)` entirely.

This is the common `update_range` benchmark case.

### 5. Grouped UPDATE index maintenance

For the remaining cases, keep the current per-index outer loop but remove the
per-row insert half:

```text
for each index:
    collect delete_keys
    collect insert_rows
    delete_many_from_single_index(...) once
    insert_many_into_single_index(...) once
    persist final root once
```

`insert_many_into_single_index(...)` should mirror the grouped behavior already
present in `batch_insert_into_indexes(...)`:

- accumulate root changes through the batch
- perform uniqueness checks with current semantics
- update bloom state
- return one final root change, not one per row

This helper must handle both:

- stable-RID rows where a key or predicate membership changed
- fallback rows where RID changed and the index is therefore affected

### 6. Invariants

The implementation must preserve these invariants:

- candidate order and `WHERE` truth are unchanged
- `matched_count` is unchanged even when no-op rows skip physical work
- every stable-RID rewrite still has one `UndoUpdateInPlace`
- heap/index write counts stay aligned for every affected index
- root-page updates remain synchronized between in-memory `IndexDef` and catalog

## Implementation phases

1. Add storage/table batch row-read helper and cover ordering/read-count behavior.
2. Route `IndexLookup` / `IndexRange` candidate materialization through the new helper.
3. Add no-op partitioning in UPDATE executor while preserving affected-row semantics.
4. Add `record_update_in_place_batch(...)` and switch stable-RID logging to it.
5. Add grouped single-index insert helper and remove per-row UPDATE insert/root writes.
6. Add integration regressions and re-run `update_range`.

## Tests to write

- unit:
  - batch row-read preserves RID order while reading same-page rows once
  - `record_update_in_place_batch(...)` produces parseable `UpdateInPlace` WAL entries
  - grouped single-index UPDATE insert preserves root and uniqueness behavior

- integration:
  - PK-only `UPDATE ... WHERE id BETWEEN ...` still returns correct rows
  - no-op UPDATE performs no visible mutation but keeps current affected-row count
  - stable-RID UPDATE on indexed column keeps index lookups correct
  - fallback UPDATE that grows a row still keeps indexes and heap consistent

- bench:
  - `python3 benches/comparison/local_bench.py --scenario update_range --rows 50000 --range-rows 5000 --table`
  - optional stress: `--indexes score` to validate the grouped index-maintenance case

## Anti-patterns to avoid

- Do not reopen planner work that `6.17` already solved.
- Do not introduce a new WAL entry type in this subphase.
- Do not silently change affected-row semantics from "matched" to "changed".
- Do not duplicate `6.18` grouped insert logic in a second incompatible helper.
- Do not skip partial-index maintenance using only assignment targets; use exact
  dependency checks.

## Risks

- Risk: batch row-read changes candidate ordering.
  Mitigation: store original positions and restore them before returning.

- Risk: no-op skip changes FK or affected-row semantics.
  Mitigation: keep matched-count accounting unchanged and run FK checks only on
  physically changed rows.

- Risk: batched WAL append breaks rollback or recovery ordering.
  Mitigation: keep the existing `UpdateInPlace` entry format and undo ops, only
  change the serialization/append mechanism.

- Risk: grouped UPDATE index inserts miss a uniqueness conflict or partial-index
  membership transition.
  Mitigation: reuse existing key-encoding/predicate helpers and add explicit
  indexed UPDATE regressions.
