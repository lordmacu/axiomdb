# Plan: DELETE apply fast path (Phase 6.21)

## Files to create/modify

- `crates/axiomdb-sql/src/executor/shared.rs`
  - expose reusable column-mask helpers for non-SELECT executors
  - keep one shared way to collect referenced `Expr::Column` indices

- `crates/axiomdb-sql/src/table.rs`
  - add a masked batch row-read helper on top of `HeapChain::read_rows_batch`
  - keep output parallel to input `RecordId`s
  - preserve the current `Value::Null` contract for skipped columns

- `crates/axiomdb-sql/src/executor/delete.rs`
  - compute the DELETE-required column mask
  - use masked decode for both scan and index candidate paths
  - keep one materialized candidate set reused by FK enforcement, heap delete,
    and batched index delete

- `crates/axiomdb-sql/src/index_maintenance.rs`
  - keep `collect_delete_keys_by_index(...)` compatible with masked rows
  - add or tighten tests proving only needed key/predicate columns are required

- `crates/axiomdb-sql/src/fk_enforcement.rs`
  - verify parent-delete enforcement works correctly with masked rows as long as
    required parent key columns are present
  - only change this file if a helper extraction is needed

- `crates/axiomdb-sql/tests/integration_executor.rs`
  - add DELETE regressions for masked scan path, masked indexed path, and
    parent-FK no-WHERE path

- `crates/axiomdb-sql/benches/sqlite_comparison.rs`
  - keep as supporting evidence only if we need a narrow embedded DELETE check

- `benches/comparison/local_bench.py`
  - use `delete` and `delete_where` as the main performance checks

## Reviewed first

These AxiomDB files were reviewed before writing this plan:

- `db.md`
- `docs/progreso.md`
- `docs/fase-05.md`
- `docs/fase-06.md`
- `benches/comparison/local_bench.py`
- `crates/axiomdb-sql/src/executor/delete.rs`
- `crates/axiomdb-sql/src/executor/shared.rs`
- `crates/axiomdb-sql/src/table.rs`
- `crates/axiomdb-sql/src/index_maintenance.rs`
- `crates/axiomdb-sql/src/fk_enforcement.rs`
- `crates/axiomdb-storage/src/heap_chain.rs`
- `crates/axiomdb-index/src/tree.rs`
- `crates/axiomdb-index/tests/integration_btree.rs`
- `crates/axiomdb-sql/benches/sqlite_comparison.rs`
- `specs/fase-06/spec-6.3b-indexed-delete-where.md`
- `specs/fase-06/spec-6.20-update-apply-fast-path.md`
- `specs/fase-06/spec-6.21-delete-apply-fast-path.md`

These research sources were reviewed for the plan:

- `research/sqlite/src/delete.c`
- `research/sqlite/src/update.c`
- `research/duckdb/src/planner/binder/statement/bind_delete.cpp`
- `research/duckdb/src/execution/operator/persistent/physical_delete.cpp`
- `research/mariadb-server/sql/sql_delete.cc`
- `research/postgres/src/backend/executor/nodeModifyTable.c`
- `research/postgres/src/backend/access/heap/heapam.c`

## Research synthesis

### Core conclusion

The next useful DELETE optimization is not a new heap-delete primitive and not a
new B-Tree delete primitive.

Those parts already exist:

- `5.16` bulk-empty root rotation
- `5.19` batched `delete_many_in(...)`
- `6.3b` indexed candidate discovery

The remaining DELETE cost sits one layer higher:

1. candidate rows still carry full decoded row payloads when DELETE only needs a
   subset of columns
2. the no-WHERE path with parent FK references still materializes more row data
   than the FK check and index maintenance require
3. the executor should treat DELETE like SELECT already does for lazy decode:
   compute a required-column mask first, then decode only those columns

### What each reference system contributes

- SQLite:
  - validates precise OLD-column masking for DELETE consumers
  - validates two-pass candidate collection when deleting while scanning is unsafe
  - rejected: VDBE/RowSet mechanics as an implementation template

- DuckDB:
  - validates scan-time pass-through of only the columns needed by delete/index
    maintenance
  - validates sparse row materialization with NULL placeholders for absent columns
  - rejected: full vectorized sink/operator rewrite for this subphase

- MariaDB:
  - validates explicit "columns needed for delete" marking
  - validates keeping truncate/direct/bulk gating separate from row-by-row apply
  - rejected: handler-specific direct-delete contracts as an API model

- PostgreSQL:
  - validates separation between tuple identification and physical delete act
  - confirms the physical delete primitive should stay focused on storage/WAL/MVCC
  - rejected: heap-level MVCC/locking machinery as a direct performance model for
    this subphase

### Chosen technical direction

Use the existing lazy-decode model already present in `TableEngine::scan_table`
and extend it to the indexed batch-read path used by DELETE.

This keeps the physical mutation layer unchanged and localizes the work to:

- row materialization
- executor wiring
- regression coverage

## Closed decisions from research

These decisions are now fixed for `6.21`:

1. Keep the current two-phase DELETE shape.
   - Borrowed from SQLite/MariaDB.
   - Candidate discovery remains separate from physical delete apply.
   - We do **not** introduce "delete while scanning" for AxiomDB in this subphase.

2. Push the required-column set before materialization.
   - Borrowed from DuckDB's delete scan pass-through and SQLite's OLD-column mask.
   - The mask is computed before fetching candidate rows.
   - The executor carries sparse rows with `Value::Null` in skipped columns.

3. Leave heap and B-Tree delete primitives unchanged.
   - Confirmed by PostgreSQL's split between tuple identification and `heap_delete()`.
   - `delete_rows_batch(...)` and `delete_many_from_indexes(...)` remain the only
     physical mutation primitives used by `6.21`.

4. Optimize only single-table DELETE.
   - Matches current AxiomDB scope and avoids entangling multi-table syntax or
     RETURNING work from later phases.

5. Do not introduce a new row container.
   - The executor continues using `Vec<(RecordId, Vec<Value>)>`.
   - This keeps FK enforcement and index maintenance contracts stable.

## Algorithm / Data structure

### 1. Compute a DELETE-required column mask

For a resolved single-table DELETE:

```text
mask = [false; n_cols]

if where_clause exists:
    mark columns referenced by WHERE

for each parent-FK referencing this table:
    mark the parent column used by that FK

for each maintained index:
    mark every index key column
    if the index is partial:
        mark every column referenced by the compiled predicate
```

Properties:

- the mask is conservative: decoding extra columns is acceptable
- decoding fewer than this set is not acceptable
- if the mask is all `true`, fall back to the current unmasked path

### 2. Add `read_rows_batch_masked(...)`

New `TableEngine` helper:

```text
read_rows_batch_masked(storage, columns, rids, mask)
    raw_rows = HeapChain::read_rows_batch(storage, raw_rids)
    for each raw row:
        if dead -> None
        else -> decode_row_masked(bytes, col_types, mask)
```

Rules:

- preserve input order
- skipped columns become `Value::Null`
- if `mask` is all `true`, use the existing full decode helper
- do not change visibility semantics: this helper only decodes rows it is asked
  to decode; caller behavior stays unchanged

### 3. Refactor `collect_delete_candidates(...)`

Current shape:

```text
Scan:
    scan_table(..., None)
IndexLookup/Range:
    read_rows_batch(...)
    full WHERE recheck
```

New shape:

```text
build delete_mask once

Scan:
    scan_table(..., Some(mask)) or scan_table(..., None) if all-true

IndexLookup/Range:
    read_rows_batch_masked(..., mask)
    full WHERE recheck
```

The function still returns:

```text
Vec<(RecordId, Vec<Value>)>
```

so downstream FK enforcement and index maintenance keep their current contract.

### 4. Reuse the same candidate rows through the whole DELETE apply phase

The executor already stages:

```text
to_delete: Vec<(RecordId, Vec<Value>)>
```

Keep that shape and ensure it is the only materialization used for:

- parent-FK enforcement
- `delete_rows_batch(...)`
- `collect_delete_keys_by_index(...)`

No second decode pass is allowed in `execute_delete[_ctx]`.

### 5. Parent-FK no-WHERE path

For:

```sql
DELETE FROM parent;
```

when parent FKs exist:

- keep the existing control flow
- replace full-row scan decode with masked scan decode
- include the FK parent key columns in the mask

This keeps correctness while shrinking row materialization cost on large parent
table deletes that cannot use bulk-empty.

### 6. Exact implementation cut

The subphase will be implemented in this exact cut order:

#### Cut A — reusable masking infrastructure

- expose `build_column_mask(...)` from `executor/shared.rs`
- add a small DELETE-specific mask builder in `executor/delete.rs`
- no behavior change yet

Exit criterion:

- new unit coverage for DELETE mask composition passes

#### Cut B — masked batch row read

- add `TableEngine::read_rows_batch_masked(...)`
- use `decode_row_masked(...)`
- preserve caller RID order and `None` semantics for dead rows

Exit criterion:

- unit tests prove order preservation and skipped-column null-fill behavior

#### Cut C — indexed candidate path

- change `AccessMethod::IndexLookup` and `AccessMethod::IndexRange` branches in
  `collect_delete_candidates(...)` to use the DELETE mask
- keep the full `WHERE` recheck

Exit criterion:

- indexed DELETE regressions pass unchanged functionally

#### Cut D — scan path + parent-FK no-WHERE path

- change scan-based candidate discovery to pass `Some(mask)` when possible
- use the same path for no-WHERE deletes blocked from bulk-empty by parent FKs

Exit criterion:

- parent-FK no-WHERE regressions pass

#### Cut E — performance validation and tracker cleanup

- run `delete` / `delete_where` benchmarks
- document the measured gain
- update stale Phase 6 warnings to reflect what was actually fixed vs. what remains

Exit criterion:

- benchmark delta recorded and remaining gap explicitly documented

## Implementation phases

1. Extract or expose reusable mask-building helpers from `executor/shared.rs`.
2. Add `TableEngine::read_rows_batch_masked(...)` with tests for ordering and
   skipped-column null-fill behavior.
3. Build DELETE-specific mask computation in `executor/delete.rs` from WHERE,
   FK parent refs, index key columns, and partial-index predicate refs.
4. Switch scan and index candidate paths in `collect_delete_candidates(...)` to
   use the mask-aware decode path.
5. Add regressions for:
   - indexed DELETE path with masked decode
   - scan DELETE path with masked decode
   - no-WHERE parent-FK DELETE path
   - partial-index delete correctness under masked rows
6. Run targeted benches and compare against the pre-6.21 DELETE baseline.

## Acceptance mapping

- Spec criteria 1-4 map to Cuts A-C.
- Spec criteria 5-6 map to Cut D.
- Spec criteria 7-10 are regression gates during Cuts C-D.
- Spec criterion 11 maps to Cut E.

## Tests to write

- unit:
  - `read_rows_batch_masked(...)` preserves RID order
  - `read_rows_batch_masked(...)` returns `Value::Null` on skipped columns
  - DELETE mask builder includes:
    - WHERE columns
    - FK parent columns
    - index key columns
    - partial-index predicate columns

- integration:
  - `DELETE WHERE id >= ...` still deletes the right rows via the indexed path
  - `DELETE FROM parent` with child FK references still enforces FK behavior
    while using masked decode
  - partial-index and non-unique index delete maintenance still remove the
    correct keys under masked candidate rows
  - non-binary collation fallback behavior remains unchanged

- bench:
  - `python3 benches/comparison/local_bench.py --scenario delete --rows 5000 --table`
  - `python3 benches/comparison/local_bench.py --scenario delete_where --rows 5000 --table`
  - optionally repeat at `1000` rows for quick iteration

## Anti-patterns to avoid

- Do not redesign heap delete or B-Tree delete in this subphase.
- Do not create a second row-materialization format just for DELETE.
- Do not special-case benchmark schemas in executor logic.
- Do not make FK enforcement depend on fully decoded rows if the required key
  columns are already present in the mask.
- Do not silently keep the stale `insert_leaf` warning as if it were this
  subphase's target; document that separately at close-out.

## Risks

- Risk: missing a required column in the DELETE mask would cause incorrect WHERE,
  FK, or index behavior.
  Mitigation: conservative union of all consumers plus dedicated mask-builder
  tests.

- Risk: partial-index predicates may reference columns outside the index key.
  Mitigation: include predicate column references explicitly in the mask.

- Risk: masked rows use `Value::Null` for skipped columns, which could leak into
  later code accidentally.
  Mitigation: only hand masked rows to consumers whose required columns are
  guaranteed to be marked in the mask; keep coverage around FK and index helpers.

- Risk: scan-path decode may not speed up enough if most DELETE statements still
  need almost every column.
  Mitigation: benchmark both `delete` and `delete_where`; if the mask is
  all-true, the executor should fall back to the existing full-decode path with
  no extra overhead.
